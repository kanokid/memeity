//! memeity: milestone 1 (hotkey pane + fuzzy filename search + copy to clipboard),
//! backed by two-way sync with one Google Drive folder.

mod clipboard;
mod config;
mod fuzzytext;
mod google;
mod index;
mod insert;
mod library;
mod ocr;
mod settings;
mod sidecar;
mod sync;
mod textindex;
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
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State, WebviewWindow, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

const PANE: &str = "pane";
const CLIENT_FILE: &str = "google-client.json";
/// Fixed content size of the window (Raycast's default), matching tauri.conf.json.
const PANE_WIDTH: f64 = 750.0;
const PANE_HEIGHT: f64 = 475.0;

pub struct AppState {
    pub library: RwLock<Library>,
    pub google: Google,
    pub sync_tx: tokio::sync::mpsc::UnboundedSender<()>,
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
    config: Mutex<Config>,
    matcher: Mutex<Matcher>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    shortcut_error: Mutex<Option<String>>,
    /// Set while a native dialog is open so the pane doesn't hide on blur.
    dialog_open: AtomicBool,
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
            eprintln!("[memeity] couldn't save config: {e}");
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
    if ready {
        position_pane(app, &win);
    } else {
        let _ = win.center();
    }
}

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

/// Index the local mirror, watch it, and let the webview load thumbnails from it.
fn load_library(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some((_, dir)) = sync_target(app) else {
        *state.library.write().unwrap() = Library::default();
        *state.watcher.lock().unwrap() = None;
        let _ = app.emit("library-changed", ());
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[memeity] couldn't create {}: {e}", dir.display());
    }
    if let Err(e) = app.asset_protocol_scope().allow_directory(&dir, true) {
        eprintln!("[memeity] couldn't allow asset scope: {e}");
    }
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
        .inspect_err(|e| eprintln!("[memeity] couldn't open search index: {e}"))
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

#[tauri::command]
fn search(state: State<'_, AppState>, query: String, limit: usize) -> SearchResult {
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
    let lib = state.library.read().unwrap();
    let mut matcher = state.matcher.lock().unwrap();
    lib.search(&query, limit, &mut matcher, &text_bonus)
}

#[tauri::command]
async fn copy_meme(app: AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let known = app.state::<AppState>().library.read().unwrap().contains(&path);
    if !known {
        return Err("That file isn't in your meme folder.".into());
    }
    clipboard::copy(&path)?;
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
}

/// What OCR found in a meme, from its sidecar.
#[tauri::command]
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
    let pending = MemeText { state: "pending", text: None, lang: None, at: None };
    let Some(entry) = entry else { return Ok(pending) };
    match sidecar::read(&root, &entry.md5).and_then(|sc| sc.ocr) {
        Some(ocr) => Ok(MemeText { state: "read", text: Some(ocr.text), lang: ocr.lang, at: ocr.at }),
        // Indexed without a sidecar means the image couldn't be decoded/read.
        None if entry.indexed => Ok(MemeText { state: "failed", ..pending }),
        None => Ok(pending),
    }
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
    tauri::async_runtime::spawn_blocking(move || insert::insert(&handle, &path, target))
        .await
        .map_err(|e| e.to_string())?
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

/// Trash a meme on Drive and remove it from this device.
#[tauri::command]
async fn delete_meme(app: AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let known = app.state::<AppState>().library.read().unwrap().contains(&path);
    if !known {
        return Err("That file isn't in your meme folder.".into());
    }
    sync::delete(&app, &path).await?;
    refresh_library(&app);
    Ok(())
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
    let sync_now = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
    let local = MenuItem::with_id(app, "local", "Open local meme folder", true, None::<&str>)?;
    let signout = MenuItem::with_id(app, "signout", "Sign out of Google", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit memeity", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &settings_item, &sep1, &sync_now, &local, &signout, &sep2, &quit])?;

    *app.state::<AppState>().open_item.lock().unwrap() = Some(open.clone());

    tray_builder(app)?
        .tooltip("memeity")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_pane(app),
            "settings" => settings::open(app),
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
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
            insert_meme,
            reveal_meme,
            delete_meme,
            load_client_file,
            sign_in,
            sign_out_cmd,
            setup_drive_folder,
            sync_now,
            open_folder,
            open_setup_link,
            hide,
            settings::open_settings,
            settings::set_shortcut,
            settings::reset_shortcut,
            settings::set_autostart,
            settings::set_primary_action,
            settings::reread_all_text,
            settings::open_link
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let config_dir = app.path().app_config_dir()?;
            // memeity's built-in client; a user-supplied one only for builds without it.
            let creds = google::embedded_client().or_else(|| {
                std::fs::read(config_dir.join(CLIENT_FILE))
                    .ok()
                    .and_then(|b| google::parse_client_json(&b).ok())
            });

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
                config: Mutex::new(Config::load(&config_dir)),
                matcher: Mutex::new(Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths())),
                watcher: Mutex::new(None),
                shortcut_error: Mutex::new(None),
                dialog_open: AtomicBool::new(false),
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
                        hide_pane(&h);
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
            sync::trigger(&handle);

            // Opened by hand: show the pane so it's obvious memeity is running. Started at
            // login: stay quietly in the tray/menu bar until the shortcut is pressed.
            let at_login = std::env::args().any(|a| a == settings::AUTOSTART_ARG);
            if !at_login || !is_ready(&handle) {
                show_pane(&handle);
            } else {
                settings::update_activation_policy(&handle);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running memeity");
}
