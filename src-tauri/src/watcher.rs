//! Re-index (and sync) when files in the local meme folder are added, changed, or removed.

use crate::{library::Library, sync, AppState};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// Quiet period before rescanning, so a burst of 50 files triggers one rescan.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// True if every path in the event is inside a dot-folder (e.g. sync's temp dir).
fn only_hidden(root: &Path, event: &Event) -> bool {
    event.paths.iter().all(|p| {
        p.strip_prefix(root)
            .map(|rel| rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')))
            .unwrap_or(false)
    })
}

/// Starts watching `root`. Dropping the returned watcher stops the background thread.
pub fn watch(app: AppHandle, root: PathBuf) -> Option<RecommendedWatcher> {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)
        .inspect_err(|e| eprintln!("[memeity] watcher init failed: {e}"))
        .ok()?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .inspect_err(|e| eprintln!("[memeity] can't watch {}: {e}", root.display()))
        .ok()?;

    std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            let relevant = matches!(&event, Ok(e) if !matches!(e.kind, EventKind::Access(_)) && !only_hidden(&root, e));
            if !relevant {
                continue;
            }
            loop {
                match rx.recv_timeout(DEBOUNCE) {
                    Ok(_) => continue,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }

            let state = app.state::<AppState>();
            let cache = state.library.read().unwrap().dims_cache();
            let mut fresh = Library::scan(&root);
            fresh.fill_dims(&cache);
            {
                let mut lib = state.library.write().unwrap();
                // The user may have switched folders while we were scanning.
                if lib.root.as_deref() != Some(root.as_path()) {
                    return;
                }
                *lib = fresh;
            }
            let _ = app.emit("library-changed", ());
            // New local files should head up to Drive.
            sync::trigger(&app);
        }
    });

    Some(watcher)
}
