//! Per-device settings. Lives in the OS app-config dir, never in the synced folder.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveFolder {
    pub id: String,
    pub name: String,
}

/// A colour theme choice. The themes themselves live in the frontend (src/themes.ts),
/// which falls back to defaults for anything it doesn't know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    pub theme: String,
    #[serde(default)]
    pub variant: Option<String>,
    /// None = the theme's default accent; "" = no accent (monochrome).
    #[serde(default)]
    pub accent: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Signed-in Google account, for display only. The token lives in the OS keychain.
    pub account: Option<String>,
    /// The Google Drive folder memeity mirrors.
    pub drive_folder: Option<DriveFolder>,
    /// The Drive access level this device's sign-in was granted. If memeity's scope
    /// changes, the sign-in is dropped and redone (see `migrate_auth_scope`).
    #[serde(default)]
    pub auth_scope: Option<String>,
    /// The pane's global shortcut, e.g. "control+alt+KeyM". None = the default.
    #[serde(default)]
    pub shortcut: Option<String>,
    /// What Enter (and a click) does in the pane: "paste" (default) or "copy".
    /// Shift+Enter / Shift+click does the other one.
    #[serde(default)]
    pub primary_action: Option<String>,
    /// The search pane's size, as a percentage of the default 750×475. None = 100.
    #[serde(default)]
    pub pane_scale: Option<u32>,
    /// Seconds the pane can stay closed before it reopens scrolled back to the top.
    /// 0 = every time. None = the default (60).
    #[serde(default)]
    pub scroll_reset_secs: Option<u32>,
    /// Colour theme (see src/themes.ts). None = memeity's own, following the system.
    #[serde(default)]
    pub theme: Option<Theme>,
    /// Random id for this device; names its usage file in the synced folder (usage.rs).
    #[serde(default)]
    pub device_id: Option<String>,
}

impl Config {
    fn file(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    pub fn load(dir: &Path) -> Self {
        std::fs::read(Self::file(dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(self).expect("config serializes");
        write_atomic(&Self::file(dir), &json)
    }
}

/// Writes `bytes` to a temp file next to `dest`, then renames it into place, so a crash
/// mid-write never leaves a half-written (unreadable) file behind.
pub fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Unique per process: two memeity processes never share a temp file.
    let tmp = dest.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dest)
}
