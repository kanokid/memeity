//! Two-way sync between one Google Drive folder and a local mirror folder.
//!
//! Rules:
//! - Drive is the source of truth. New or changed files on Drive download; files
//!   removed from Drive (in Drive's UI or via memeity's delete) leave this device
//!   too (they go to the Recycle Bin, not straight to oblivion).
//! - New files dropped into the local folder upload to Drive.
//! - Deleting a file locally (e.g. in Explorer) does NOT delete it on Drive. It
//!   re-downloads on the next sync. Real deletes go through memeity's pane
//!   (which trashes on Drive) or Drive's own UI.
//!
//! - Sidecars (`.meta/<md5>.json`, see `sidecar.rs`) sync like memes, so text read on
//!   one device is reused everywhere. On a sidecar conflict Drive's copy wins (they're
//!   regenerable metadata, not user files worth keeping twice).
//!
//! Per-device bookkeeping (`sync-state.json`) lives in the app-data dir, never in
//! the synced folder.

use crate::google::{DriveItem, Google, FOLDER_MIME};
use crate::library::{is_meme_name, Library};
use crate::sidecar::{self, META_DIR};
use crate::{refresh_library, sync_target, AppState};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::UnboundedReceiver;

/// How often to poll Drive for remote changes.
const POLL: Duration = Duration::from_secs(30);
/// Every this many polls, do a full sync instead of a change-feed check.
const FULL_EVERY: u32 = 20;
/// Showing the pane triggers a sync if the last one is older than this.
const STALE: Duration = Duration::from_secs(15);
/// Don't upload files modified more recently than this (they may still be being written).
const SETTLE: Duration = Duration::from_secs(2);
/// Downloads land here first, then get renamed into place. Dot-prefixed, so it's never indexed.
const TMP_DIR: &str = ".memeity-tmp";

#[derive(Clone, Copy, Default, Serialize)]
pub struct Counts {
    pub downloaded: usize,
    pub uploaded: usize,
    pub removed: usize,
}

#[derive(Clone, Default, Serialize)]
pub struct SyncStatus {
    /// "off" | "syncing" | "idle" | "error"
    pub state: &'static str,
    pub message: Option<String>,
    /// Unix millis of the last completed sync.
    pub last_synced: Option<u64>,
    pub last: Counts,
}

#[derive(Default, Serialize, Deserialize)]
struct SyncState {
    folder_id: String,
    /// The Drive scope this state was built under. If it changes, the set of files we
    /// can *see* changes, so stale entries would look like remote deletes. Start over.
    #[serde(default)]
    scope: String,
    /// Local relative path (`/`-separated) -> what it was last synced as.
    files: BTreeMap<String, Synced>,
    /// Bookmark in Drive's change feed, taken at the start of the last full sync. Polls
    /// ask "anything new since this?" instead of listing the whole folder.
    #[serde(default)]
    page_token: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Synced {
    id: String,
    md5: Option<String>,
}

impl SyncState {
    fn file(data_dir: &Path) -> PathBuf {
        data_dir.join("sync-state.json")
    }

    fn load(data_dir: &Path, folder_id: &str) -> Self {
        std::fs::read(Self::file(data_dir))
            .ok()
            .and_then(|b| serde_json::from_slice::<SyncState>(&b).ok())
            .filter(|s| s.folder_id == folder_id && s.scope == crate::google::SCOPE)
            .unwrap_or_else(|| SyncState {
                folder_id: folder_id.to_string(),
                scope: crate::google::SCOPE.to_string(),
                files: BTreeMap::new(),
                page_token: None,
            })
    }

    fn save(&self, data_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        std::fs::write(Self::file(data_dir), serde_json::to_vec(self).expect("state serializes"))
            .map_err(|e| format!("Couldn't save sync state: {e}"))
    }
}

// ---------- loop & triggers ----------

/// How thorough a sync pass should be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncKind {
    /// Ask Drive's change feed whether anything changed; only sync fully if so.
    Check,
    /// List everything and reconcile (startup, local changes, "Sync now").
    Full,
}

pub fn spawn_loop(app: AppHandle, mut rx: UnboundedReceiver<SyncKind>) {
    tauri::async_runtime::spawn(async move {
        let mut polls = 0u32;
        loop {
            let mut kind = tokio::select! {
                msg = rx.recv() => match msg { Some(kind) => kind, None => return },
                _ = tokio::time::sleep(POLL) => {
                    // Safety net: a full pass every FULL_EVERY polls (~10 min), in case the
                    // file watcher ever missed a local change.
                    polls += 1;
                    if polls.is_multiple_of(FULL_EVERY) { SyncKind::Full } else { SyncKind::Check }
                }
            };
            // Coalesce bursts of triggers; any request for a full sync wins.
            while let Ok(more) = rx.try_recv() {
                if more == SyncKind::Full {
                    kind = SyncKind::Full;
                }
            }
            run_once(&app, kind).await;
        }
    });
}

/// Full sync (local files changed, "Sync now", new sidecars to upload...).
pub fn trigger(app: &AppHandle) {
    let _ = app.state::<AppState>().sync_tx.send(SyncKind::Full);
}

/// Opening the pane: a cheap check for remote changes, if the last sync is a while ago.
pub fn trigger_if_stale(app: &AppHandle) {
    let last = app.state::<AppState>().sync_status.lock().unwrap().last_synced;
    let stale = last.is_none_or(|ms| now_ms().saturating_sub(ms) > STALE.as_millis() as u64);
    if stale {
        let _ = app.state::<AppState>().sync_tx.send(SyncKind::Check);
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn set_status(app: &AppHandle, f: impl FnOnce(&mut SyncStatus)) {
    f(&mut app.state::<AppState>().sync_status.lock().unwrap());
    let _ = app.emit("sync-status", ());
}

async fn run_once(app: &AppHandle, kind: SyncKind) {
    let state = app.state::<AppState>();
    let Some((folder, root)) = sync_target(app) else {
        set_status(app, |s| *s = SyncStatus { state: "off", ..Default::default() });
        return;
    };
    if !state.google.signed_in() {
        set_status(app, |s| s.state = "off");
        return;
    }
    let Ok(data_dir) = app.path().app_data_dir() else { return };

    let _guard = state.sync_lock.lock().await;

    // A check is one tiny request when nothing changed on Drive. Local changes always
    // come in as Full syncs (from the file watcher), so they never wait on this.
    if kind == SyncKind::Check {
        let saved = SyncState::load(&data_dir, &folder.id);
        if let Some(token) = saved.page_token.clone()
            && let Ok(Some(fresh)) = state.google.changes_since(&token).await
        {
            let mut saved = saved;
            saved.page_token = Some(fresh);
            let _ = saved.save(&data_dir);
            set_status(app, |s| {
                if s.state != "error" {
                    s.state = "idle";
                    s.message = None;
                }
                s.last_synced = Some(now_ms());
                s.last = Counts::default();
            });
            return;
        }
        // Something changed (or there's no bookmark yet / the check failed): full sync.
    }

    set_status(app, |s| {
        s.state = "syncing";
        s.message = None;
    });

    let report = cycle(&state.google, &folder.id, &root, &data_dir).await;
    let changed = report.counts.downloaded + report.counts.uploaded + report.counts.removed > 0;
    set_status(app, |s| {
        s.last = report.counts;
        s.last_synced = Some(now_ms());
        match report.errors.first() {
            None => {
                s.state = "idle";
                s.message = None;
            }
            Some(first) => {
                s.state = "error";
                s.message = Some(if report.errors.len() > 1 {
                    format!("{first} (+{} more)", report.errors.len() - 1)
                } else {
                    first.clone()
                });
            }
        }
    });
    if !report.downloaded.is_empty() {
        let now = std::time::Instant::now();
        let mut arrived = state.arrived.lock().unwrap();
        for rel in &report.downloaded {
            arrived.insert(rel.clone(), now);
        }
    }
    if changed {
        refresh_library(app);
    }
    if report.retry_soon {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(SETTLE + Duration::from_secs(1)).await;
            trigger(&app);
        });
    }
}

// ---------- one sync pass ----------

struct Report {
    counts: Counts,
    errors: Vec<String>,
    retry_soon: bool,
    /// Memes (not sidecars) downloaded this pass.
    downloaded: Vec<String>,
}

async fn cycle(g: &Google, folder_id: &str, root: &Path, data_dir: &Path) -> Report {
    let mut report = Report { counts: Counts::default(), errors: Vec::new(), retry_soon: false, downloaded: Vec::new() };

    if let Err(e) = std::fs::create_dir_all(root.join(TMP_DIR)) {
        report.errors.push(format!("Couldn't create {}: {e}", root.display()));
        return report;
    }
    // Bookmark the change feed *before* reading Drive, so changes made while this sync
    // runs show up in the next check rather than being missed.
    let fresh_token = g.start_page_token().await.ok();
    let mut remote = match remote_tree(g, folder_id).await {
        Ok(r) => r,
        Err(e) => {
            report.errors.push(e);
            return report;
        }
    };
    let mut st = SyncState::load(data_dir, folder_id);

    // 0a. Renamed/moved on Drive -> rename the local file to match (same Drive id, new path).
    follow_remote_renames(root, &remote, &mut st, &mut report);
    // 0b. Renamed/moved locally -> rename on Drive (a file vanished and one with the same
    //     bytes appeared). Without this, a rename looks like "delete + new file" and
    //     we'd end up with two copies.
    push_local_renames(g, root, &mut remote, &mut st, &mut report).await;

    // 1. Gone from Drive -> gone from this device (to the Recycle Bin).
    let gone: Vec<String> = st.files.keys().filter(|rel| !remote.files.contains_key(*rel)).cloned().collect();
    for rel in gone {
        let path = rel_path(root, &rel);
        if path.exists()
            && let Err(e) = trash::delete(&path) {
                report.errors.push(format!("Couldn't remove {rel}: {e}"));
                continue;
            }
        st.files.remove(&rel);
        report.counts.removed += 1;
    }

    // 2. Download anything new, changed, or missing locally (incl. local deletes).
    for (rel, item) in &remote.files {
        let path = rel_path(root, rel);
        let need = if !path.is_file() {
            true
        } else if let Some(s) = st.files.get(rel) {
            s.id != item.id || s.md5 != item.md5_checksum
        } else if item.md5_checksum.is_some() && file_md5(&path).ok() == item.md5_checksum {
            // Same bytes already here (e.g. first sync, or state was lost). Just adopt it.
            st.files.insert(rel.clone(), Synced { id: item.id.clone(), md5: item.md5_checksum.clone() });
            false
        } else if sidecar::is_sidecar_rel(rel) {
            // Two devices wrote the same sidecar: take Drive's, no "(this device)" copy.
            true
        } else {
            // A different local file with the same name: keep it under a new name
            // (it'll upload in step 3) and download Drive's version.
            if let Err(e) = std::fs::rename(&path, conflict_path(&path)) {
                report.errors.push(format!("Couldn't set aside local {rel}: {e}"));
                continue;
            }
            true
        };
        if need {
            match download(g, item, root, &path).await {
                Ok(()) => {
                    st.files.insert(rel.clone(), Synced { id: item.id.clone(), md5: item.md5_checksum.clone() });
                    report.counts.downloaded += 1;
                    if !sidecar::is_sidecar_rel(rel) {
                        report.downloaded.push(rel.clone());
                    }
                }
                Err(e) => report.errors.push(format!("Download {rel}: {e}")),
            }
        }
    }

    // 3. Upload local files (memes and sidecars) Drive doesn't know about.
    let mut folders = remote.folders;
    let now = SystemTime::now();
    for meme in local_files(root) {
        if let Some(synced) = st.files.get(&meme.rel).cloned() {
            // Already on Drive. Sidecars change locally (e.g. text re-read by a newer OCR
            // engine; tags later), so push those edits, but only while Drive still has the
            // version we last synced. If another device changed it, step 2 already took
            // Drive's copy.
            let remote_unchanged = remote
                .files
                .get(&meme.rel)
                .is_some_and(|r| r.id == synced.id && r.md5_checksum == synced.md5);
            let settled = now.duration_since(meme.modified).is_ok_and(|age| age >= SETTLE);
            if sidecar::is_sidecar_rel(&meme.rel)
                && remote_unchanged
                && settled
                && let Ok(local_md5) = file_md5(&meme.path)
                && Some(&local_md5) != synced.md5.as_ref()
            {
                let result = async {
                    let bytes = std::fs::read(&meme.path).map_err(|e| e.to_string())?;
                    g.update_content(&synced.id, mime_for(&meme.rel), bytes).await
                }
                .await;
                match result {
                    Ok(item) => {
                        st.files.insert(meme.rel.clone(), Synced { id: item.id, md5: item.md5_checksum });
                        report.counts.uploaded += 1;
                    }
                    Err(e) => report.errors.push(format!("Update {}: {e}", meme.rel)),
                }
            }
            continue;
        }
        if now.duration_since(meme.modified).is_ok_and(|age| age < SETTLE) {
            report.retry_soon = true;
            continue;
        }
        let (dir, name) = meme.rel.rsplit_once('/').unwrap_or(("", &meme.rel));
        let result = async {
            let parent = ensure_folder(g, &mut folders, dir).await?;
            let bytes = std::fs::read(&meme.path).map_err(|e| e.to_string())?;
            g.upload(&parent, name, mime_for(name), bytes).await
        }
        .await;
        match result {
            Ok(item) => {
                st.files.insert(meme.rel.clone(), Synced { id: item.id, md5: item.md5_checksum });
                report.counts.uploaded += 1;
            }
            Err(e) => report.errors.push(format!("Upload {}: {e}", meme.rel)),
        }
    }

    st.page_token = fresh_token;
    if let Err(e) = st.save(data_dir) {
        report.errors.push(e);
    }
    report
}

fn follow_remote_renames(root: &Path, remote: &RemoteTree, st: &mut SyncState, report: &mut Report) {
    let rel_by_id: HashMap<&str, &str> = remote.files.iter().map(|(rel, f)| (f.id.as_str(), rel.as_str())).collect();
    let moves: Vec<(String, String)> = st
        .files
        .iter()
        .filter_map(|(old, s)| {
            let new = *rel_by_id.get(s.id.as_str())?;
            (new != old && !st.files.contains_key(new)).then(|| (old.clone(), new.to_string()))
        })
        .collect();
    for (old, new) in moves {
        let (from, to) = (rel_path(root, &old), rel_path(root, &new));
        if from.is_file() && !to.exists() {
            let moved = to
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::rename(&from, &to));
            if let Err(e) = moved {
                report.errors.push(format!("Couldn't rename {old} to {new}: {e}"));
                continue;
            }
        }
        if let Some(entry) = st.files.remove(&old) {
            st.files.insert(new, entry);
        }
    }
}

async fn push_local_renames(g: &Google, root: &Path, remote: &mut RemoteTree, st: &mut SyncState, report: &mut Report) {
    // Synced files that are missing locally but still on Drive, unchanged, at the same path.
    let missing: Vec<(String, Synced)> = st
        .files
        .iter()
        .filter(|(rel, s)| {
            s.md5.is_some()
                && !rel_path(root, rel).exists()
                && remote.files.get(*rel).is_some_and(|r| r.id == s.id)
        })
        .map(|(rel, s)| (rel.clone(), s.clone()))
        .collect();
    if missing.is_empty() {
        return;
    }

    // Local files memeity hasn't seen before, by content hash.
    let mut untracked: HashMap<String, String> = HashMap::new();
    for meme in Library::scan(root).memes {
        if !st.files.contains_key(&meme.rel) && !remote.files.contains_key(&meme.rel)
            && let Ok(md5) = file_md5(&meme.path) {
                untracked.entry(md5).or_insert(meme.rel);
            }
    }

    for (old, synced) in missing {
        let Some(new) = synced.md5.as_ref().and_then(|m| untracked.remove(m)) else { continue };
        let (old_dir, _) = old.rsplit_once('/').unwrap_or(("", &old));
        let (new_dir, new_name) = new.rsplit_once('/').unwrap_or(("", &new));
        let result = async {
            let reparent = if old_dir == new_dir {
                None
            } else {
                let from = remote.folders.get(old_dir).cloned().ok_or("unknown source folder")?;
                let to = ensure_folder(g, &mut remote.folders, new_dir).await?;
                Some((to, from))
            };
            g.rename(&synced.id, new_name, reparent.as_ref().map(|(to, from)| (to.as_str(), from.as_str())))
                .await
        }
        .await;
        match result {
            Ok(item) => {
                st.files.remove(&old);
                st.files.insert(new.clone(), Synced { id: item.id.clone(), md5: item.md5_checksum.clone() });
                remote.files.remove(&old);
                remote.files.insert(new, item);
            }
            Err(e) => report.errors.push(format!("Rename {old} to {new} on Drive: {e}")),
        }
    }
}

/// How long a delete can be undone. Afterwards the local copy goes to the Recycle Bin
/// (the Drive copy stays in Drive's trash for 30 days either way).
const UNDO_WINDOW: Duration = Duration::from_secs(10);
/// Deleted files wait here (hidden, never indexed or synced) until the undo window ends.
const HOLD_DIR: &str = ".memeity-tmp/deleted";

/// A delete that can still be undone.
pub struct HeldDelete {
    path: PathBuf,
    held: PathBuf,
    drive_id: Option<String>,
}

/// Removes a meme everywhere: trashes it on Drive and sets the local copy aside for
/// UNDO_WINDOW (then it goes to the Recycle Bin). Returns a token for `undo_delete`.
pub async fn delete(app: &AppHandle, path: &Path) -> Result<String, String> {
    let state = app.state::<AppState>();
    let (folder, root) = sync_target(app).ok_or("No Drive folder selected.")?;
    let rel = path
        .strip_prefix(&root)
        .map_err(|_| "That file isn't in your meme folder.".to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;

    let _guard = state.sync_lock.lock().await;
    let mut st = SyncState::load(&data_dir, &folder.id);
    let drive_id = st.files.get(&rel).map(|s| s.id.clone());
    if let Some(id) = &drive_id {
        state.google.trash(id).await?;
    }

    let token = format!("{:x}", Md5::digest(format!("{rel}{}", now_ms())));
    let hold = root.join(HOLD_DIR).join(&token);
    std::fs::create_dir_all(&hold).map_err(|e| e.to_string())?;
    let held = hold.join(path.file_name().unwrap_or_default());
    if path.exists() {
        std::fs::rename(path, &held).map_err(|e| format!("Trashed on Drive, but couldn't remove the local copy: {e}"))?;
    }
    st.files.remove(&rel);
    st.save(&data_dir)?;

    state
        .pending_deletes
        .lock()
        .unwrap()
        .insert(token.clone(), HeldDelete { path: path.to_path_buf(), held, drive_id });
    let app = app.clone();
    let expire = token.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(UNDO_WINDOW).await;
        let entry = app.state::<AppState>().pending_deletes.lock().unwrap().remove(&expire);
        if let Some(entry) = entry {
            finalize(&entry.held);
        }
    });
    Ok(token)
}

/// Sends a held file to the Recycle Bin and removes its holding folder.
fn finalize(held: &Path) {
    if held.exists()
        && let Err(e) = trash::delete(held)
    {
        eprintln!("[memeity] couldn't move {} to the Recycle Bin: {e}", held.display());
        return;
    }
    if let Some(dir) = held.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Puts a just-deleted meme back, on Drive and on this device.
pub async fn undo_delete(app: &AppHandle, token: &str) -> Result<PathBuf, String> {
    let state = app.state::<AppState>();
    let entry = state.pending_deletes.lock().unwrap().remove(token).ok_or("It's too late to undo that.")?;
    let _guard = state.sync_lock.lock().await;
    // Drive first: if the local file came back first, sync could see it as a brand-new
    // file and upload a second copy.
    if let Some(id) = &entry.drive_id {
        state.google.untrash(id).await?;
    }
    let dest = if entry.path.exists() {
        let name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        conflict_path(&entry.path.with_file_name(name))
    } else {
        entry.path.clone()
    };
    std::fs::rename(&entry.held, &dest).map_err(|e| format!("Couldn't restore the file: {e}"))?;
    if let Some(dir) = entry.held.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(dest)
}

/// Startup: anything still held from last time (quit or crash within an undo window)
/// goes to the Recycle Bin now.
pub fn sweep_held_deletes(root: &Path) {
    let Ok(dirs) = std::fs::read_dir(root.join(HOLD_DIR)) else { return };
    for dir in dirs.flatten() {
        if let Ok(files) = std::fs::read_dir(dir.path()) {
            for file in files.flatten() {
                finalize(&file.path());
            }
        }
        let _ = std::fs::remove_dir(dir.path());
    }
}

// ---------- remote tree ----------

struct RemoteTree {
    /// Local relative path -> Drive file.
    files: HashMap<String, DriveItem>,
    /// Local relative dir ("" = the synced folder itself) -> Drive folder id.
    folders: HashMap<String, String>,
}

async fn remote_tree(g: &Google, root_id: &str) -> Result<RemoteTree, String> {
    let mut tree = RemoteTree {
        files: HashMap::new(),
        folders: HashMap::from([(String::new(), root_id.to_string())]),
    };
    // Windows paths are case-insensitive, so dedupe on lowercase.
    let mut taken: HashSet<String> = HashSet::new();
    let mut queue = vec![(String::new(), root_id.to_string())];

    while let Some((dir, id)) = queue.pop() {
        let mut items = g.list_children(&id).await?;
        items.sort_by(|a, b| a.id.cmp(&b.id)); // deterministic duplicate naming
        for item in items {
            let name = sanitize(&item.name);
            let is_folder = item.mime_type == FOLDER_MIME;
            let in_meta = dir == META_DIR;
            // Dot-names are skipped, except the top-level `.meta` sidecar folder.
            let meta_root = dir.is_empty() && name == META_DIR && is_folder;
            if name.is_empty() || (name.starts_with('.') && !meta_root) || (in_meta && is_folder) {
                continue;
            }
            let rel = join(&dir, &name);
            let wanted = if in_meta { name.ends_with(".json") } else { is_meme_name(&name) };
            if is_folder {
                if taken.insert(rel.to_lowercase()) {
                    tree.folders.insert(rel.clone(), item.id.clone());
                    queue.push((rel, item.id));
                }
            } else if wanted && !item.mime_type.starts_with("application/vnd.google-apps.") {
                let duplicate = taken.contains(&rel.to_lowercase());
                if duplicate && in_meta {
                    continue; // two devices uploaded the same sidecar at once; one is enough
                }
                let rel = if duplicate { dedupe(&rel, &item.id) } else { rel };
                taken.insert(rel.to_lowercase());
                tree.files.insert(rel, item);
            }
        }
    }
    Ok(tree)
}

async fn ensure_folder(g: &Google, folders: &mut HashMap<String, String>, dir: &str) -> Result<String, String> {
    if let Some(id) = folders.get(dir) {
        return Ok(id.clone());
    }
    let (parent_dir, name) = dir.rsplit_once('/').unwrap_or(("", dir));
    let parent = Box::pin(ensure_folder(g, folders, parent_dir)).await?;
    let id = g.create_folder(&parent, name).await?;
    folders.insert(dir.to_string(), id.clone());
    Ok(id)
}

async fn download(g: &Google, item: &DriveItem, root: &Path, dest: &Path) -> Result<(), String> {
    let bytes = g.download(&item.id).await?;
    let tmp = root.join(TMP_DIR).join(&item.id);
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    // Keep Drive's modified time so "newest first" means newest on Drive.
    if let Some(t) = item.modified_time.as_deref().and_then(|t| humantime::parse_rfc3339_weak(t).ok())
        && let Ok(f) = std::fs::File::options().write(true).open(&tmp) {
            let _ = f.set_modified(t);
        }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
}

// ---------- helpers ----------

fn rel_path(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, part| p.join(part))
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

/// Makes a Drive name safe as a Windows/macOS/Linux filename.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    cleaned.trim_end_matches(['.', ' ']).trim().to_string()
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// `cat.png` + id `1AbCdEf…` -> `cat (1AbCdE).png`
fn dedupe(rel: &str, id: &str) -> String {
    let (stem, ext) = split_ext(rel);
    format!("{stem} ({}){ext}", &id[..id.len().min(6)])
}

/// `cat.png` -> `cat (this device).png`, numbered if needed.
fn conflict_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = split_ext(&name);
    (1..)
        .map(|n| {
            let suffix = if n == 1 { String::new() } else { format!(" {n}") };
            path.with_file_name(format!("{stem} (this device{suffix}){ext}"))
        })
        .find(|p| !p.exists())
        .expect("some name is free")
}

/// A local file that sync may upload: a meme, or a sidecar in `.meta/`.
struct LocalFile {
    rel: String,
    path: PathBuf,
    modified: SystemTime,
}

fn local_files(root: &Path) -> Vec<LocalFile> {
    let mut files: Vec<LocalFile> = Library::scan(root)
        .memes
        .into_iter()
        .map(|m| LocalFile { rel: m.rel, path: m.path, modified: m.modified })
        .collect();
    if let Ok(entries) = std::fs::read_dir(root.join(META_DIR)) {
        for entry in entries.flatten() {
            let rel = format!("{META_DIR}/{}", entry.file_name().to_string_lossy());
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_file() && sidecar::is_sidecar_rel(&rel) {
                let modified = meta.modified().unwrap_or(UNIX_EPOCH);
                files.push(LocalFile { rel, path: entry.path(), modified });
            }
        }
    }
    files
}

pub(crate) fn file_md5(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(format!("{:x}", Md5::digest(&bytes)))
}

fn mime_for(name: &str) -> &'static str {
    match split_ext(name).1.to_ascii_lowercase().as_str() {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".bmp" => "image/bmp",
        ".json" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_makes_drive_names_filesystem_safe() {
        assert_eq!(sanitize("what: no?.png"), "what_ no_.png");
        assert_eq!(sanitize(r"a/b\c.jpg"), "a_b_c.jpg");
        assert_eq!(sanitize("trailing dots... "), "trailing dots");
    }

    #[test]
    fn dedupe_keeps_extension() {
        assert_eq!(dedupe("reactions/cat.png", "1AbCdEfGh"), "reactions/cat (1AbCdE).png");
    }

    #[test]
    fn conflict_path_picks_free_name() {
        let dir = std::env::temp_dir().join(format!("memeity-test-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cat.png");
        assert_eq!(conflict_path(&p), dir.join("cat (this device).png"));
        std::fs::write(dir.join("cat (this device).png"), b"x").unwrap();
        assert_eq!(conflict_path(&p), dir.join("cat (this device 2).png"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rel_path_uses_native_separators() {
        let root = Path::new("root");
        assert_eq!(rel_path(root, "a/b.png"), root.join("a").join("b.png"));
    }

    fn item(id: &str, md5: &str) -> DriveItem {
        DriveItem {
            id: id.into(),
            name: String::new(),
            mime_type: "image/png".into(),
            md5_checksum: Some(md5.into()),
            modified_time: None,
        }
    }

    #[test]
    fn follows_renames_made_on_drive() {
        let root = std::env::temp_dir().join(format!("memeity-rename-{}", now_ms()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("old.png"), b"x").unwrap();

        let mut st = SyncState::default();
        st.files.insert("old.png".into(), Synced { id: "A".into(), md5: Some("m".into()) });
        let remote = RemoteTree {
            files: HashMap::from([("reactions/new.png".to_string(), item("A", "m"))]),
            folders: HashMap::new(),
        };
        let mut report = Report { counts: Counts::default(), errors: vec![], retry_soon: false, downloaded: vec![] };

        follow_remote_renames(&root, &remote, &mut st, &mut report);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!root.join("old.png").exists());
        assert!(root.join("reactions").join("new.png").is_file(), "moved, not re-downloaded");
        assert!(st.files.contains_key("reactions/new.png") && !st.files.contains_key("old.png"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn mime_and_md5() {
        assert_eq!(mime_for("x.JPG"), "image/jpeg");
        let f = std::env::temp_dir().join(format!("memeity-md5-{}", now_ms()));
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(file_md5(&f).unwrap(), "5d41402abc4b2a76b9719d911017c592");
        std::fs::remove_file(f).unwrap();
    }
}
