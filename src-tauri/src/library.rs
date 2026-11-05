//! In-memory index of the meme folder plus fzf-style fuzzy search via nucleo.
//!
//! Indexes the local mirror of the Drive folder. Filenames are fuzzy-matched here
//! in memory (sub-millisecond); text found in images (and tags, later) comes from the
//! SQLite FTS5 index (`index.rs`) and is merged into the ranking by `search`.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use walkdir::{DirEntry, WalkDir};

/// Image formats only (no video yet). Animated GIF/WebP copy as files so they keep
/// their animation; see `clipboard.rs`.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

pub struct Meme {
    pub path: PathBuf,
    /// Path relative to the library root, `/`-separated, e.g. `reactions/this is fine.png`.
    pub rel: String,
    /// What the fuzzy matcher scores against: `rel` without the extension.
    haystack: String,
    pub modified: SystemTime,
    /// Pixel size, read from the file header. Lets the pane lay out its masonry grid
    /// before images load. `None` until `fill_dims` runs (or if the header is unreadable).
    dims: Option<(u32, u32)>,
    /// GIF / animated WebP. The grid shows a still thumbnail and animates on hover.
    animated: bool,
}

/// Score bonus for memes whose image text matched (nucleo filename scores for a good
/// match are typically ~50-300, so a text hit competes with a decent filename hit).
/// Word matches from the FTS index get BASE + SPAN for the best hit, tapering to BASE.
const TEXT_BONUS_BASE: u32 = 100;
const TEXT_BONUS_SPAN: u32 = 100;

/// Bonus for a loose ("squashed", typo-tolerant) text match with `typos` edits: an exact
/// squashed match ranks with the best word match; each typo costs a bit.
pub fn fuzzy_text_bonus(typos: usize) -> u32 {
    match typos {
        0 => TEXT_BONUS_BASE + TEXT_BONUS_SPAN,
        1 => TEXT_BONUS_BASE + TEXT_BONUS_SPAN / 2,
        2 => TEXT_BONUS_BASE + TEXT_BONUS_SPAN / 5,
        _ => TEXT_BONUS_BASE,
    }
}

/// Search boost for a used meme: enough to break ties and lift favourites a little,
/// never enough to beat a clearly better match (good matches score in the hundreds).
const USAGE_BOOST_MAX: u32 = 60;

fn usage_boost(score: f64) -> u32 {
    ((score * 15.0) as u32).min(USAGE_BOOST_MAX)
}

/// Text-match bonuses per meme: word matches from FTS (`ranked`, best first) and loose
/// matches (`loose`: rel, typos). A meme keeps its best bonus.
pub fn text_bonuses(ranked: &[String], loose: &[(String, usize)]) -> HashMap<String, u32> {
    let n = ranked.len().max(1) as u32;
    let mut bonus: HashMap<String, u32> = ranked
        .iter()
        .enumerate()
        .map(|(i, rel)| (rel.clone(), TEXT_BONUS_BASE + TEXT_BONUS_SPAN * (n - i as u32) / n))
        .collect();
    for (rel, typos) in loose {
        let b = fuzzy_text_bonus(*typos);
        bonus.entry(rel.clone()).and_modify(|v| *v = (*v).max(b)).or_insert(b);
    }
    bonus
}

/// Previously-read dimensions and animation flags, keyed by path, valid while the mtime
/// is unchanged.
pub type DimsCache = HashMap<PathBuf, (SystemTime, (u32, u32), bool)>;

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
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Plays on hover/selection in the grid (which otherwise shows a still thumbnail).
    pub animated: bool,
    /// Changes whenever the file does (its modified time), so cached thumbnails refresh.
    pub version: u64,
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
            width: m.dims.map(|d| d.0),
            height: m.dims.map(|d| d.1),
            animated: m.animated,
            version: m.modified.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
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
                Meme { path, rel, haystack, modified, dims: None, animated: false }
            })
            .collect();
        memes.sort_by_key(|m| std::cmp::Reverse(m.modified));
        Library { root: Some(root.to_path_buf()), memes }
    }

    /// Snapshot of known dimensions, to carry over into a fresh scan.
    pub fn dims_cache(&self) -> DimsCache {
        self.memes
            .iter()
            .filter_map(|m| Some((m.path.clone(), (m.modified, m.dims?, m.animated))))
            .collect()
    }

    /// Fills in image dimensions, reusing `cache` for unchanged files and reading
    /// just the header (not the whole image) for new or changed ones.
    pub fn fill_dims(&mut self, cache: &DimsCache) {
        for m in &mut self.memes {
            (m.dims, m.animated) = match cache.get(&m.path) {
                Some((modified, dims, animated)) if *modified == m.modified => (Some(*dims), *animated),
                _ => (
                    image::ImageReader::open(&m.path)
                        .ok()
                        .and_then(|r| r.with_guessed_format().ok())
                        .and_then(|r| r.into_dimensions().ok()),
                    crate::clipboard::is_animated(&m.path),
                ),
            };
        }
    }

    /// `text_bonus` holds memes whose image text/tags matched the query (see
    /// `text_bonuses`). A meme can match by filename, by text, or both (ranks highest).
    /// Returns the page `offset..offset + limit` of the ranked results (the pane loads
    /// more pages as you scroll).
    ///
    /// `usage` holds recently/frequently used memes (decayed scores): with no query they
    /// come first; with one they get a modest boost.
    pub fn search(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
        matcher: &mut Matcher,
        text_bonus: &HashMap<String, u32>,
        usage: &HashMap<String, f64>,
    ) -> SearchResult {
        let start = Instant::now();
        let query = query.trim();

        if query.is_empty() {
            // Used memes first (most used first), then everything else newest-first.
            let mut used: Vec<(usize, f64)> = self
                .memes
                .iter()
                .enumerate()
                .filter_map(|(i, m)| usage.get(&m.rel).map(|&u| (i, u)))
                .collect();
            used.sort_by(|a, b| b.1.total_cmp(&a.1));
            let used_set: std::collections::HashSet<usize> = used.iter().map(|&(i, _)| i).collect();
            let order = used
                .iter()
                .map(|&(i, _)| i)
                .chain((0..self.memes.len()).filter(|i| !used_set.contains(i)));
            return SearchResult {
                hits: order.skip(offset).take(limit).map(|i| Hit::from(&self.memes[i])).collect(),
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
                let fuzzy = pattern.score(Utf32Str::new(&m.haystack, &mut buf), matcher);
                let text = text_bonus.get(&m.rel).copied();
                if fuzzy.is_none() && text.is_none() {
                    return None;
                }
                let boost = usage.get(&m.rel).map_or(0, |&u| usage_boost(u));
                Some((fuzzy.unwrap_or(0) + text.unwrap_or(0) + boost, i))
            })
            .collect();
        // Stable sort: equal scores keep newest-first order.
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));

        SearchResult {
            total: scored.len(),
            hits: scored
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|(_, i)| Hit::from(&self.memes[i]))
                .collect(),
            micros: start.elapsed().as_micros(),
        }
    }

    /// Display info for one meme, by library-relative path.
    pub fn hit_for(&self, rel: &str) -> Option<Hit> {
        self.memes.iter().find(|m| m.rel == rel).map(Hit::from)
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
        let res = lib.search("spika", 0, 10, &mut m, &HashMap::new(), &HashMap::new());
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].rel, "reactions/surprised pikachu.jpg");
        assert_eq!(lib.search("", 0, 10, &mut m, &HashMap::new(), &HashMap::new()).hits.len(), 2);
        assert_eq!(lib.search("zzzz", 0, 10, &mut m, &HashMap::new(), &HashMap::new()).hits.len(), 0);

        // A text-only match (filename doesn't match) still shows up...
        let res = lib.search("zzzz", 0, 10, &mut m, &text_bonuses(&["this is fine.png".to_string()], &[]), &HashMap::new());
        assert_eq!(res.hits.len(), 1);
        // ...and a text hit lifts a meme above a filename-only match.
        let res = lib.search("i", 0, 10, &mut m, &text_bonuses(&["reactions/surprised pikachu.jpg".to_string()], &[]), &HashMap::new());
        assert_eq!(res.hits[0].rel, "reactions/surprised pikachu.jpg");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn loose_and_word_text_matches_combine() {
        let ranked = vec!["a.png".to_string(), "b.png".to_string()];
        let loose = vec![("b.png".to_string(), 0), ("c.png".to_string(), 2)];
        let bonus = text_bonuses(&ranked, &loose);
        assert_eq!(bonus["a.png"], TEXT_BONUS_BASE + TEXT_BONUS_SPAN, "best word match");
        assert_eq!(bonus["b.png"], TEXT_BONUS_BASE + TEXT_BONUS_SPAN, "exact loose match lifts b");
        assert!(bonus["c.png"] < bonus["b.png"], "typos rank lower");
    }

    #[test]
    fn pages_through_results() {
        let root = std::env::temp_dir().join(format!("memeity-pages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..5 {
            std::fs::write(root.join(format!("cat {i}.png")), b"x").unwrap();
        }
        let lib = Library::scan(&root);
        let mut m = Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths());
        let none = HashMap::new();
        let first = lib.search("cat", 0, 2, &mut m, &none, &HashMap::new());
        let rest = lib.search("cat", 2, 10, &mut m, &none, &HashMap::new());
        assert_eq!((first.hits.len(), rest.hits.len(), first.total), (2, 3, 5));
        let all: Vec<_> = first.hits.iter().chain(&rest.hits).map(|h| h.rel.clone()).collect();
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), 5, "pages don't overlap");
        assert_eq!(lib.search("", 4, 10, &mut m, &none, &HashMap::new()).hits.len(), 1, "empty query pages too");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn used_memes_come_first_and_get_a_boost() {
        let root = std::env::temp_dir().join(format!("memeity-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for name in ["cat one.png", "cat two.png", "dog.png"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let lib = Library::scan(&root);
        let mut m = Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths());
        let usage = HashMap::from([("dog.png".to_string(), 3.0), ("cat two.png".to_string(), 1.0)]);

        let empty = lib.search("", 0, 10, &mut m, &HashMap::new(), &usage);
        let order: Vec<_> = empty.hits.iter().map(|h| h.rel.as_str()).collect();
        assert_eq!(&order[..2], ["dog.png", "cat two.png"], "most used first");
        assert_eq!(order.len(), 3, "then the rest");

        let cats = lib.search("cat", 0, 10, &mut m, &HashMap::new(), &usage);
        assert_eq!(cats.hits[0].rel, "cat two.png", "used meme wins an otherwise equal match");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
