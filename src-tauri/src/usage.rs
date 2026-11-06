//! Frecency ("recently and often used first") shared across devices.
//!
//! Each device keeps its own uses in the per-device index (`usage` table) and publishes
//! them to `<root>/.meta/usage-<device>.json`, which syncs through Drive like a sidecar.
//! A device only ever writes its own file, so two devices never edit the same file and
//! nothing can conflict. Every device reads the others' files into the index
//! (`remote_usage`), and ranking adds them all up.
//!
//! Scores are stored raw (score + when it was last bumped); whoever reads them applies
//! the decay, so an old file from a device that's gone quietly fades to nothing.
//!
//! File format (version 1):
//! ```json
//! { "version": 1, "device": "3f9c2a71b8d0",
//!   "uses": { "<md5 of the image>": { "score": 2.7, "last_used": 1793935817389 } } }
//! ```

use crate::config::write_atomic;
use crate::sidecar::META_DIR;
use crate::{config, sync, sync_target, update_config, AppState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Manager};

const VERSION: u32 = 1;
const PREFIX: &str = "usage-";
/// Uses are published this long after the first one in a burst (one upload, not one
/// per paste).
const FLUSH_DELAY: Duration = Duration::from_secs(30);

static FLUSH_PENDING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Serialize, Deserialize)]
pub struct UsageFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub uses: BTreeMap<String, Use>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Use {
    pub score: f64,
    pub last_used: i64,
}

/// This device's id: random, made once, kept in the per-device config.
pub fn device_id(app: &AppHandle) -> String {
    if let Some(id) = config(app).device_id {
        return id;
    }
    let id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
    update_config(app, |c| {
        c.device_id.get_or_insert_with(|| id.clone());
    });
    config(app).device_id.unwrap_or(id)
}

fn file_path(root: &Path, device: &str) -> PathBuf {
    root.join(META_DIR).join(format!("{PREFIX}{device}.json"))
}

/// The device a `.meta/` file name belongs to, if it's a usage file.
fn device_of(file_name: &str) -> Option<&str> {
    file_name
        .strip_prefix(PREFIX)?
        .strip_suffix(".json")
        .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// A meme was used: publish this device's uses shortly (and sync them up).
pub fn schedule_flush(app: &AppHandle) {
    if FLUSH_PENDING.swap(true, Ordering::SeqCst) {
        return; // already on its way
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FLUSH_DELAY).await;
        FLUSH_PENDING.store(false, Ordering::SeqCst);
        if flush(&app) {
            sync::trigger(&app);
        }
    });
}

/// Writes this device's usage file if it changed. Returns true if it wrote it.
pub fn flush(app: &AppHandle) -> bool {
    let Some((_, root)) = sync_target(app) else { return false };
    let device = device_id(app);
    let uses = {
        let state = app.state::<AppState>();
        let index = state.index.lock().unwrap();
        let Some(idx) = index.as_ref() else { return false };
        match idx.own_usage(crate::now_ms()) {
            Ok(rows) => rows,
            Err(e) => {
                log::warn!("couldn't read usage: {e}");
                return false;
            }
        }
    };
    let path = file_path(&root, &device);
    if uses.is_empty() && !path.exists() {
        return false;
    }
    let file = UsageFile { version: VERSION, device, uses };
    let bytes = serde_json::to_vec(&file).expect("usage serializes");
    if std::fs::read(&path).is_ok_and(|old| old == bytes) {
        return false;
    }
    let written = std::fs::create_dir_all(path.parent().expect("has a parent")).and_then(|_| write_atomic(&path, &bytes));
    match written {
        Ok(()) => true,
        Err(e) => {
            log::warn!("couldn't write {}: {e}", path.display());
            false
        }
    }
}

/// Reads the other devices' usage files into the index. Called at startup and after
/// a sync brought something new.
pub fn load_remote(app: &AppHandle) {
    let Some((_, root)) = sync_target(app) else { return };
    let own = device_id(app);
    let mut files: Vec<(String, BTreeMap<String, Use>)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join(META_DIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(device) = device_of(&name) else { continue };
            if device == own {
                continue;
            }
            match std::fs::read(entry.path()).map(|b| serde_json::from_slice::<UsageFile>(&b)) {
                Ok(Ok(file)) => files.push((device.to_string(), file.uses)),
                Ok(Err(e)) => log::warn!("skipping unreadable {name}: {e}"),
                Err(_) => {}
            }
        }
    }
    let state = app.state::<AppState>();
    let index = state.index.lock().unwrap();
    let Some(idx) = index.as_ref() else { return };
    let devices: HashSet<String> = files.iter().map(|(d, _)| d.clone()).collect();
    for (device, uses) in &files {
        if let Err(e) = idx.set_remote_usage(device, uses) {
            log::warn!("couldn't load usage from {device}: {e}");
        }
    }
    if let Err(e) = idx.retain_remote_devices(&devices) {
        log::warn!("couldn't tidy remote usage: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_usage_files() {
        assert_eq!(device_of("usage-3f9c2a71b8d0.json"), Some("3f9c2a71b8d0"));
        assert_eq!(device_of("usage-.json"), None);
        assert_eq!(device_of("usage-../x.json"), None);
        assert_eq!(device_of("0123456789abcdef0123456789abcdef.json"), None);
    }

    #[test]
    fn reads_files_with_missing_fields() {
        let f: UsageFile = serde_json::from_str(r#"{"uses":{"abc":{"score":1.5,"last_used":5}}}"#).unwrap();
        assert_eq!(f.uses["abc"], Use { score: 1.5, last_used: 5 });
    }
}
