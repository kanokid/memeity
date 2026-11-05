//! Background worker that keeps the search index's text up to date.
//!
//! On every library change it walks the memes (newest first) and, for each one whose
//! text isn't indexed yet:
//!   1. uses its sidecar if one exists (written here earlier, or synced from another
//!      device, so each image is OCR'd once, ever, across all devices),
//!   2. otherwise runs OCR, writes the sidecar (which then syncs to Drive), and indexes it.
//!
//! It also re-reads a sidecar whenever its file changes, which is how tags edited on
//! another device (milestone 2) will reach this device's search.

use crate::index::Index;
use crate::ocr::{self, Engine};
use crate::sidecar::{self, Sidecar};
use crate::{sync, sync_target, AppState};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

/// Refresh search results / upload sidecars every this many OCR'd memes.
const BATCH: usize = 10;

#[derive(Clone, Default, Serialize)]
pub struct OcrStatus {
    /// "idle" | "reading" | "unavailable"
    pub state: &'static str,
    pub done: usize,
    pub total: usize,
    pub message: Option<String>,
}

pub fn trigger(app: &AppHandle) {
    let _ = app.state::<AppState>().text_tx.send(());
}

pub fn spawn(app: AppHandle, rx: Receiver<()>) {
    std::thread::spawn(move || {
        // Created lazily on first use and kept for the thread's lifetime.
        let mut engine: Option<Result<Engine, String>> = None;
        while rx.recv().is_ok() {
            while rx.try_recv().is_ok() {} // coalesce
            if let Err(e) = run(&app, &rx, &mut engine) {
                eprintln!("[memeity] text indexing: {e}");
            }
        }
    });
}

fn set_status(app: &AppHandle, status: OcrStatus) {
    *app.state::<AppState>().ocr_status.lock().unwrap() = status;
    let _ = app.emit("ocr-status", ());
}

fn millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn mtime_of(path: &Path) -> Option<i64> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok().map(millis)
}

/// Runs `f` against the index, if it opened. Holds the lock only for that call.
fn with_index<T>(app: &AppHandle, f: impl FnOnce(&Index) -> rusqlite::Result<T>) -> Result<Option<T>, String> {
    let state = app.state::<AppState>();
    let guard = state.index.lock().unwrap();
    match guard.as_ref() {
        Some(idx) => f(idx).map(Some).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

struct Pending {
    rel: String,
    path: PathBuf,
    md5: String,
}

fn run(app: &AppHandle, rx: &Receiver<()>, engine: &mut Option<Result<Engine, String>>) -> Result<(), String> {
    let Some((_, root)) = sync_target(app) else { return Ok(()) };
    let memes: Vec<(String, PathBuf)> = {
        let state = app.state::<AppState>();
        let lib = state.library.read().unwrap();
        if lib.root.as_deref() != Some(root.as_path()) {
            return Ok(()); // library not loaded for this folder yet
        }
        lib.memes.iter().map(|m| (m.rel.clone(), m.path.clone())).collect()
    };

    // 1. Forget memes that are gone.
    let keep: HashSet<String> = memes.iter().map(|(rel, _)| rel.clone()).collect();
    if with_index(app, |idx| idx.retain(&keep))?.is_none() {
        return Ok(()); // index unavailable; search falls back to filenames
    }

    // "Re-read all memes" from Settings: OCR everything again, ignoring cached text.
    let force = app.state::<AppState>().reread_text.swap(false, std::sync::atomic::Ordering::SeqCst);

    // 2. Work out what needs (re)indexing. Library order is newest first.
    let mut pending = Vec::new();
    let mut changed = false;
    for (rel, path) in memes {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let (size, mtime) = (meta.len() as i64, meta.modified().map(millis).unwrap_or(0));

        let cached = with_index(app, |idx| idx.lookup(&rel))?.flatten();
        let (md5, indexed, seen_sidecar) = match cached {
            Some(e) if e.size == size && e.mtime == mtime => (e.md5, e.indexed, e.sidecar_mtime),
            _ => {
                // New or changed file: hash it (outside the index lock).
                let Ok(md5) = sync::file_md5(&path) else { continue };
                with_index(app, |idx| idx.upsert_file(&rel, size, mtime, &md5))?;
                (md5, false, None)
            }
        };

        let sidecar_path = sidecar::path(&root, &md5);
        let sidecar_mtime = mtime_of(&sidecar_path);
        if force {
            pending.push(Pending { rel, path, md5 });
            continue;
        }
        if indexed && sidecar_mtime == seen_sidecar {
            continue; // up to date
        }
        match sidecar::read(&root, &md5) {
            Some(sc) if sc.ocr.is_some() => {
                let text = sc.ocr.as_ref().map(|o| o.text.as_str()).unwrap_or_default();
                with_index(app, |idx| idx.set_text(&rel, text, &sc.tags, sidecar_mtime))?;
                changed = true;
            }
            _ => pending.push(Pending { rel, path, md5 }),
        }
    }
    if changed {
        let _ = app.emit("library-changed", ());
    }

    if pending.is_empty() {
        set_status(app, OcrStatus { state: "idle", ..Default::default() });
        return Ok(());
    }

    // 3. OCR whatever is left.
    let engine = match engine.get_or_insert_with(Engine::new) {
        Ok(engine) => engine,
        Err(message) => {
            set_status(app, OcrStatus { state: "unavailable", message: Some(message.clone()), ..Default::default() });
            return Ok(());
        }
    };
    let total = pending.len();
    set_status(app, OcrStatus { state: "reading", done: 0, total, message: None });

    let mut rerun = false;
    for (n, item) in pending.into_iter().enumerate() {
        if rx.try_recv().is_ok() {
            rerun = true; // the library changed meanwhile; go round again after this pass
        }
        if !item.path.exists() {
            continue;
        }
        match engine.recognize(&item.path) {
            Ok(text) => {
                let text = text.trim().to_string();
                // Merge into any existing sidecar so we never clobber tags/unknown fields.
                let mut sc: Sidecar = sidecar::read(&root, &item.md5).unwrap_or_default();
                sc.ocr = Some(sidecar::Ocr {
                    text: text.clone(),
                    engine: ocr::ENGINE_NAME.to_string(),
                    lang: engine.language(),
                    at: Some(humantime::format_rfc3339_seconds(SystemTime::now()).to_string()),
                });
                let sidecar_mtime = match sidecar::write(&root, &item.md5, &sc) {
                    Ok(()) => mtime_of(&sidecar::path(&root, &item.md5)),
                    Err(e) => {
                        eprintln!("[memeity] couldn't write sidecar for {}: {e}", item.rel);
                        None
                    }
                };
                with_index(app, |idx| idx.set_text(&item.rel, &text, &sc.tags, sidecar_mtime))?;
            }
            Err(e) => {
                // Unreadable image: index it as text-less so we don't retry every pass.
                eprintln!("[memeity] OCR failed for {}: {e}", item.rel);
                with_index(app, |idx| idx.set_text(&item.rel, "", &[], None))?;
            }
        }

        let done = n + 1;
        set_status(app, OcrStatus { state: "reading", done, total, message: None });
        if done % BATCH == 0 {
            let _ = app.emit("library-changed", ()); // new text is searchable now
            sync::trigger(app); // upload the new sidecars
        }
    }

    set_status(app, OcrStatus { state: "idle", ..Default::default() });
    let _ = app.emit("library-changed", ());
    sync::trigger(app);
    if rerun {
        trigger(app);
    }
    Ok(())
}
