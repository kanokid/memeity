//! memeity: milestone 1 (hotkey pane + fuzzy filename search + copy to clipboard),
//! backed by two-way sync with one Google Drive folder.

mod clipboard;
mod config;
mod dupes;
mod fuzzytext;
mod google;
mod import;
mod index;
mod insert;
mod library;
mod ocr;
mod settings;
mod sidecar;
mod sync;
mod textindex;
mod thumbs;
mod updates;
mod watcher;

use config::{Config, DriveFolder};
use google::Google;
use library::{Library, SearchResult};
use notify::RecommendedWatcher;
use nucleo_matcher::Matcher;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, RwLock};
use sync::SyncStatus;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewWindow, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

const PANE: &str = "pane";
/// Log file name (without extension) in the OS log folder.
const LOG_FILE: &str = "memeity";
const CLIENT_FILE: &str = "google-client.json";
/// Fixed content size of the window (Raycast's default), matching tauri.conf.json.
const PANE_WIDTH: f64 = 750.0;
const PANE_HEIGHT: f64 = 475.0;

pub struct AppState {
    pub library: RwLock<Library>,
    pub google: Google,
    pub sync_tx: tokio::sync::mpsc::UnboundedSender<sync::SyncKind>,
    /// Serializes sync passes and deletes.
    pub sync_lock: tokio::sync::Mutex<()>,
    pub sync_status: Mutex<SyncStatus>,
    /// Per-device SQLite/FTS5 index of text in images. None if it couldn't be opened
    /// (search then falls back to filenames only).
    pub index: Mutex<Option<index::Index>>,
    /// Squashed image text per meme, for typo-tolerant text search (kept in sync with `index`).
    pub text_cache: Mutex<fuzzytext::TextCache>,
    /// Wakes the background OCR/indexing worker.
    pub text_tx: std::sync::mpsc::Sender<()>,
    pub ocr_status: Mutex<textindex::OcrStatus>,
    /// Memes this device just downloaded from Drive, and when. OCR waits a little for
    /// their sidecars (the device that added them is already reading their text).
    pub arrived: Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// A newer published version, if the updater found one (e.g. "0.2.0").
    pub update: Mutex<Option<String>>,
    /// The tray's "Check for Updates… / Install Update…" item.
    update_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// Deletes that can still be undone, by token.
    pub pending_deletes: Mutex<std::collections::HashMap<String, sync::HeldDelete>>,
    config: Mutex<Config>,
    matcher: Mutex<Matcher>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    shortcut_error: Mutex<Option<String>>,
    /// Set while a native dialog is open so the pane doesn't hide on blur.
    dialog_open: AtomicBool,
    /// A drag (files/images/links) is over the pane: don't hide it.
    dragging: AtomicBool,
    /// Whether the pane has actually received focus since it was last shown. Windows
    /// may refuse to focus a window (e.g. right after launch); we only hide on blur
    /// once it really had focus, so it can't vanish before you ever see it.
    had_focus: AtomicBool,
    /// Which window mode is applied: Some(true) = search pane, Some(false) = setup window.
    window_mode: Mutex<Option<bool>>,
    /// The global shortcut that opens the pane (configurable in Settings).
    shortcut: Mutex<Shortcut>,
    /// The tray's "Open memeity (...)" item, relabelled when the shortcut changes.
    open_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// Whether the Settings window is open (macOS shows a Dock icon while it is).
    settings_open: AtomicBool,
    /// Asks the text worker to re-run OCR on every meme on its next pass.
    pub reread_text: AtomicBool,
    /// The window that was in front before the pane opened: where instant insert pastes.
    prev_window: AtomicIsize,
}

// ---------- config & paths ----------

fn config(app: &AppHandle) -> Config {
    app.state::<AppState>().config.lock().unwrap().clone()
}

fn update_config(app: &AppHandle, f: impl FnOnce(&mut Config)) {
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    f(&mut cfg);
    if let Ok(dir) = app.path().app_config_dir()
        && let Err(e) = cfg.save(&dir) {
            log::warn!("couldn't save config: {e}");
        }
}

/// Where a Drive folder is mirrored on this device: `~/memeity/<folder name>`.
fn local_dir(app: &AppHandle, folder: &DriveFolder) -> Option<PathBuf> {
    let name = sync::sanitize(&folder.name);
    let name = if name.is_empty() { folder.id.clone() } else { name };
    app.path().home_dir().ok().map(|h| h.join("memeity").join(name))
}

/// The Drive folder being synced and its local mirror, if one is chosen.
pub fn sync_target(app: &AppHandle) -> Option<(DriveFolder, PathBuf)> {
    let folder = config(app).drive_folder?;
    let dir = local_dir(app, &folder)?;
    Some((folder, dir))
}

// ---------- status ----------

#[derive(Serialize)]
struct Status {
    /// "setup" (no OAuth client) | "signin" | "folder" (pick a Drive folder) | "ready"
    stage: &'static str,
    account: Option<String>,
    drive_folder: Option<DriveFolder>,
    local_folder: Option<String>,
    count: usize,
    /// How many memes have had their text read (for Settings > Text Search).
    text_indexed: usize,
    shortcut: String,
    shortcut_error: Option<String>,
    sync: SyncStatus,
    ocr: textindex::OcrStatus,
    client_file: Option<String>,
    autostart: bool,
    /// A newer version waiting to be installed, if any.
    update: Option<String>,
    /// "paste" or "copy": what Enter / click does in the pane.
    primary_action: &'static str,
    version: String,
}

/// True once memeity is fully set up (client loaded, signed in, Drive folder picked).
fn is_ready(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    state.google.has_creds() && state.google.signed_in() && config(app).drive_folder.is_some()
}

/// Setup (client JSON, sign-in, folder picking) happens in a normal window you can
/// Alt+Tab away from and back to. Once ready, the same window becomes the Raycast-style
/// pane: frameless, always on top, no taskbar entry, hides when it loses focus.
fn apply_window_mode(app: &AppHandle) {
    let ready = is_ready(app);
    {
        let state = app.state::<AppState>();
        let mut mode = state.window_mode.lock().unwrap();
        if *mode == Some(ready) {
            return;
        }
        *mode = Some(ready);
    }
    let Some(win) = pane(app) else { return };
    let _ = win.set_decorations(!ready);
    let _ = win.set_always_on_top(ready);
    let _ = win.set_skip_taskbar(ready);
    let _ = win.set_title(if ready { "memeity" } else { "memeity setup" });
    // macOS: menu-bar only once set up; a regular Dock app during setup.
    settings::update_activation_policy(app);
    lock_size(&win);
    set_rounded(&win, ready);
    if ready {
        position_pane(app, &win);
    } else {
        let _ = win.center();
    }
}

/// Corner radius of the search pane on macOS, in points (Windows 11 has its own fixed one).
#[cfg(target_os = "macos")]
const PANE_RADIUS: f64 = 16.0;

/// Rounds the frameless search pane's corners (or squares them again for the normal,
/// decorated setup window, which the OS already rounds itself). Done natively, so the
/// window keeps its drop shadow.
#[cfg(target_os = "macos")]
fn set_rounded(win: &WebviewWindow, rounded: bool) {
    let Ok(ptr) = win.ns_window() else { return };
    let ptr = ptr as usize;
    let _ = win.run_on_main_thread(move || {
        use objc2_app_kit::{NSColor, NSWindow};
        // SAFETY: Tauri's live NSWindow for this window, used on the main thread.
        let window: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
        // Clear, see-through window background: the corners outside the rounded content show through.
        window.setOpaque(!rounded);
        let background = if rounded { NSColor::clearColor() } else { NSColor::windowBackgroundColor() };
        window.setBackgroundColor(Some(&background));
        if let Some(view) = window.contentView() {
            view.setWantsLayer(true);
            if let Some(layer) = view.layer() {
                layer.setCornerRadius(if rounded { PANE_RADIUS } else { 0.0 });
                layer.setMasksToBounds(rounded);
            }
        }
        // Recompute the shadow from the new (rounded) shape.
        window.invalidateShadow();
    });
}

/// Windows 11: ask DWM for rounded corners (Windows 10 ignores it and stays square).
#[cfg(windows)]
fn set_rounded(win: &WebviewWindow, rounded: bool) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_ROUND, DWM_WINDOW_CORNER_PREFERENCE,
    };
    let Ok(hwnd) = win.hwnd() else { return };
    let preference: DWM_WINDOW_CORNER_PREFERENCE = if rounded { DWMWCP_ROUND } else { DWMWCP_DEFAULT };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&preference as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn set_rounded(_win: &WebviewWindow, _rounded: bool) {}

fn status(app: &AppHandle) -> Status {
    apply_window_mode(app);
    let state = app.state::<AppState>();
    let cfg = config(app);
    let stage = if !state.google.has_creds() {
        "setup"
    } else if !state.google.signed_in() {
        "signin"
    } else if cfg.drive_folder.is_none() {
        "folder"
    } else {
        "ready"
    };
    Status {
        stage,
        account: cfg.account.clone(),
        local_folder: sync_target(app).map(|(_, d)| d.to_string_lossy().into_owned()),
        drive_folder: cfg.drive_folder,
        count: state.library.read().unwrap().memes.len(),
        text_indexed: state
            .index
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|idx| idx.indexed_count().ok())
            .unwrap_or(0),
        shortcut: settings::label(&settings::current(app)),
        shortcut_error: state.shortcut_error.lock().unwrap().clone(),
        sync: state.sync_status.lock().unwrap().clone(),
        ocr: state.ocr_status.lock().unwrap().clone(),
        client_file: app.path().app_config_dir().ok().map(|d| d.join(CLIENT_FILE).to_string_lossy().into_owned()),
        autostart: settings::autostart_enabled(app),
        update: state.update.lock().unwrap().clone(),
        primary_action: settings::primary_action(app),
        version: app.package_info().version.to_string(),
    }
}

// ---------- pane window ----------

fn pane(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(PANE)
}

/// Put the pane on whichever monitor the cursor is on, centered, in the upper third.
fn position_pane(app: &AppHandle, win: &WebviewWindow) {
    let Ok(cursor) = app.cursor_position() else { return };
    let Ok(Some(monitor)) = app.monitor_from_point(cursor.x, cursor.y) else { return };
    let Ok(size) = win.outer_size() else { return };
    let (mp, ms) = (monitor.position(), monitor.size());
    let x = mp.x + (ms.width as i32 - size.width as i32) / 2;
    let y = mp.y + (ms.height as i32 - size.height as i32) / 3;
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

/// Pin the window to its fixed size. Re-applied on every show because window
/// managers (e.g. tiling ones like komorebi) or a maximize can resize it behind our back.
fn lock_size(win: &WebviewWindow) {
    let size = LogicalSize::new(PANE_WIDTH, PANE_HEIGHT);
    if win.is_maximized().unwrap_or(false) {
        let _ = win.unmaximize();
    }
    let _ = win.set_min_size(Some(size));
    let _ = win.set_max_size(Some(size));
    let _ = win.set_size(size);
}

fn show_pane(app: &AppHandle) {
    let Some(win) = pane(app) else { return };
    remember_foreground(app, &win);
    // Paste-to-app hides memeity as a whole app on macOS; un-hide it, or its window
    // stays invisible.
    #[cfg(target_os = "macos")]
    let _ = app.show();
    apply_window_mode(app);
    if is_ready(app) {
        lock_size(&win);
        position_pane(app, &win);
    }
    app.state::<AppState>().had_focus.store(false, Ordering::SeqCst);
    set_window_animation(&win, true); // opening keeps the usual Windows animation
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit_to(PANE, "pane-shown", ());
    sync::trigger_if_stale(app);
}

/// Hides the pane after it loses focus, except when the user is dragging something onto
/// it: dragging from Explorer means clicking Explorer first, which takes focus. So if
/// the mouse button is down when focus goes, wait for the release and only hide if no
/// drag arrived over the pane (a plain click elsewhere hides on mouse-up).
#[cfg(windows)]
fn hide_on_blur(app: &AppHandle) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    let button_down = || unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000 != 0;
    if !button_down() {
        hide_pane(app);
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        while button_down() {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // Give a drop on the pane a moment to be reported before deciding.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let state = app.state::<AppState>();
        let refocused = pane(&app).is_some_and(|w| w.is_focused().unwrap_or(false));
        if !state.dragging.load(Ordering::SeqCst) && !refocused {
            hide_pane(&app);
        }
    });
}

#[cfg(not(windows))]
fn hide_on_blur(app: &AppHandle) {
    hide_pane(app);
}

/// The pane reports drags entering/leaving it. After a drop (`refocus`) it takes focus
/// back, so clicking away afterwards hides it as usual.
#[tauri::command]
fn set_dragging(app: AppHandle, active: bool, refocus: Option<bool>) {
    app.state::<AppState>().dragging.store(active, Ordering::SeqCst);
    if refocus == Some(true)
        && let Some(win) = pane(&app)
        && win.is_visible().unwrap_or(false)
    {
        let _ = win.set_focus();
    }
}

/// Notes which app the user was in, so instant insert can paste back into it.
#[cfg(windows)]
fn remember_foreground(app: &AppHandle, win: &WebviewWindow) {
    let foreground = insert::win::foreground();
    let ours = win.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    if foreground != 0 && foreground != ours {
        app.state::<AppState>().prev_window.store(foreground, Ordering::SeqCst);
    }
}

#[cfg(not(windows))]
fn remember_foreground(_app: &AppHandle, _win: &WebviewWindow) {}

/// Closes the pane instantly (Esc, clicking away, the hotkey, the close button).
fn hide_pane(app: &AppHandle) {
    if let Some(win) = pane(app) {
        set_window_animation(&win, false);
        let _ = win.hide();
    }
}

/// Closes the pane with the Windows fade-out: only after copying a meme, as a
/// "done" cue.
fn hide_pane_after_copy(app: &AppHandle) {
    if let Some(win) = pane(app) {
        set_window_animation(&win, true);
        let _ = win.hide();
    }
}

/// Turns Windows' show/hide window animation on or off for the pane.
#[cfg(windows)]
fn set_window_animation(win: &WebviewWindow, enabled: bool) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED};
    let Ok(hwnd) = win.hwnd() else { return };
    let disabled: i32 = (!enabled).into(); // a Win32 BOOL
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            (&disabled as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn set_window_animation(_win: &WebviewWindow, _enabled: bool) {}

fn toggle_pane(app: &AppHandle) {
    let Some(win) = pane(app) else { return };
    if win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false) {
        hide_pane(app);
    } else {
        show_pane(app);
    }
}

// ---------- local library ----------

/// Index the local mirror and watch it. (The webview loads images through `thumb://`,
/// which only serves files in the current library, so no folder is ever opened up to it.)
fn load_library(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some((_, dir)) = sync_target(app) else {
        *state.library.write().unwrap() = Library::default();
        *state.watcher.lock().unwrap() = None;
        let _ = app.emit("library-changed", ());
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("couldn't create {}: {e}", dir.display());
    }
    sync::sweep_held_deletes(&dir);
    let cache = state.library.read().unwrap().dims_cache();
    let mut lib = Library::scan(&dir);
    lib.fill_dims(&cache);
    *state.library.write().unwrap() = lib;
    // Replacing the watcher drops (and stops) the old one.
    *state.watcher.lock().unwrap() = watcher::watch(app.clone(), dir);
    let _ = app.emit("library-changed", ());
    textindex::trigger(app);
}

/// Opens (or creates) the per-device search index in the app-data dir.
fn open_index(app: &AppHandle) -> Option<index::Index> {
    let dir = app.path().app_data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    index::Index::open(&dir.join("index.sqlite"))
        .inspect_err(|e| log::warn!("couldn't open search index: {e}"))
        .ok()
}

/// Rescan the current library (after a sync or delete).
pub fn refresh_library(app: &AppHandle) {
    let state = app.state::<AppState>();
    let root = state.library.read().unwrap().root.clone();
    if let Some(root) = root {
        let cache = state.library.read().unwrap().dims_cache();
        let mut fresh = Library::scan(&root);
        fresh.fill_dims(&cache);
        let mut lib = state.library.write().unwrap();
        if lib.root.as_ref() == Some(&root) {
            *lib = fresh;
        }
    }
    let _ = app.emit("library-changed", ());
    textindex::trigger(app);
}

// ---------- actions shared by commands and the tray ----------

fn open_local_folder(app: &AppHandle) {
    if let Some((_, dir)) = sync_target(app) {
        let _ = std::fs::create_dir_all(&dir);
        hide_pane(app);
        let _ = app.opener().open_path(dir.to_string_lossy(), None::<&str>);
    }
}

/// Finds (or creates) the "memeity" folder on Drive and starts syncing it.
async fn setup_library_folder(app: &AppHandle) -> Result<(), String> {
    let folder = app.state::<AppState>().google.find_or_create_library().await?;
    update_config(app, |c| c.drive_folder = Some(DriveFolder { id: folder.id, name: folder.name }));
    load_library(app);
    sync::trigger(app);
    Ok(())
}

/// Drive access level changed since this device last signed in (e.g. full Drive ->
/// memeity's files only): sign in again and start with a fresh folder lookup. Sync's
/// own state resets too (see SyncState), so nothing is mistaken for a remote delete.
fn migrate_auth_scope(app: &AppHandle) {
    if config(app).auth_scope.as_deref() == Some(google::SCOPE) {
        return;
    }
    let state = app.state::<AppState>();
    if state.google.signed_in() {
        state.google.sign_out();
    }
    update_config(app, |c| {
        c.account = None;
        c.drive_folder = None;
    });
}

fn sign_out(app: &AppHandle) {
    app.state::<AppState>().google.sign_out();
    update_config(app, |c| c.account = None);
    sync::trigger(app); // updates the status to "off"
    let _ = app.emit("library-changed", ());
}

// ---------- commands ----------

#[tauri::command]
fn get_status(app: AppHandle) -> Status {
    status(&app)
}

/// Runs on a worker thread (`async`), not the UI thread: it runs on every keystroke.
#[tauri::command(async)]
fn search(app: AppHandle, query: String, offset: Option<usize>, limit: usize) -> SearchResult {
    let state = app.state::<AppState>();
    /// How many image-text matches to consider for ranking.
    const TEXT_HITS: usize = 500;
    let text_bonus = if query.trim().is_empty() {
        Default::default()
    } else {
        // Word matches (FTS), plus loose matches that ignore spaces and allow typos.
        let words = state
            .index
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|idx| idx.search(&query, TEXT_HITS).ok())
            .unwrap_or_default();
        let loose = state.text_cache.lock().unwrap().matches(&query);
        library::text_bonuses(&words, &loose)
    };
    let usage = state
        .index
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|idx| idx.usage_scores(now_ms()).ok())
        .unwrap_or_default();
    let lib = state.library.read().unwrap();
    let mut matcher = state.matcher.lock().unwrap();
    lib.search(&query, offset.unwrap_or(0), limit, &mut matcher, &text_bonus, &usage)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Counts a copy/paste towards "recently & frequently used".
fn record_use(app: &AppHandle, path: &std::path::Path) {
    let Some((_, root)) = sync_target(app) else { return };
    let Ok(rel) = path.strip_prefix(&root) else { return };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if let Some(idx) = app.state::<AppState>().index.lock().unwrap().as_ref()
        && let Err(e) = idx.record_use(&rel, now_ms())
    {
        log::warn!("couldn't record use: {e}");
    }
}

#[tauri::command]
async fn copy_meme(app: AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let known = app.state::<AppState>().library.read().unwrap().contains(&path);
    if !known {
        return Err("That file isn't in your meme folder.".into());
    }
    clipboard::copy(&path)?;
    record_use(&app, &path);
    hide_pane_after_copy(&app);
    Ok(())
}

#[derive(Serialize)]
struct MemeText {
    /// "read" (text below, possibly empty), "pending" (not read yet), "failed" (couldn't read the image)
    state: &'static str,
    text: Option<String>,
    lang: Option<String>,
    /// When it was read (RFC 3339).
    at: Option<String>,
    /// The user edited the text.
    edited: bool,
}

/// What OCR found in a meme, from its sidecar.
#[tauri::command(async)]
fn meme_text(app: AppHandle, path: String) -> Result<MemeText, String> {
    let (_, root) = sync_target(&app).ok_or("No Drive folder selected.")?;
    let path = PathBuf::from(path);
    let rel = path
        .strip_prefix(&root)
        .map_err(|_| "That file isn't in your meme folder.".to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    let state = app.state::<AppState>();
    let entry = state.index.lock().unwrap().as_ref().and_then(|idx| idx.lookup(&rel).ok().flatten());
    let pending = MemeText { state: "pending", text: None, lang: None, at: None, edited: false };
    let Some(entry) = entry else { return Ok(pending) };
    match sidecar::read(&root, &entry.md5).and_then(|sc| sc.ocr) {
        Some(ocr) => Ok(MemeText { state: "read", text: Some(ocr.text), lang: ocr.lang, at: ocr.at, edited: ocr.edited }),
        // Indexed without a sidecar means the image couldn't be decoded/read.
        None if entry.indexed => Ok(MemeText { state: "failed", ..pending }),
        None => Ok(pending),
    }
}

#[derive(Serialize)]
struct DuplicateItem {
    #[serde(flatten)]
    hit: library::Hit,
    bytes: u64,
}

#[derive(Serialize)]
struct DuplicateGroup {
    /// All identical files (vs. visually similar).
    exact: bool,
    /// Best copy to keep first: highest resolution, then biggest file, then oldest.
    items: Vec<DuplicateItem>,
}

#[derive(Serialize)]
struct Duplicates {
    groups: Vec<DuplicateGroup>,
    /// Memes not yet fingerprinted (near-duplicates among them aren't found yet).
    pending: usize,
    /// Byte-identical extra copies (every copy but the best of each set).
    exact_extras: usize,
}

/// Best copy first: highest resolution, then biggest file, then oldest.
fn best_first(items: &mut [DuplicateItem]) {
    items.sort_by_key(|i| {
        let area = u64::from(i.hit.width.unwrap_or(0)) * u64::from(i.hit.height.unwrap_or(0));
        (std::cmp::Reverse(area), std::cmp::Reverse(i.bytes), i.hit.version)
    });
}

fn duplicate_item(hit: library::Hit) -> DuplicateItem {
    let bytes = std::fs::metadata(&hit.path).map(|m| m.len()).unwrap_or(0);
    DuplicateItem { hit, bytes }
}

/// Every byte-identical copy of a meme except the best one of each set.
fn exact_extras(app: &AppHandle) -> Result<Vec<library::Hit>, String> {
    let state = app.state::<AppState>();
    let candidates = {
        let guard = state.index.lock().unwrap();
        let idx = guard.as_ref().ok_or("The search index isn't available.")?;
        idx.duplicate_candidates().map_err(|e| e.to_string())?
    };
    let mut by_md5: std::collections::HashMap<&str, Vec<&str>> = Default::default();
    for c in &candidates {
        by_md5.entry(&c.md5).or_default().push(&c.rel);
    }
    let lib = state.library.read().unwrap();
    let mut extras = Vec::new();
    for rels in by_md5.values().filter(|r| r.len() > 1) {
        let mut items: Vec<DuplicateItem> = rels.iter().filter_map(|rel| lib.hit_for(rel)).map(duplicate_item).collect();
        best_first(&mut items);
        extras.extend(items.into_iter().skip(1).map(|i| i.hit));
    }
    Ok(extras)
}

/// How many identical extra copies the library has (for the pane's cleanup prompt).
#[tauri::command(async)]
fn duplicate_count(app: AppHandle) -> usize {
    exact_extras(&app).map(|e| e.len()).unwrap_or(0)
}

/// Deletes every identical extra copy, keeping the best of each set. They go to Drive's
/// trash (30 days) and this device's Recycle Bin. Returns how many were removed.
#[tauri::command]
async fn remove_exact_duplicates(app: AppHandle) -> Result<usize, String> {
    let extras = exact_extras(&app)?;
    let mut removed = 0;
    for hit in &extras {
        sync::delete(&app, std::path::Path::new(&hit.path)).await?;
        removed += 1;
    }
    log::info!("removed {removed} identical duplicate(s)");
    refresh_library(&app);
    textindex::trigger(&app);
    Ok(removed)
}

#[tauri::command(async)]
fn find_duplicates(app: AppHandle) -> Result<Duplicates, String> {
    let state = app.state::<AppState>();
    let (candidates, pending) = {
        let guard = state.index.lock().unwrap();
        let idx = guard.as_ref().ok_or("The search index isn't available.")?;
        let candidates = idx.duplicate_candidates().map_err(|e| e.to_string())?;
        let pending = idx.missing_phash(100_000).map(|v| v.len()).unwrap_or(0);
        (candidates, pending)
    };
    let groups = dupes::group(&candidates);
    let exact_extras = exact_extras(&app).map(|e| e.len()).unwrap_or(0);
    let lib = state.library.read().unwrap();
    let groups = groups
        .into_iter()
        .filter_map(|g| {
            let mut items: Vec<DuplicateItem> =
                g.rels.iter().filter_map(|rel| lib.hit_for(rel)).map(duplicate_item).collect();
            best_first(&mut items);
            (items.len() > 1).then_some(DuplicateGroup { exact: g.exact, items })
        })
        .collect();
    Ok(Duplicates { groups, pending, exact_extras })
}

/// The meme's library root and relative path, checking it's actually in the library.
fn library_rel(app: &AppHandle, path: &std::path::Path) -> Result<(PathBuf, String), String> {
    let (_, root) = sync_target(app).ok_or("No meme folder selected.")?;
    if !app.state::<AppState>().library.read().unwrap().contains(path) {
        return Err("That file isn't in your meme folder.".into());
    }
    let rel = path
        .strip_prefix(&root)
        .map_err(|_| "That file isn't in your meme folder.".to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    Ok((root, rel))
}

/// Renames a meme (keeping its extension and folder), here and on Drive. Returns the
/// new path.
#[tauri::command]
async fn rename_meme(app: AppHandle, path: String, name: String) -> Result<String, String> {
    let path = PathBuf::from(path);
    library_rel(&app, &path)?;
    let stem = sync::sanitize(name.trim());
    if stem.is_empty() {
        return Err("Give it a name.".into());
    }
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let file_name = if ext.is_empty() { stem } else { format!("{stem}.{ext}") };
    let dest = path.with_file_name(&file_name);
    if dest == path {
        return Ok(path.to_string_lossy().into_owned());
    }
    // Checks for a clash (a case-only rename like "Cat.png" -> "cat.png" isn't one).
    sync::rename(&app, &path, &dest).await?;
    refresh_library(&app);
    Ok(dest.to_string_lossy().into_owned())
}

/// Replaces a meme's recognized text with what the user typed. Stored in its sidecar
/// (so it syncs everywhere) and marked as edited, so OCR never overwrites it.
#[tauri::command(async)]
fn set_meme_text(app: AppHandle, path: String, text: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let (root, rel) = library_rel(&app, &path)?;
    let state = app.state::<AppState>();
    let md5 = state
        .index
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|idx| idx.lookup(&rel).ok().flatten())
        .map(|e| e.md5)
        .ok_or("memeity is still indexing this meme. Try again in a moment.")?;

    let text = text.trim().to_string();
    let mut sc = sidecar::read(&root, &md5).unwrap_or_default();
    let engine = sc.ocr.as_ref().map(|o| o.engine.clone()).unwrap_or_else(|| ocr::ENGINE_NAME.to_string());
    sc.ocr = Some(sidecar::Ocr {
        text: text.clone(),
        engine: engine.clone(),
        lang: None,
        at: Some(humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()),
        edited: true,
    });
    sidecar::write(&root, &md5, &sc).map_err(|e| format!("Couldn't save the text: {e}"))?;
    let sidecar_mtime = std::fs::metadata(sidecar::path(&root, &md5))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    if let Some(idx) = state.index.lock().unwrap().as_ref() {
        idx.set_text(&rel, &text, &sc.tags, sidecar_mtime, Some(&engine)).map_err(|e| e.to_string())?;
    }
    state.text_cache.lock().unwrap().set(&rel, &text, &sc.tags);
    sync::trigger(&app); // upload the edited sidecar
    Ok(())
}

/// Instant insert (Shift+Enter): paste the meme into the app the user was in, then
/// restore their clipboard.
#[tauri::command]
async fn insert_meme(app: AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let state = app.state::<AppState>();
    if !state.library.read().unwrap().contains(&path) {
        return Err("That file isn't in your meme folder.".into());
    }
    let target = state.prev_window.load(Ordering::SeqCst);
    let handle = app.clone();
    let used = path.clone();
    tauri::async_runtime::spawn_blocking(move || insert::insert(&handle, &path, target))
        .await
        .map_err(|e| e.to_string())??;
    record_use(&app, &used);
    Ok(())
}

/// Drag a meme out of the pane into another app (a chat, a document, a folder). Hands
/// over the same file a copy would (converted if its format is picky), with a small
/// thumbnail under the cursor. Emits `drag-out-ended` when the drag is over.
#[tauri::command]
async fn drag_meme(app: AppHandle, window: WebviewWindow, path: String) -> Result<(), String> {
    /// Longest side of the image under the cursor, in pixels.
    const ICON_SIDE: u32 = 96;
    let path = PathBuf::from(path);
    if !app.state::<AppState>().library.read().unwrap().contains(&path) {
        return Err("That file isn't in your meme folder.".into());
    }
    let cache = thumbs::cache_dir(&app).ok_or("No cache folder.")?;
    let source = path.clone();
    let (file, icon) = tauri::async_runtime::spawn_blocking(move || {
        let file = clipboard::shareable(&source)?;
        let (thumb, _) = thumbs::thumbnail(&cache, &source)?;
        let small = image::load_from_memory(&thumb).map_err(|e| e.to_string())?.thumbnail(ICON_SIDE, ICON_SIDE);
        let mut png = Vec::new();
        small
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((file, png))
    })
    .await
    .map_err(|e| e.to_string())??;

    // The OS drag has to start on the main thread (macOS returns right away and the drag
    // carries on; Windows returns once it's over).
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let done = handle.clone();
        let result = drag::start_drag(
            &window,
            drag::DragItem::Files(vec![file]),
            drag::Image::Raw(icon),
            move |result, _| {
                if matches!(result, drag::DragResult::Dropped) {
                    record_use(&done, &path);
                }
                let _ = done.emit("drag-out-ended", ());
            },
            drag::Options { skip_animatation_on_cancel_or_failure: false, mode: drag::DragMode::Copy },
        );
        let _ = tx.send(result.map_err(|e| format!("Couldn't start dragging: {e}")));
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())?
}

/// Show in folder (Ctrl+Shift+F): open Explorer/Finder with the meme selected.
#[tauri::command]
fn reveal_meme(app: AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !app.state::<AppState>().library.read().unwrap().contains(&path) {
        return Err("That file isn't in your meme folder.".into());
    }
    hide_pane(&app);
    app.opener().reveal_item_in_dir(&path).map_err(|e| format!("Couldn't show the file: {e}"))
}

/// Trash a meme on Drive and remove it from this device. Returns a token that
/// `undo_delete` accepts for the next few seconds.
#[tauri::command]
async fn delete_meme(app: AppHandle, path: String) -> Result<String, String> {
    let path = PathBuf::from(path);
    let known = app.state::<AppState>().library.read().unwrap().contains(&path);
    if !known {
        return Err("That file isn't in your meme folder.".into());
    }
    let token = sync::delete(&app, &path).await?;
    refresh_library(&app);
    Ok(token)
}

#[tauri::command]
async fn undo_delete(app: AppHandle, token: String) -> Result<String, String> {
    let restored = sync::undo_delete(&app, &token).await?;
    refresh_library(&app);
    sync::trigger(&app);
    Ok(restored.to_string_lossy().into_owned())
}

/// Pick the OAuth client JSON downloaded from Google Cloud Console.
#[tauri::command]
async fn load_client_file(app: AppHandle) -> Result<Status, String> {
    let state = app.state::<AppState>();
    state.dialog_open.store(true, Ordering::SeqCst);
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Choose your Google OAuth client JSON")
        .add_filter("Google OAuth client", &["json"]);
    if let Some(win) = pane(&app) {
        dialog = dialog.set_parent(&win);
    }
    let picked = dialog.blocking_pick_file();
    state.dialog_open.store(false, Ordering::SeqCst);
    show_pane(&app);

    let Some(path) = picked.and_then(|p| p.into_path().ok()) else {
        return Ok(status(&app));
    };
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let creds = google::parse_client_json(&bytes)?;
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(CLIENT_FILE), &bytes).map_err(|e| e.to_string())?;
    state.google.set_creds(creds);
    Ok(status(&app))
}

#[tauri::command]
async fn sign_in(app: AppHandle) -> Result<Status, String> {
    let state = app.state::<AppState>();
    let opener = app.clone();
    let result = state
        .google
        .sign_in(move |url| {
            let _ = opener.opener().open_url(url, None::<&str>);
        })
        .await;
    show_pane(&app);
    result?;
    let email = state.google.account_email().await.ok();
    update_config(&app, |c| {
        c.account = email;
        c.auth_scope = Some(google::SCOPE.to_string());
    });
    // Go straight to the library. If this fails, the pane retries via setup_drive_folder.
    setup_library_folder(&app).await?;
    Ok(status(&app))
}

#[tauri::command]
fn sign_out_cmd(app: AppHandle) -> Status {
    sign_out(&app);
    status(&app)
}

/// Signed in but no library folder yet (e.g. the first attempt failed offline): retry.
#[tauri::command]
async fn setup_drive_folder(app: AppHandle) -> Result<Status, String> {
    setup_library_folder(&app).await?;
    Ok(status(&app))
}

/// The app's own pages: `tauri://localhost` (macOS/Linux), `http(s)://tauri.localhost`
/// (Windows), and the Vite dev server in `tauri dev`.
fn is_app_url(url: &tauri::Url) -> bool {
    match (url.scheme(), url.host_str()) {
        ("tauri", Some("localhost")) => true,
        ("http" | "https", Some("tauri.localhost")) => true,
        ("http", Some("localhost")) => cfg!(debug_assertions) && url.port() == Some(1420),
        // about:blank and the like: harmless, and webviews use them internally.
        ("about", _) => true,
        _ => false,
    }
}

/// Copies a bug-report-ready summary to the clipboard: version, OS, sync and text-search
/// status, and the end of the log. (Not the account email.)
#[tauri::command(async)]
fn copy_diagnostics(app: AppHandle) -> Result<(), String> {
    /// How much of the log to include.
    const LOG_LINES: usize = 300;
    let s = status(&app);
    let mut out = format!(
        "memeity {} on {} {}\nstage: {}\nmemes: {} ({} with text read)\nsync: {} {}\n\
         last sync: {} down, {} up, {} removed\ntext: {} {}/{} {}\nshortcut: {}{}\n",
        s.version,
        std::env::consts::OS,
        std::env::consts::ARCH,
        s.stage,
        s.count,
        s.text_indexed,
        s.sync.state,
        s.sync.message.as_deref().unwrap_or(""),
        s.sync.last.downloaded,
        s.sync.last.uploaded,
        s.sync.last.removed,
        s.ocr.state,
        s.ocr.done,
        s.ocr.total,
        s.ocr.message.as_deref().unwrap_or(""),
        s.shortcut,
        s.shortcut_error.map(|e| format!(" ({e})")).unwrap_or_default(),
    );
    let log = app
        .path()
        .app_log_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join(format!("{LOG_FILE}.log"))).ok())
        .unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    out.push_str(&format!("\n--- last {} log lines ---\n", lines.len().min(LOG_LINES)));
    out.push_str(&lines[lines.len().saturating_sub(LOG_LINES)..].join("\n"));
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(out))
        .map_err(|e| format!("Couldn't copy to the clipboard: {e}"))
}

#[tauri::command]
fn sync_now(app: AppHandle) {
    sync::trigger(&app);
}

#[tauri::command]
fn open_folder(app: AppHandle) {
    open_local_folder(&app);
}

#[tauri::command]
fn hide(app: AppHandle) {
    hide_pane(&app);
}

/// Opens a setup link in the user's browser. Only Google Cloud Console pages are allowed,
/// so the webview can't be used to launch arbitrary URLs.
#[tauri::command]
fn open_setup_link(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://console.cloud.google.com/") {
        return Err("Only Google Cloud Console links can be opened from setup.".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

// ---------- setup ----------

/// macOS menu-bar icons are "template" images: black strokes on transparent, which
/// macOS recolors for light/dark menu bars. Elsewhere the tray uses the full-colour app icon.
#[cfg(target_os = "macos")]
fn tray_builder(_app: &AppHandle) -> tauri::Result<TrayIconBuilder<tauri::Wry>> {
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?;
    Ok(TrayIconBuilder::with_id("main").icon(icon).icon_as_template(true))
}

#[cfg(not(target_os = "macos"))]
fn tray_builder(app: &AppHandle) -> tauri::Result<TrayIconBuilder<tauri::Wry>> {
    Ok(TrayIconBuilder::with_id("main").icon(app.default_window_icon().cloned().expect("bundle has an icon")))
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    // The shortcut in this label (and the tooltip) is filled in by settings::register_initial.
    let open = MenuItem::with_id(app, "open", "Open memeity", true, None::<&str>)?;
    let settings_item = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let update_item = MenuItem::with_id(app, "update", "Check for Updates\u{2026}", true, None::<&str>)?;
    let sync_now = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
    let local = MenuItem::with_id(app, "local", "Open local meme folder", true, None::<&str>)?;
    let signout = MenuItem::with_id(app, "signout", "Sign out of Google", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit memeity", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &settings_item, &update_item, &sep1, &sync_now, &local, &signout, &sep2, &quit])?;

    *app.state::<AppState>().open_item.lock().unwrap() = Some(open.clone());
    *app.state::<AppState>().update_item.lock().unwrap() = Some(update_item.clone());

    tray_builder(app)?
        .tooltip("memeity")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_pane(app),
            "settings" => settings::open(app),
            "update" => updates::tray_clicked(app),
            "sync" => sync::trigger(app),
            "local" => open_local_folder(app),
            "signout" => {
                sign_out(app);
                show_pane(app);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_pane(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let (sync_tx, sync_rx) = tokio::sync::mpsc::unbounded_channel();
    let (text_tx, text_rx) = std::sync::mpsc::channel();

    tauri::Builder::default()
        // Grid thumbnails: `thumb://<path>`, made on demand and cached (see thumbs.rs).
        // `?full=1` serves the original instead (details preview, animated GIFs).
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let uri_path = request.uri().path().to_string();
            let full = request.uri().query().is_some_and(|q| q.split('&').any(|p| p == "full=1"));
            tauri::async_runtime::spawn(async move {
                responder.respond(thumbs::respond(app, uri_path, full).await);
            });
        })
        // A small rotating log in the OS log folder (and the terminal in dev), so problems
        // users hit can be diagnosed: Settings > About > Copy diagnostics.
        .plugin(
            tauri_plugin_log::Builder::new()
                .clear_targets()
                .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                    file_name: Some(LOG_FILE.into()),
                }))
                .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout))
                .level(log::LevelFilter::Info)
                // Dependencies' chatter (HTTP internals etc.) only when it's a problem.
                .level_for("reqwest", log::LevelFilter::Warn)
                .level_for("hyper", log::LevelFilter::Warn)
                .level_for("hyper_util", log::LevelFilter::Warn)
                .level_for("ort", log::LevelFilter::Warn)
                .max_file_size(1_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
                .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                .build(),
        )
        // memeity's windows only ever show memeity. Without this, a file dropped where the
        // page doesn't catch it (or any stray link) would replace the app with that file.
        .plugin(
            tauri::plugin::Builder::<tauri::Wry, ()>::new("navigation-guard")
                .on_navigation(|_webview, url| {
                    let allowed = is_app_url(url);
                    if !allowed {
                        log::warn!("blocked navigation to {}", url.scheme());
                    }
                    allowed
                })
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![settings::AUTOSTART_ARG]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed && *shortcut == settings::current(app) {
                        toggle_pane(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_status,
            search,
            copy_meme,
            meme_text,
            rename_meme,
            find_duplicates,
            duplicate_count,
            remove_exact_duplicates,
            set_meme_text,
            insert_meme,
            reveal_meme,
            drag_meme,
            delete_meme,
            undo_delete,
            load_client_file,
            sign_in,
            sign_out_cmd,
            setup_drive_folder,
            sync_now,
            copy_diagnostics,
            open_folder,
            open_setup_link,
            hide,
            settings::open_settings,
            settings::set_shortcut,
            settings::reset_shortcut,
            settings::retry_shortcut,
            settings::set_autostart,
            settings::set_primary_action,
            settings::reread_all_text,
            settings::open_link,
            import::import_bytes,
            import::import_url,
            set_dragging,
            updates::check_for_update,
            updates::install_update
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            log::info!(
                "memeity {} starting on {} {}",
                app.package_info().version,
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            let config_dir = app.path().app_config_dir()?;
            // memeity's built-in client; a user-supplied one only for builds without it.
            let creds = google::embedded_client().or_else(|| {
                std::fs::read(config_dir.join(CLIENT_FILE))
                    .ok()
                    .and_then(|b| google::parse_client_json(&b).ok())
            });

            if let Ok(cache) = app.path().app_cache_dir() {
                clipboard::set_share_dir(cache.join("shared"));
            }

            app.manage(AppState {
                library: RwLock::new(Library::default()),
                google: Google::new(creds),
                sync_tx,
                sync_lock: tokio::sync::Mutex::new(()),
                sync_status: Mutex::new(SyncStatus { state: "off", ..Default::default() }),
                index: Mutex::new(open_index(app.handle())),
                text_cache: Mutex::new(fuzzytext::TextCache::default()),
                text_tx,
                ocr_status: Mutex::new(textindex::OcrStatus { state: "idle", ..Default::default() }),
                arrived: Mutex::new(Default::default()),
                pending_deletes: Mutex::new(Default::default()),
                update: Mutex::new(None),
                update_item: Mutex::new(None),
                config: Mutex::new(Config::load(&config_dir)),
                matcher: Mutex::new(Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths())),
                watcher: Mutex::new(None),
                shortcut_error: Mutex::new(None),
                dialog_open: AtomicBool::new(false),
                dragging: AtomicBool::new(false),
                had_focus: AtomicBool::new(false),
                window_mode: Mutex::new(None),
                shortcut: Mutex::new(settings::DEFAULT_SHORTCUT.parse().expect("default shortcut is valid")),
                open_item: Mutex::new(None),
                settings_open: AtomicBool::new(false),
                reread_text: AtomicBool::new(false),
                prev_window: AtomicIsize::new(0),
            });

            migrate_auth_scope(&handle);

            // Fill the typo-tolerant text cache from the index, so search works immediately.
            {
                let state = app.state::<AppState>();
                let texts = state.index.lock().unwrap().as_ref().and_then(|idx| idx.all_texts().ok());
                let mut cache = state.text_cache.lock().unwrap();
                for (rel, text, tags) in texts.unwrap_or_default() {
                    cache.set(&rel, &text, &tags);
                }
            }

            setup_tray(&handle)?;
            settings::register_initial(&handle);

            // Launcher behaviour: hide on blur, and closing just hides.
            let win = pane(&handle).expect("pane window is defined in tauri.conf.json");
            let h = handle.clone();
            win.on_window_event(move |event| match event {
                WindowEvent::Focused(true) => {
                    h.state::<AppState>().had_focus.store(true, Ordering::SeqCst);
                }
                WindowEvent::Focused(false) => {
                    let state = h.state::<AppState>();
                    // Only the search pane hides on blur; the setup window stays put.
                    if is_ready(&h)
                        && state.had_focus.load(Ordering::SeqCst)
                        && !state.dialog_open.load(Ordering::SeqCst)
                    {
                        hide_on_blur(&h);
                    }
                }
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    hide_pane(&h);
                }
                _ => {}
            });

            // Index off the main thread so startup stays instant on big folders.
            let h = handle.clone();
            std::thread::spawn(move || load_library(&h));

            sync::spawn_loop(handle.clone(), sync_rx);
            textindex::spawn(handle.clone(), text_rx);
            updates::spawn(handle.clone());
            sync::trigger(&handle);

            // Opened by hand: show the pane so it's obvious memeity is running. Started at
            // login: stay quietly in the tray/menu bar until the shortcut is pressed.
            // Also show it if the shortcut couldn't be registered: otherwise there'd be
            // no (obvious) way to open memeity, and nothing saying why.
            let at_login = std::env::args().any(|a| a == settings::AUTOSTART_ARG);
            let shortcut_failed = app.state::<AppState>().shortcut_error.lock().unwrap().is_some();
            if !at_login || !is_ready(&handle) || shortcut_failed {
                show_pane(&handle);
            } else {
                settings::update_activation_policy(&handle);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running memeity");
}

#[cfg(test)]
mod tests {
    use super::is_app_url;

    #[test]
    fn only_the_app_itself_can_load() {
        let ok = |u: &str| is_app_url(&u.parse().unwrap());
        assert!(ok("tauri://localhost/index.html"));
        assert!(ok("http://tauri.localhost/index.html#duplicates"));
        assert!(!ok("file:///Users/me/memeity/memeity/cat.gif"));
        assert!(!ok("https://example.com/"));
        assert!(!ok("http://tauri.localhost.evil.example/"));
        assert_eq!(ok("http://localhost:1420/"), cfg!(debug_assertions));
    }
}
