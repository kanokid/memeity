//! Background worker that keeps the search index's text up to date.
//!
//! On every library change it walks the memes (newest first) and, for each one whose
//! text isn't indexed yet:
//!   1. uses its sidecar if one exists (written here earlier, or synced from another
//!      device, so each image is OCR'd once, ever, across all devices),
//!   2. otherwise runs OCR, writes the sidecar (which then syncs to Drive), and indexes it.
//!
//! It also re-reads a sidecar whenever its file changes, which is how tags edited on
//! another device (milestone 2) will reach this device's search, and it re-reads (with
//! OCR) any meme whose text came from an older engine, so upgrades apply everywhere.

use crate::index::Index;
use crate::ocr::{self, Engine};
use crate::sidecar::{self, Sidecar};
use crate::{sync, sync_target, AppState};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

/// Memes that just arrived from Drive wait this long for their sidecar (made by the
/// device that added them) before this device reads their text itself.
const OCR_GRACE: Duration = Duration::from_secs(180);

/// Refresh search results / upload sidecars every this many OCR'd memes.
const BATCH: usize = 10;

#[derive(Clone, Default, Serialize)]
pub struct OcrStatus {
    /// "idle" | "preparing" (downloading the model) | "reading" | "unavailable"
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
        // Loaded (and, the first time, downloaded) on first use, then kept. A failure
        // (e.g. offline) isn't cached: the next pass simply tries again.
        let mut engine: Option<Engine> = None;
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

fn run(app: &AppHandle, rx: &Receiver<()>, engine: &mut Option<Engine>) -> Result<(), String> {
    let Some((_, root)) = sync_target(app) else { return Ok(()) };
    let (memes, thumb_keys): (Vec<(String, PathBuf)>, HashSet<String>) = {
        let state = app.state::<AppState>();
        let lib = state.library.read().unwrap();
        if lib.root.as_deref() != Some(root.as_path()) {
            return Ok(()); // library not loaded for this folder yet
        }
        (
            lib.memes.iter().map(|m| (m.rel.clone(), m.path.clone())).collect(),
            lib.memes.iter().map(|m| crate::thumbs::key(&m.path, m.modified)).collect(),
        )
    };

    // 1. Forget memes that are gone.
    let keep: HashSet<String> = memes.iter().map(|(rel, _)| rel.clone()).collect();
    if with_index(app, |idx| idx.retain(&keep))?.is_none() {
        return Ok(()); // index unavailable; search falls back to filenames
    }
    app.state::<AppState>().text_cache.lock().unwrap().retain(&keep);

    // "Re-read all memes" from Settings: OCR everything again, ignoring cached text.
    let force = app.state::<AppState>().reread_text.swap(false, std::sync::atomic::Ordering::SeqCst);

    // Memes downloaded from Drive within the grace period: leave their OCR to the device
    // that added them (its sidecar is on the way) rather than reading them twice.
    let waiting: std::collections::HashMap<String, Instant> = {
        let state = app.state::<AppState>();
        let mut arrived = state.arrived.lock().unwrap();
        arrived.retain(|_, at| at.elapsed() < OCR_GRACE);
        arrived.clone()
    };
    let mut wait_until: Option<Instant> = None;

    // 2. Work out what needs (re)indexing. Library order is newest first.
    let mut pending = Vec::new();
    let mut changed = false;
    for (rel, path) in memes {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let (size, mtime) = (meta.len() as i64, meta.modified().map(millis).unwrap_or(0));

        let cached = with_index(app, |idx| idx.lookup(&rel))?.flatten();
        let (md5, indexed, seen_sidecar, current_engine) = match cached {
            Some(e) if e.size == size && e.mtime == mtime => {
                let current = e.engine.as_deref() == Some(ocr::ENGINE_NAME);
                (e.md5, e.indexed, e.sidecar_mtime, current)
            }
            _ => {
                // New or changed file: hash it (outside the index lock).
                let Ok(md5) = sync::file_md5(&path) else { continue };
                with_index(app, |idx| idx.upsert_file(&rel, size, mtime, &md5))?;
                app.state::<AppState>().text_cache.lock().unwrap().remove(&rel);
                (md5, false, None, false)
            }
        };

        let sidecar_path = sidecar::path(&root, &md5);
        let sidecar_mtime = mtime_of(&sidecar_path);
        if force {
            // "Re-read all" re-runs OCR, but keeps text the user typed themselves.
            let edited = sidecar::read(&root, &md5).and_then(|sc| sc.ocr).is_some_and(|o| o.edited);
            if !edited {
                pending.push(Pending { rel, path, md5 });
                continue;
            }
        }
        if indexed && sidecar_mtime == seen_sidecar && current_engine {
            continue; // up to date
        }
        match sidecar::read(&root, &md5) {
            Some(sc) if sc.ocr.is_some() => {
                let ocr_text = sc.ocr.as_ref().expect("checked above");
                // Index what we have right away so search works meanwhile...
                with_index(app, |idx| {
                    idx.set_text(&rel, &ocr_text.text, &sc.tags, sidecar_mtime, Some(&ocr_text.engine))
                })?;
                app.state::<AppState>().text_cache.lock().unwrap().set(&rel, &ocr_text.text, &sc.tags);
                changed = true;
                // ...and re-read it if an older engine produced that text.
                if ocr_text.engine != ocr::ENGINE_NAME && !ocr_text.edited {
                    pending.push(Pending { rel, path, md5 });
                }
            }
            _ => match waiting.get(&rel) {
                // No sidecar yet for a meme that just arrived: check again after the grace.
                Some(at) => {
                    let ready = *at + OCR_GRACE;
                    wait_until = Some(wait_until.map_or(ready, |w| w.min(ready)));
                }
                None => pending.push(Pending { rel, path, md5 }),
            },
        }
    }
    if changed {
        let _ = app.emit("library-changed", ());
    }

    if let Some(when) = wait_until {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(when.saturating_duration_since(Instant::now()) + Duration::from_secs(1));
            trigger(&app);
        });
    }

    if pending.is_empty() {
        set_status(app, OcrStatus { state: "idle", ..Default::default() });
        fill_phashes(app, &root, rx);
        prune_thumbs(app, &thumb_keys);
        return Ok(());
    }

    // 3. OCR whatever is left (loading the engine, and downloading it the first time).
    if engine.is_none() {
        let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join(ocr::MODEL_DIR);
        if !Engine::is_downloaded(&dir) {
            let message = format!("Downloading the text recognition model ({})…", ocr::DOWNLOAD_SIZE);
            set_status(app, OcrStatus { state: "preparing", message: Some(message), ..Default::default() });
        }
        match Engine::new(&dir) {
            Ok(loaded) => *engine = Some(loaded),
            Err(message) => {
                set_status(app, OcrStatus { state: "unavailable", message: Some(message), ..Default::default() });
                return Ok(());
            }
        }
    }
    let engine = engine.as_ref().expect("loaded above");
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
                if sc.ocr.as_ref().is_some_and(|o| o.edited) {
                    continue; // the user typed this meme's text while OCR was running
                }
                sc.ocr = Some(sidecar::Ocr {
                    text: text.clone(),
                    engine: ocr::ENGINE_NAME.to_string(),
                    lang: None, // PP-OCRv6 reads many scripts at once
                    edited: false,
                    at: Some(humantime::format_rfc3339_seconds(SystemTime::now()).to_string()),
                });
                let sidecar_mtime = match sidecar::write(&root, &item.md5, &sc) {
                    Ok(()) => mtime_of(&sidecar::path(&root, &item.md5)),
                    Err(e) => {
                        eprintln!("[memeity] couldn't write sidecar for {}: {e}", item.rel);
                        None
                    }
                };
                with_index(app, |idx| idx.set_text(&item.rel, &text, &sc.tags, sidecar_mtime, Some(ocr::ENGINE_NAME)))?;
                app.state::<AppState>().text_cache.lock().unwrap().set(&item.rel, &text, &sc.tags);
            }
            Err(e) => {
                // Unreadable image: index it as text-less so we don't retry every pass.
                eprintln!("[memeity] OCR failed for {}: {e}", item.rel);
                with_index(app, |idx| idx.set_text(&item.rel, "", &[], None, Some(ocr::ENGINE_NAME)))?;
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
    } else {
        fill_phashes(app, &root, rx);
        prune_thumbs(app, &thumb_keys);
    }
    Ok(())
}

/// Clears cached thumbnails that no longer belong to any meme in the library.
fn prune_thumbs(app: &AppHandle, keep: &HashSet<String>) {
    if let Some(dir) = crate::thumbs::cache_dir(app) {
        let removed = crate::thumbs::prune(&dir, keep);
        if removed > 0 {
            eprintln!("[memeity] cleared {removed} unused thumbnail(s)");
        }
    }
}

/// Background, lowest priority: perceptual hashes for the duplicate finder. Stops as
/// soon as anything else needs the worker (the next pass picks up where this left off).
fn fill_phashes(app: &AppHandle, root: &Path, rx: &Receiver<()>) {
    const CHUNK: usize = 200;
    loop {
        let Ok(Some(rels)) = with_index(app, |idx| idx.missing_phash(CHUNK)) else { return };
        if rels.is_empty() {
            return;
        }
        for rel in &rels {
            if rx.try_recv().is_ok() {
                trigger(app); // something changed: handle that first, resume later
                return;
            }
            let path = rel.split('/').fold(root.to_path_buf(), |p, part| p.join(part));
            // Unreadable images get 0 so they aren't retried every pass (and match nothing
            // useful: duplicates of a blank hash are still checked by MD5).
            let hash = crate::dupes::dhash(&path).unwrap_or(0);
            if with_index(app, |idx| idx.set_phash(rel, hash)).is_err() {
                return;
            }
        }
    }
}
