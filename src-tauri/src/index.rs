//! Per-device search index: SQLite + FTS5, in the app-data dir. Never stored in the
//! synced folder (two devices writing one SQLite file through a sync client corrupts it).
//!
//! It's a cache: everything in it can be rebuilt from the library and its sidecars.
//! - `memes` remembers each file's content hash (keyed by path + size + mtime, so we
//!   don't re-hash unchanged files) and whether its text is indexed.
//! - `meme_fts` holds searchable text per meme: OCR text now, tags in milestone 2.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memes (
    id            INTEGER PRIMARY KEY,
    rel           TEXT NOT NULL UNIQUE,
    size          INTEGER NOT NULL,
    mtime         INTEGER NOT NULL,
    md5           TEXT NOT NULL,
    indexed       INTEGER NOT NULL DEFAULT 0,
    sidecar_mtime INTEGER,
    engine        TEXT,
    phash         INTEGER
);
-- How much each image (by content hash) gets used on this device: copies and pastes.
-- `score` decays with a half-life (see USAGE_HALF_LIFE_MS), so it means
-- recently AND frequently used.
CREATE TABLE IF NOT EXISTS usage (
    md5       TEXT PRIMARY KEY,
    score     REAL NOT NULL,
    last_used INTEGER NOT NULL
);
-- Pinned memes (by content hash), mirrored from their sidecars.
CREATE TABLE IF NOT EXISTS pins (
    md5 TEXT PRIMARY KEY
);
-- Other devices' usage, read from their synced usage files (see usage.rs). Replaced
-- wholesale per device whenever its file changes.
CREATE TABLE IF NOT EXISTS remote_usage (
    device    TEXT NOT NULL,
    md5       TEXT NOT NULL,
    score     REAL NOT NULL,
    last_used INTEGER NOT NULL,
    PRIMARY KEY (device, md5)
);
CREATE VIRTUAL TABLE IF NOT EXISTS meme_fts USING fts5(
    text,
    tags,
    tokenize = 'unicode61 remove_diacritics 2'
);
";

/// What the index knows about one file.
pub struct Entry {
    pub size: i64,
    pub mtime: i64,
    pub md5: String,
    pub indexed: bool,
    pub sidecar_mtime: Option<i64>,
    /// Which OCR engine produced the indexed text (e.g. "paddleocr.pp-ocrv6-small").
    pub engine: Option<String>,
}

pub struct Index {
    conn: Connection,
}

impl Index {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        init(&conn)?;
        Ok(Index { conn })
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        Index { conn }
    }

    pub fn lookup(&self, rel: &str) -> rusqlite::Result<Option<Entry>> {
        self.conn
            .query_row(
                "SELECT size, mtime, md5, indexed, sidecar_mtime, engine FROM memes WHERE rel = ?1",
                [rel],
                |r| {
                    Ok(Entry {
                        size: r.get(0)?,
                        mtime: r.get(1)?,
                        md5: r.get(2)?,
                        indexed: r.get::<_, i64>(3)? != 0,
                        sidecar_mtime: r.get(4)?,
                        engine: r.get(5)?,
                    })
                },
            )
            .optional()
    }

    /// Records a (new or changed) file. Its old text is dropped until re-indexed.
    pub fn upsert_file(&self, rel: &str, size: i64, mtime: i64, md5: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO memes (rel, size, mtime, md5, indexed, sidecar_mtime)
             VALUES (?1, ?2, ?3, ?4, 0, NULL)
             ON CONFLICT(rel) DO UPDATE SET size = ?2, mtime = ?3, md5 = ?4, indexed = 0, sidecar_mtime = NULL, engine = NULL, phash = NULL",
            params![rel, size, mtime, md5],
        )?;
        self.conn.execute(
            "DELETE FROM meme_fts WHERE rowid = (SELECT id FROM memes WHERE rel = ?1)",
            [rel],
        )?;
        Ok(())
    }

    /// Sets the searchable text for a meme and marks it indexed.
    pub fn set_text(
        &self,
        rel: &str,
        text: &str,
        tags: &[String],
        sidecar_mtime: Option<i64>,
        engine: Option<&str>,
    ) -> rusqlite::Result<()> {
        let Some(id) = self
            .conn
            .query_row("SELECT id FROM memes WHERE rel = ?1", [rel], |r| r.get::<_, i64>(0))
            .optional()?
        else {
            return Ok(());
        };
        self.conn.execute("DELETE FROM meme_fts WHERE rowid = ?1", [id])?;
        self.conn.execute(
            "INSERT INTO meme_fts (rowid, text, tags) VALUES (?1, ?2, ?3)",
            params![id, text, tags.join(" ")],
        )?;
        self.conn.execute(
            "UPDATE memes SET indexed = 1, sidecar_mtime = ?2, engine = ?3 WHERE id = ?1",
            params![id, sidecar_mtime, engine],
        )?;
        Ok(())
    }

    /// Forgets files that are no longer in the library.
    pub fn retain(&self, keep: &HashSet<String>) -> rusqlite::Result<()> {
        let stale: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, rel FROM memes")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.filter_map(Result::ok).filter(|(_, rel)| !keep.contains(rel)).collect()
        };
        for (id, _) in stale {
            self.conn.execute("DELETE FROM meme_fts WHERE rowid = ?1", [id])?;
            self.conn.execute("DELETE FROM memes WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// Records that a meme was copied/pasted (by its library-relative path).
    pub fn record_use(&self, rel: &str, now_ms: i64) -> rusqlite::Result<()> {
        let Some(md5) = self
            .conn
            .query_row("SELECT md5 FROM memes WHERE rel = ?1", [rel], |r| r.get::<_, String>(0))
            .optional()?
        else {
            return Ok(()); // not hashed yet; rare, and harmless to skip
        };
        let previous: Option<(f64, i64)> = self
            .conn
            .query_row("SELECT score, last_used FROM usage WHERE md5 = ?1", [&md5], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let score = previous.map_or(0.0, |(score, last)| decayed(score, last, now_ms)) + 1.0;
        self.conn.execute(
            "INSERT INTO usage (md5, score, last_used) VALUES (?1, ?2, ?3)
             ON CONFLICT(md5) DO UPDATE SET score = ?2, last_used = ?3",
            params![md5, score, now_ms],
        )?;
        Ok(())
    }

    /// Current (decayed) usage score per meme, for memes that have been used: this
    /// device's uses plus every other device's.
    pub fn usage_scores(&self, now_ms: i64) -> rusqlite::Result<HashMap<String, f64>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.rel, u.score, u.last_used
             FROM (SELECT md5, score, last_used FROM usage
                   UNION ALL SELECT md5, score, last_used FROM remote_usage) u
             JOIN memes m ON m.md5 = u.md5",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut out: HashMap<String, f64> = HashMap::new();
        for row in rows {
            let (rel, score, last) = row?;
            *out.entry(rel).or_default() += decayed(score, last, now_ms);
        }
        out.retain(|_, score| *score >= MIN_USAGE);
        Ok(out)
    }

    pub fn set_pinned(&self, md5: &str, pinned: bool) -> rusqlite::Result<()> {
        if pinned {
            self.conn.execute("INSERT OR IGNORE INTO pins (md5) VALUES (?1)", [md5])?;
        } else {
            self.conn.execute("DELETE FROM pins WHERE md5 = ?1", [md5])?;
        }
        Ok(())
    }

    /// Library-relative paths of pinned memes.
    pub fn pinned_rels(&self) -> rusqlite::Result<HashSet<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT m.rel FROM pins p JOIN memes m ON m.md5 = p.md5")?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    /// This device's own uses that still count (for its synced usage file), by content hash.
    pub fn own_usage(&self, now_ms: i64) -> rusqlite::Result<std::collections::BTreeMap<String, crate::usage::Use>> {
        let mut stmt = self.conn.prepare_cached("SELECT md5, score, last_used FROM usage")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, crate::usage::Use { score: r.get(1)?, last_used: r.get(2)? }))
        })?;
        let mut out = std::collections::BTreeMap::new();
        for row in rows {
            let (md5, u) = row?;
            if decayed(u.score, u.last_used, now_ms) >= MIN_USAGE {
                out.insert(md5, u);
            }
        }
        Ok(out)
    }

    /// Replaces what we know of another device's usage.
    pub fn set_remote_usage(
        &self,
        device: &str,
        uses: &std::collections::BTreeMap<String, crate::usage::Use>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM remote_usage WHERE device = ?1", [device])?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO remote_usage (device, md5, score, last_used) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (md5, u) in uses {
                if u.score.is_finite() && u.score > 0.0 {
                    insert.execute(params![device, md5, u.score, u.last_used])?;
                }
            }
        }
        tx.commit()
    }

    /// Forgets devices whose usage file is gone.
    pub fn retain_remote_devices(&self, devices: &HashSet<String>) -> rusqlite::Result<()> {
        let known: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT DISTINCT device FROM remote_usage")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for device in known.iter().filter(|d| !devices.contains(*d)) {
            self.conn.execute("DELETE FROM remote_usage WHERE device = ?1", [device])?;
        }
        Ok(())
    }

    /// Memes that don't have a perceptual hash yet (newest first isn't needed: all get one).
    pub fn missing_phash(&self, limit: usize) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT rel FROM memes WHERE phash IS NULL LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| r.get(0))?;
        rows.collect()
    }

    pub fn set_phash(&self, rel: &str, phash: u64) -> rusqlite::Result<()> {
        // Stored as SQLite's signed 64-bit integer; same bits.
        self.conn.execute("UPDATE memes SET phash = ?2 WHERE rel = ?1", params![rel, phash as i64])?;
        Ok(())
    }

    /// Every meme's content hash and perceptual hash, for the duplicate finder.
    pub fn duplicate_candidates(&self) -> rusqlite::Result<Vec<crate::dupes::Candidate>> {
        let mut stmt = self.conn.prepare("SELECT rel, md5, phash FROM memes")?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::dupes::Candidate {
                rel: r.get(0)?,
                md5: r.get(1)?,
                phash: r.get::<_, Option<i64>>(2)?.map(|h| h as u64),
            })
        })?;
        rows.collect()
    }

    /// A meme with exactly these bytes (by MD5), if the library has one.
    pub fn rel_for_md5(&self, md5: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT rel FROM memes WHERE md5 = ?1 LIMIT 1", [md5], |r| r.get(0))
            .optional()
    }

    /// Every indexed meme's text and tags (for the in-memory loose-match cache).
    pub fn all_texts(&self) -> rusqlite::Result<Vec<(String, String, Vec<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT m.rel, f.text, f.tags FROM meme_fts f JOIN memes m ON m.id = f.rowid")?;
        let rows = stmt.query_map([], |r| {
            let tags: String = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, tags.split_whitespace().map(str::to_owned).collect()))
        })?;
        rows.collect()
    }

    /// How many memes have had their text indexed.
    pub fn indexed_count(&self) -> rusqlite::Result<usize> {
        self.conn
            .query_row("SELECT COUNT(*) FROM memes WHERE indexed = 1", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
    }

    /// Memes whose text or tags match every word of `query` (as prefixes), best first.
    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<String>> {
        let Some(fts) = fts_query(query) else { return Ok(Vec::new()) };
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.rel FROM meme_fts JOIN memes m ON m.id = meme_fts.rowid
             WHERE meme_fts MATCH ?1 ORDER BY bm25(meme_fts) LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts, limit as i64], |r| r.get(0))?;
        rows.collect()
    }
}

/// Usage counts halve every two weeks: last month's favourites fade, today's rise.
const USAGE_HALF_LIFE_MS: f64 = 14.0 * 24.0 * 3600.0 * 1000.0;
/// Below this, a meme no longer counts as "used" (one use ~2 months ago).
const MIN_USAGE: f64 = 0.05;

fn decayed(score: f64, last_used_ms: i64, now_ms: i64) -> f64 {
    let age = (now_ms - last_used_ms).max(0) as f64;
    score * 0.5f64.powf(age / USAGE_HALF_LIFE_MS)
}

/// Creates the schema, and migrates indexes made by earlier versions.
fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    // Each version added a column; fresh databases already have it from SCHEMA.
    let add_column = |name: &str, kind: &str| -> rusqlite::Result<()> {
        if conn.prepare(&format!("SELECT {name} FROM memes LIMIT 0")).is_err() {
            conn.execute_batch(&format!("ALTER TABLE memes ADD COLUMN {name} {kind}"))?;
        }
        Ok(())
    };
    if version < 2 {
        add_column("engine", "TEXT")?; // which OCR engine produced the text
    }
    if version < 3 {
        add_column("phash", "INTEGER")?; // perceptual hash, for the duplicate finder
        conn.execute_batch("PRAGMA user_version = 3")?;
    }
    Ok(())
}

/// Turns free text into a safe FTS5 query: each word becomes a quoted prefix term
/// (`when the` -> `"when"* "the"*`, i.e. both must match). Punctuation is dropped so
/// user input can never be parsed as FTS syntax.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_safe_prefix_queries() {
        assert_eq!(fts_query("when the").as_deref(), Some("\"when\"* \"the\"*"));
        assert_eq!(fts_query("\"); DROP TABLE memes; --").as_deref(), Some("\"DROP\"* \"TABLE\"* \"memes\"*"));
        assert_eq!(fts_query("  ?! "), None);
    }

    #[test]
    fn indexes_searches_and_forgets() {
        let idx = Index::in_memory();
        idx.upsert_file("a.png", 1, 1, "aaa").unwrap();
        idx.upsert_file("b.png", 1, 1, "bbb").unwrap();
        idx.set_text("a.png", "When the code compiles first try", &[], None, Some("eng")).unwrap();
        idx.set_text("b.png", "nobody: absolutely nobody", &["cat".into()], Some(5), Some("eng")).unwrap();

        assert_eq!(idx.search("compil", 10).unwrap(), vec!["a.png"], "prefix match");
        assert_eq!(idx.search("CODE when", 10).unwrap(), vec!["a.png"], "any order, case-insensitive");
        assert_eq!(idx.search("cat", 10).unwrap(), vec!["b.png"], "tags are searchable");
        assert!(idx.search("compiles nobody", 10).unwrap().is_empty(), "all words must match");

        let e = idx.lookup("b.png").unwrap().unwrap();
        assert!(e.indexed && e.sidecar_mtime == Some(5) && e.md5 == "bbb");
        assert_eq!(e.engine.as_deref(), Some("eng"));

        // Changing a file drops its old text until it's re-indexed.
        idx.upsert_file("a.png", 2, 2, "aaa2").unwrap();
        assert!(idx.search("compiles", 10).unwrap().is_empty());
        let a = idx.lookup("a.png").unwrap().unwrap();
        assert!(!a.indexed && a.engine.is_none(), "a changed file forgets its text and engine");

        idx.retain(&HashSet::from(["a.png".to_string()])).unwrap();
        assert!(idx.lookup("b.png").unwrap().is_none());
        assert!(idx.search("nobody", 10).unwrap().is_empty());
    }

    #[test]
    fn migrates_v1_index_by_adding_engine_column() {
        let conn = Connection::open_in_memory().unwrap();
        // A v1 database: same tables, no `engine` column, user_version 0.
        conn.execute_batch(&SCHEMA.replace(",
    engine        TEXT", "")).unwrap();
        assert!(conn.prepare("SELECT engine FROM memes").is_err(), "test setup: v1 has no engine");
        init(&conn).unwrap();
        assert!(conn.prepare("SELECT engine FROM memes").is_ok());
        init(&conn).unwrap(); // idempotent
    }

    #[test]
    fn other_devices_usage_adds_up() {
        use crate::usage::Use;
        use std::collections::BTreeMap;
        let idx = Index::in_memory();
        idx.upsert_file("a.png", 1, 1, "aaa").unwrap();
        idx.upsert_file("b.png", 1, 1, "bbb").unwrap();
        idx.record_use("a.png", 0).unwrap();
        let laptop = BTreeMap::from([
            ("aaa".to_string(), Use { score: 2.0, last_used: 0 }),
            ("bbb".to_string(), Use { score: 1.0, last_used: 0 }),
            ("gone".to_string(), Use { score: 9.0, last_used: 0 }), // not in this library
        ]);
        idx.set_remote_usage("laptop", &laptop).unwrap();
        let scores = idx.usage_scores(0).unwrap();
        assert!((scores["a.png"] - 3.0).abs() < 1e-9, "1 here + 2 on the laptop");
        assert!((scores["b.png"] - 1.0).abs() < 1e-9, "used only on the laptop");
        assert_eq!(scores.len(), 2);

        // Only this device's own uses get published.
        let own = idx.own_usage(0).unwrap();
        assert_eq!(own.keys().collect::<Vec<_>>(), vec!["aaa"]);

        // A newer file from the laptop replaces its old numbers; a vanished device is forgotten.
        idx.set_remote_usage("laptop", &BTreeMap::from([("bbb".to_string(), Use { score: 4.0, last_used: 0 })])).unwrap();
        let scores = idx.usage_scores(0).unwrap();
        assert!((scores["a.png"] - 1.0).abs() < 1e-9);
        assert!((scores["b.png"] - 4.0).abs() < 1e-9);
        idx.retain_remote_devices(&HashSet::new()).unwrap();
        assert!(!idx.usage_scores(0).unwrap().contains_key("b.png"));
    }

    #[test]
    fn usage_counts_decay_and_follow_content() {
        let idx = Index::in_memory();
        idx.upsert_file("a.png", 1, 1, "aaa").unwrap();
        idx.upsert_file("b.png", 1, 1, "bbb").unwrap();
        let day = 24 * 3600 * 1000;
        idx.record_use("a.png", 0).unwrap();
        idx.record_use("a.png", 0).unwrap();
        idx.record_use("b.png", 14 * day).unwrap();

        let now = idx.usage_scores(14 * day).unwrap();
        assert!((now["a.png"] - 1.0).abs() < 1e-9, "2 uses, one half-life ago -> 1.0");
        assert!((now["b.png"] - 1.0).abs() < 1e-9, "1 use just now -> 1.0");

        // Renaming keeps the history (it's keyed by content).
        idx.retain(&HashSet::from(["b.png".to_string()])).unwrap();
        idx.upsert_file("renamed.png", 1, 1, "aaa").unwrap();
        assert!(idx.usage_scores(14 * day).unwrap().contains_key("renamed.png"));

        // Long unused memes drop out.
        assert!(idx.usage_scores(200 * day).unwrap().is_empty());
    }
}
