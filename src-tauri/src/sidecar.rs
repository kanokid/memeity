//! Per-meme metadata that syncs alongside the library, at `<root>/.meta/<md5>.json`.
//!
//! Sidecars are keyed by a hash of the image's *contents*, not its filename, so
//! renaming or moving a meme (on any device) never orphans its metadata, and the
//! same image added twice shares one sidecar.
//!
//! Schema (version 1):
//! ```json
//! {
//!   "version": 1,
//!   "tags": ["reaction", "cat"],          // reserved for milestone 2, omitted while empty
//!   "ocr": { "text": "…", "engine": "windows.media.ocr", "lang": "en-US", "at": "2026-…Z" },
//!   "pinned": true                         // omitted unless pinned
//! }
//! ```
//! Unknown fields are preserved on rewrite, so an older memeity never drops data a
//! newer one added.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const META_DIR: &str = ".meta";
pub const VERSION: u32 = 1;
/// Scratch space for atomic writes. Dot-prefixed, so it's never indexed or synced.
const TMP_DIR: &str = ".memeity-tmp";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    #[serde(default = "current_version")]
    pub version: u32,
    /// User-applied tags (milestone 2). Indexed for search already, just never set yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Text found in the image, so other devices never re-run OCR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr: Option<Ocr>,
    /// Pinned to the top of the pane (on every device).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Anything a newer version wrote that we don't know about. Kept as-is.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ocr {
    pub text: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// When the text was recognized (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// The user typed/corrected this text. OCR (re-reads, engine upgrades) never
    /// overwrites it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
}

fn current_version() -> u32 {
    VERSION
}

impl Default for Sidecar {
    fn default() -> Self {
        Sidecar { version: VERSION, tags: Vec::new(), ocr: None, pinned: false, extra: serde_json::Map::new() }
    }
}

pub fn path(root: &Path, md5: &str) -> PathBuf {
    root.join(META_DIR).join(format!("{md5}.json"))
}

/// True for a sidecar's library-relative path, e.g. `.meta/<md5>.json`.
pub fn is_sidecar_rel(rel: &str) -> bool {
    rel.strip_prefix(META_DIR).and_then(|r| r.strip_prefix('/')).is_some_and(|name| {
        !name.contains('/') && name.ends_with(".json")
    })
}

pub fn read(root: &Path, md5: &str) -> Option<Sidecar> {
    let bytes = std::fs::read(path(root, md5)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes atomically (temp file + rename), so sync never uploads a half-written file.
pub fn write(root: &Path, md5: &str, sidecar: &Sidecar) -> std::io::Result<()> {
    let dest = path(root, md5);
    std::fs::create_dir_all(dest.parent().expect("sidecar path has a parent"))?;
    let tmp_dir = root.join(TMP_DIR);
    std::fs::create_dir_all(&tmp_dir)?;
    // Unique per process: two memeity processes never share a temp file.
    let tmp = tmp_dir.join(format!("{md5}.json.{}.part", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(sidecar).expect("sidecar serializes"))?;
    std::fs::rename(&tmp, &dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_unknown_fields() {
        let json = r#"{"version":1,"ocr":{"text":"hi","engine":"x"},"stars":5}"#;
        let mut sc: Sidecar = serde_json::from_str(json).unwrap();
        assert_eq!(sc.ocr.as_ref().unwrap().text, "hi");
        sc.tags.push("cat".into());
        let out = serde_json::to_value(&sc).unwrap();
        assert_eq!(out["stars"], 5, "future fields survive a rewrite");
        assert_eq!(out["tags"][0], "cat");
    }

    #[test]
    fn empty_tags_are_omitted_and_missing_fields_default() {
        let sc: Sidecar = serde_json::from_str("{}").unwrap();
        assert_eq!(sc.version, VERSION);
        let out = serde_json::to_string(&sc).unwrap();
        assert!(!out.contains("tags") && !out.contains("ocr"), "{out}");
    }

    #[test]
    fn recognizes_sidecar_paths() {
        assert!(is_sidecar_rel(".meta/abc.json"));
        assert!(!is_sidecar_rel(".meta/sub/abc.json"));
        assert!(!is_sidecar_rel("memes/abc.json"));
        assert!(!is_sidecar_rel(".metadata/abc.json"));
    }

    #[test]
    fn writes_atomically_and_reads_back() {
        let root = std::env::temp_dir().join(format!("memeity-sidecar-{}", std::process::id()));
        let sc = Sidecar {
            ocr: Some(Ocr { text: "when the".into(), engine: "test".into(), lang: None, at: None, edited: false }),
            ..Default::default()
        };
        write(&root, "deadbeef", &sc).unwrap();
        assert_eq!(read(&root, "deadbeef").unwrap().ocr.unwrap().text, "when the");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
