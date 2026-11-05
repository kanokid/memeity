//! Per-device search index: SQLite + FTS5, in the app-data dir. Never stored in the
//! synced folder (two devices writing one SQLite file through a sync client corrupts it).
//!
//! It's a cache: everything in it can be rebuilt from the library and its sidecars.
//! - `memes` remembers each file's content hash (keyed by path + size + mtime, so we
//!   don't re-hash unchanged files) and whether its text is indexed.
//! - `meme_fts` holds searchable text per meme: OCR text now, tags in milestone 2.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memes (
    id            INTEGER PRIMARY KEY,
    rel           TEXT NOT NULL UNIQUE,
    size          INTEGER NOT NULL,
    mtime         INTEGER NOT NULL,
    md5           TEXT NOT NULL,
    indexed       INTEGER NOT NULL DEFAULT 0,
    sidecar_mtime INTEGER
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
}

pub struct Index {
    conn: Connection,
}

impl Index {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Index { conn })
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Index { conn }
    }

    pub fn lookup(&self, rel: &str) -> rusqlite::Result<Option<Entry>> {
        self.conn
            .query_row(
                "SELECT size, mtime, md5, indexed, sidecar_mtime FROM memes WHERE rel = ?1",
                [rel],
                |r| {
                    Ok(Entry {
                        size: r.get(0)?,
                        mtime: r.get(1)?,
                        md5: r.get(2)?,
                        indexed: r.get::<_, i64>(3)? != 0,
                        sidecar_mtime: r.get(4)?,
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
             ON CONFLICT(rel) DO UPDATE SET size = ?2, mtime = ?3, md5 = ?4, indexed = 0, sidecar_mtime = NULL",
            params![rel, size, mtime, md5],
        )?;
        self.conn.execute(
            "DELETE FROM meme_fts WHERE rowid = (SELECT id FROM memes WHERE rel = ?1)",
            [rel],
        )?;
        Ok(())
    }

    /// Sets the searchable text for a meme and marks it indexed.
    pub fn set_text(&self, rel: &str, text: &str, tags: &[String], sidecar_mtime: Option<i64>) -> rusqlite::Result<()> {
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
            "UPDATE memes SET indexed = 1, sidecar_mtime = ?2 WHERE id = ?1",
            params![id, sidecar_mtime],
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
        idx.set_text("a.png", "When the code compiles first try", &[], None).unwrap();
        idx.set_text("b.png", "nobody: absolutely nobody", &["cat".into()], Some(5)).unwrap();

        assert_eq!(idx.search("compil", 10).unwrap(), vec!["a.png"], "prefix match");
        assert_eq!(idx.search("CODE when", 10).unwrap(), vec!["a.png"], "any order, case-insensitive");
        assert_eq!(idx.search("cat", 10).unwrap(), vec!["b.png"], "tags are searchable");
        assert!(idx.search("compiles nobody", 10).unwrap().is_empty(), "all words must match");

        let e = idx.lookup("b.png").unwrap().unwrap();
        assert!(e.indexed && e.sidecar_mtime == Some(5) && e.md5 == "bbb");

        // Changing a file drops its old text until it's re-indexed.
        idx.upsert_file("a.png", 2, 2, "aaa2").unwrap();
        assert!(idx.search("compiles", 10).unwrap().is_empty());
        assert!(!idx.lookup("a.png").unwrap().unwrap().indexed);

        idx.retain(&HashSet::from(["a.png".to_string()])).unwrap();
        assert!(idx.lookup("b.png").unwrap().is_none());
        assert!(idx.search("nobody", 10).unwrap().is_empty());
    }
}
