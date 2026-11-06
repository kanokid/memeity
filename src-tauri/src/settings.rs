//! The Settings window (a normal, decorated window, unlike the pane) and the
//! per-device preferences it controls: the global shortcut, launch at login,
//! re-reading text, and links out.

#[cfg(target_os = "macos")]
use crate::is_ready;
use crate::{config, hide_pane, status, textindex, update_config, AppState, Status};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Modifiers, Shortcut};
use tauri_plugin_opener::OpenerExt;

pub const WINDOW: &str = "settings";
/// Ctrl+Alt+M (Ctrl+Option+M on macOS).
pub const DEFAULT_SHORTCUT: &str = "control+alt+KeyM";
/// Passed by the login item so a login launch stays quietly in the tray.
pub const AUTOSTART_ARG: &str = "--autostart";

// ---------- window ----------

/// Opens the Settings window, or focuses it if it's already open.
pub fn open(app: &AppHandle) {
    open_at(app, None);
}

/// Opens Settings at a section (e.g. "duplicates"), or wherever it was.
pub fn open_at(app: &AppHandle, section: Option<&str>) {
    if let Some(win) = app.get_webview_window(WINDOW) {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        if let Some(section) = section {
            let _ = app.emit_to(WINDOW, "show-section", section);
        }
        return;
    }
    hide_pane(app);
    app.state::<AppState>().settings_open.store(true, Ordering::SeqCst);
    update_activation_policy(app);

    // The section rides along as the URL fragment (read once at startup).
    let url = match section {
        Some(section) => format!("index.html#{section}"),
        None => "index.html".into(),
    };
    let built = WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::App(url.into()))
        .title("memeity Settings")
        .inner_size(700.0, 500.0)
        .min_inner_size(620.0, 440.0)
        .center()
        .focused(true)
        .build();
    match built {
        Ok(win) => {
            let handle = app.clone();
            win.on_window_event(move |event| {
                if matches!(event, WindowEvent::Destroyed) {
                    handle.state::<AppState>().settings_open.store(false, Ordering::SeqCst);
                    update_activation_policy(&handle);
                }
            });
        }
        Err(e) => {
            log::warn!("couldn't open settings: {e}");
            app.state::<AppState>().settings_open.store(false, Ordering::SeqCst);
            update_activation_policy(app);
        }
    }
}

/// macOS: memeity is a menu-bar-only app (no Dock icon, not in Cmd+Tab) except while a
/// "real" window is up: the Settings window, or the pane in its setup/sign-in mode.
#[cfg(target_os = "macos")]
pub fn update_activation_policy(app: &AppHandle) {
    let regular = !is_ready(app) || app.state::<AppState>().settings_open.load(Ordering::SeqCst);
    let _ = app.set_activation_policy(if regular {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    });
    if regular {
        set_dock_icon(app);
    }
}

/// Switching to a regular (Dock) app makes macOS show the bundle's icon. Under
/// `tauri dev` there is no .app bundle, so that's the generic "exec" icon; Tauri only
/// fixes it once at startup. Re-apply memeity's logo after every switch. (Harmless in
/// packaged builds, whose bundle icon is already the logo.)
#[cfg(target_os = "macos")]
fn set_dock_icon(app: &AppHandle) {
    // Queued after the activation-policy change, on the main thread AppKit requires.
    let _ = app.run_on_main_thread(|| {
        use objc2::{AllocAnyThread, MainThreadMarker};
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::NSData;

        let Some(mtm) = MainThreadMarker::new() else { return };
        let data = NSData::with_bytes(include_bytes!("../icons/icon.png"));
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else { return };
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    });
}

#[cfg(not(target_os = "macos"))]
pub fn update_activation_policy(_app: &AppHandle) {}

// ---------- shortcut ----------

/// The shortcut currently opening the pane.
pub fn current(app: &AppHandle) -> Shortcut {
    *app.state::<AppState>().shortcut.lock().unwrap()
}

/// Human-friendly label, e.g. "Ctrl+Alt+M" (or "Ctrl+Option+M" on macOS).
pub fn label(shortcut: &Shortcut) -> String {
    let mac = cfg!(target_os = "macos");
    let mut parts: Vec<String> = Vec::new();
    if shortcut.mods.contains(Modifiers::CONTROL) {
        parts.push("Ctrl".into());
    }
    if shortcut.mods.contains(Modifiers::ALT) {
        parts.push(if mac { "Option" } else { "Alt" }.into());
    }
    if shortcut.mods.contains(Modifiers::SHIFT) {
        parts.push("Shift".into());
    }
    if shortcut.mods.contains(Modifiers::SUPER) {
        parts.push(if mac { "Cmd" } else { "Win" }.into());
    }
    let code = shortcut.key.to_string();
    let key = match code.as_str() {
        "ArrowUp" => "↑".to_string(),
        "ArrowDown" => "↓".to_string(),
        "ArrowLeft" => "←".to_string(),
        "ArrowRight" => "→".to_string(),
        "Backquote" => "`".to_string(),
        "Comma" => ",".to_string(),
        "Period" => ".".to_string(),
        "Slash" => "/".to_string(),
        "Semicolon" => ";".to_string(),
        "Quote" => "'".to_string(),
        "Minus" => "-".to_string(),
        "Equal" => "=".to_string(),
        "BracketLeft" => "[".to_string(),
        "BracketRight" => "]".to_string(),
        "Backslash" => "\\".to_string(),
        other => other.strip_prefix("Key").or_else(|| other.strip_prefix("Digit")).unwrap_or(other).to_string(),
    };
    parts.push(key);
    parts.join("+")
}

fn parse(accelerator: &str) -> Result<Shortcut, String> {
    let shortcut: Shortcut = accelerator.parse().map_err(|e| format!("That's not a valid shortcut: {e}"))?;
    let has_main_modifier = shortcut.mods.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER);
    if !has_main_modifier {
        return Err("Include Ctrl, Alt or Win (Cmd/Option on Mac), so it doesn't clash with typing.".into());
    }
    Ok(shortcut)
}

/// Registers the saved (or default) shortcut at startup.
pub fn register_initial(app: &AppHandle) {
    let saved = config(app).shortcut.and_then(|s| parse(&s).ok());
    let shortcut = saved.unwrap_or_else(|| parse(DEFAULT_SHORTCUT).expect("default shortcut is valid"));
    *app.state::<AppState>().shortcut.lock().unwrap() = shortcut;
    if register(app, shortcut).is_err() {
        *app.state::<AppState>().shortcut_error.lock().unwrap() = Some(taken_message(&shortcut));
    }
    refresh_shortcut_labels(app);
}

/// Makes `shortcut` open the pane: the normal global-hotkey way, or on Windows, for
/// combinations Windows won't hand out (Win+Space, Alt+Space...), memeity's own keyboard
/// hook (see winhook.rs), like Raycast.
fn register(app: &AppHandle, shortcut: Shortcut) -> Result<(), String> {
    match app.global_shortcut().register(shortcut) {
        Ok(()) => {
            #[cfg(windows)]
            crate::winhook::set_target(None);
            Ok(())
        }
        Err(e) => {
            #[cfg(windows)]
            if crate::winhook::set_target(Some(shortcut)) {
                log::info!("{} is reserved by Windows; memeity takes it over with its keyboard hook", label(&shortcut));
                return Ok(());
            }
            log::warn!("couldn't register {}: {e}", label(&shortcut));
            Err(e.to_string())
        }
    }
}

/// Stops `shortcut` opening the pane, however it was registered.
fn unregister(app: &AppHandle, shortcut: Shortcut) {
    let _ = app.global_shortcut().unregister(shortcut);
    #[cfg(windows)]
    crate::winhook::set_target(None);
}

/// Shown when a shortcut can't be registered (almost always: another app owns it).
fn taken_message(shortcut: &Shortcut) -> String {
    let os = if cfg!(target_os = "macos") { "macOS" } else { "Windows" };
    format!("{} is already used by another app or {os}, so it can't open memeity.", label(shortcut))
}

fn registered(app: &AppHandle) -> bool {
    app.state::<AppState>().shortcut_error.lock().unwrap().is_none()
}

/// Swaps in a new shortcut. If it can't be registered, the old one stays active.
fn apply(app: &AppHandle, next: Shortcut) -> Result<(), String> {
    let old = current(app);
    // The current shortcut may never have registered (taken at startup); then
    // "applying" it again is a retry, not a no-op.
    let old_registered = registered(app);
    if next == old && old_registered {
        return Ok(());
    }
    if old_registered {
        unregister(app, old);
    }
    if register(app, next).is_err() {
        if old_registered {
            let _ = register(app, old);
        }
        return Err(format!("{} Try another combination.", taken_message(&next)));
    }
    *app.state::<AppState>().shortcut.lock().unwrap() = next;
    *app.state::<AppState>().shortcut_error.lock().unwrap() = None;
    update_config(app, |c| c.shortcut = Some(next.into_string()));
    refresh_shortcut_labels(app);
    let _ = app.emit("status-changed", ());
    Ok(())
}

/// The tray menu and tooltip mention the shortcut; keep them in sync.
fn refresh_shortcut_labels(app: &AppHandle) {
    let text = label(&current(app));
    if let Some(item) = app.state::<AppState>().open_item.lock().unwrap().as_ref() {
        let _ = item.set_text(format!("Open memeity ({text})"));
    }
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(format!("memeity ({text})")));
    }
}

// ---------- commands ----------

#[tauri::command]
pub async fn open_settings(app: AppHandle, section: Option<String>) {
    open_at(&app, section.as_deref());
}

/// `accelerator` uses the browser's key codes, e.g. "control+alt+KeyM".
#[tauri::command]
pub fn set_shortcut(app: AppHandle, accelerator: String) -> Result<Status, String> {
    apply(&app, parse(&accelerator)?)?;
    Ok(status(&app))
}

/// Settings started recording a shortcut. On Windows, memeity then captures the next
/// combination system-wide, so ones Windows would act on itself (Alt+Space, Win+...) can
/// be recorded too; it arrives as a `shortcut-captured` event. Returns whether it does
/// (otherwise Settings reads the keys itself).
#[tauri::command]
pub fn start_shortcut_capture() -> bool {
    #[cfg(windows)]
    return crate::winhook::start_capture();
    #[cfg(not(windows))]
    false
}

#[tauri::command]
pub fn stop_shortcut_capture() {
    #[cfg(windows)]
    crate::winhook::stop_capture();
}

/// Tries the current shortcut again (e.g. after quitting the app that had it).
#[tauri::command]
pub fn retry_shortcut(app: AppHandle) -> Result<Status, String> {
    apply(&app, current(&app))?;
    Ok(status(&app))
}

#[tauri::command]
pub fn reset_shortcut(app: AppHandle) -> Result<Status, String> {
    apply(&app, parse(DEFAULT_SHORTCUT).expect("default shortcut is valid"))?;
    Ok(status(&app))
}

/// What Enter / click does in the pane. Defaults to pasting into the app you were in.
pub fn primary_action(app: &AppHandle) -> &'static str {
    match config(app).primary_action.as_deref() {
        Some("copy") => "copy",
        _ => "paste",
    }
}

#[tauri::command]
pub fn set_primary_action(app: AppHandle, action: String) -> Result<Status, String> {
    if action != "paste" && action != "copy" {
        return Err("Unknown action.".into());
    }
    update_config(&app, |c| c.primary_action = Some(action));
    Ok(status(&app))
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<Status, String> {
    let launcher = app.autolaunch();
    let result = if enabled { launcher.enable() } else { launcher.disable() };
    result.map_err(|e| format!("Couldn't change launch at login: {e}"))?;
    Ok(status(&app))
}

pub fn autostart_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Re-run text recognition on every meme (e.g. after installing a new OCR language).
#[tauri::command]
pub fn reread_all_text(app: AppHandle) {
    app.state::<AppState>().reread_text.store(true, Ordering::SeqCst);
    textindex::trigger(&app);
}

/// Opens one of a fixed set of links in the browser (never an arbitrary URL).
#[tauri::command]
pub fn open_link(app: AppHandle, target: String) -> Result<(), String> {
    let url = match target.as_str() {
        "website" => "https://memeity.stelle.codes".to_string(),
        "privacy" => "https://memeity.stelle.codes/privacy/".to_string(),
        "source" => "https://github.com/purestellenium/memeity".to_string(),
        "drive" => {
            let folder = config(&app).drive_folder.ok_or("No Drive folder yet.")?;
            format!("https://drive.google.com/drive/folders/{}", folder.id)
        }
        _ => return Err("Unknown link.".into()),
    };
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_browser_key_codes_and_labels_them() {
        let s = parse("control+alt+KeyM").unwrap();
        assert_eq!(s, parse(DEFAULT_SHORTCUT).unwrap());
        let expected = if cfg!(target_os = "macos") { "Ctrl+Option+M" } else { "Ctrl+Alt+M" };
        assert_eq!(label(&s), expected);
        assert_eq!(label(&parse("control+shift+Digit1").unwrap()), "Ctrl+Shift+1");
        assert_eq!(label(&parse("alt+Space").unwrap()).rsplit('+').next(), Some("Space"));
        assert_eq!(label(&parse("control+ArrowUp").unwrap()), "Ctrl+↑");
    }

    #[test]
    fn rejects_shortcuts_that_would_clash_with_typing() {
        assert!(parse("KeyM").is_err());
        assert!(parse("shift+KeyM").is_err());
        assert!(parse("nonsense+KeyM").is_err());
    }
}
