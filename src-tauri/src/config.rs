//! Per-device settings. Lives in the OS app-config dir, never in the synced folder.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveFolder {
    pub id: String,
    pub name: String,
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
        std::fs::write(Self::file(dir), json)
    }
}
