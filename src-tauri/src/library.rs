//! In-memory index of the meme folder plus fzf-style fuzzy search via nucleo.
//!
//! Indexes the local mirror of the Drive folder. Milestone 1 only searches
//! filenames, so a plain Vec rescanned from disk is enough and stays
//! sub-millisecond. The per-device SQLite/FTS5 index from the handoff arrives
//! with tags (M2) and OCR text (M3).

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use walkdir::{DirEntry, WalkDir};

/// Still images only for now. GIFs are indexed, but copying one gives a static
/// first frame (GIF/video support is an open decision in the handoff).
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

pub struct Meme {
    pub path: PathBuf,
    /// Path relative to the library root, `/`-separated, e.g. `reactions/this is fine.png`.
    pub rel: String,
    /// What the fuzzy matcher scores against: `rel` without the extension.
    haystack: String,
    pub modified: SystemTime,
}

#[derive(Default)]
pub struct Library {
    pub root: Option<PathBuf>,
    /// Sorted newest-first, so an empty query shows recent memes and ties keep that order.
    pub memes: Vec<Meme>,
}

#[derive(Serialize)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub rel: String,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    /// Matches before `limit` was applied.
    pub total: usize,
    /// Time spent matching, so we can keep an eye on "the pane being fast".
    pub micros: u128,
}

impl From<&Meme> for Hit {
    fn from(m: &Meme) -> Self {
        Hit {
            path: m.path.to_string_lossy().into_owned(),
            name: m
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            rel: m.rel.clone(),
        }
    }
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry.depth() > 0 && entry.file_name().to_string_lossy().starts_with('.')
}

pub fn is_meme_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(ext)))
}

fn is_meme(path: &Path) -> bool {
    path.file_name().is_some_and(|n| is_meme_name(&n.to_string_lossy()))
}

impl Library {
    pub fn scan(root: &Path) -> Library {
        let mut memes: Vec<Meme> = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            // Skip dot-folders (e.g. a future `.meta/`) and dotfiles.
            .filter_entry(|e| !is_hidden(e))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file() && is_meme(e.path()))
            .map(|e| {
                let path = e.path().to_path_buf();
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let haystack = match rel.rfind('.') {
                    Some(dot) => rel[..dot].to_string(),
                    None => rel.clone(),
                };
                let modified = e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(UNIX_EPOCH);
                Meme { path, rel, haystack, modified }
            })
            .collect();
        memes.sort_by(|a, b| b.modified.cmp(&a.modified));
        Library { root: Some(root.to_path_buf()), memes }
    }

    pub fn search(&self, query: &str, limit: usize, matcher: &mut Matcher) -> SearchResult {
        let start = Instant::now();
        let query = query.trim();

        if query.is_empty() {
            return SearchResult {
                hits: self.memes.iter().take(limit).map(Hit::from).collect(),
                total: self.memes.len(),
                micros: start.elapsed().as_micros(),
            };
        }

        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize)> = self
            .memes
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                pattern
                    .score(Utf32Str::new(&m.haystack, &mut buf), matcher)
                    .map(|score| (score, i))
            })
            .collect();
        // Stable sort: equal scores keep newest-first order.
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        SearchResult {
            total: scored.len(),
            hits: scored
                .into_iter()
                .take(limit)
                .map(|(_, i)| Hit::from(&self.memes[i]))
                .collect(),
            micros: start.elapsed().as_micros(),
        }
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.memes.iter().any(|m| m.path == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_and_fuzzy_searches() {
        let root = std::env::temp_dir().join(format!("memeity-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("reactions")).unwrap();
        std::fs::create_dir_all(root.join(".memeity-tmp")).unwrap();
        for f in ["this is fine.png", "reactions/surprised pikachu.jpg", "notes.txt", ".memeity-tmp/partial.png"] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        let lib = Library::scan(&root);
        assert_eq!(lib.memes.len(), 2, "skips non-images and dot-folders");

        let mut m = Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths());
        let res = lib.search("spika", 10, &mut m);
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].rel, "reactions/surprised pikachu.jpg");
        assert_eq!(lib.search("", 10, &mut m).hits.len(), 2);
        assert_eq!(lib.search("zzzz", 10, &mut m).hits.len(), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
