//! Self-updating from GitHub Releases (Tauri's updater plugin).
//!
//! Release builds check shortly after launch and then every few hours. When a newer
//! version is published, the tray menu offers "Install Update (vX.Y.Z)…" and Settings >
//! About shows an Update button. Installing downloads the new version, verifies its
//! signature against the public key in tauri.conf.json, installs it, and restarts.
//!
//! The manifest (`latest.json`) and signed installers are produced by the release
//! workflow (.github/workflows/release.yml). Drafts and pre-releases are never offered.

use crate::AppState;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

/// First automatic check, a little after launch (don't slow startup).
const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Starts the periodic background check (release builds only: dev builds have no
/// published version to compare against).
pub fn spawn(app: AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            if let Err(e) = check(&app).await {
                log::warn!("update check failed: {e}");
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

/// Asks GitHub for a newer version. Returns it (e.g. "0.2.0") if there is one.
pub async fn check(app: &AppHandle) -> Result<Option<String>, String> {
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    let version = update.map(|u| u.version);
    *app.state::<AppState>().update.lock().unwrap() = version.clone();
    refresh_tray_item(app);
    let _ = app.emit("update-status", ());
    Ok(version)
}

/// The tray's update item reflects whether an update is waiting.
pub fn refresh_tray_item(app: &AppHandle) {
    let state = app.state::<AppState>();
    let text = match state.update.lock().unwrap().as_deref() {
        Some(version) => format!("Install Update ({version})…"),
        None => "Check for Updates…".to_string(),
    };
    if let Some(item) = state.update_item.lock().unwrap().as_ref() {
        let _ = item.set_text(text);
    }
}

/// Downloads, verifies and installs the latest version, then restarts memeity.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("memeity is already up to date.")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("Couldn't install the update: {e}"))?;
    // Windows exits on its own to run the installer; macOS needs a relaunch.
    app.restart();
}

/// Tray "Check for Updates…" / "Install Update…".
pub fn tray_clicked(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let waiting = app.state::<AppState>().update.lock().unwrap().is_some();
        let result = if waiting {
            install(&app).await
        } else {
            match check(&app).await {
                // Found one: the item now reads "Install Update (x)…"; open Settings to show it.
                Ok(Some(_)) => {
                    crate::settings::open(&app);
                    Ok(())
                }
                Ok(None) => {
                    if let Some(item) = app.state::<AppState>().update_item.lock().unwrap().as_ref() {
                        let _ = item.set_text(format!("Up to Date ({})", app.package_info().version));
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };
        if let Err(e) = result {
            log::warn!("update: {e}");
        }
    });
}

#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Option<String>, String> {
    check(&app).await
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    install(&app).await
}
