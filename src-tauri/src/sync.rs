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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
/// Local files modified this long before the previous sync started aren't re-hashed to
/// look for local edits (covers coarse filesystem timestamps and small clock jumps).
const MTIME_SLACK: Duration = Duration::from_secs(5);

// ---------- Drive, as sync sees it ----------

/// The Drive calls sync makes. `Google` implements it for real; tests use a fake.
pub(crate) trait Drive {
    async fn list_children(&self, parent: &str) -> Result<Vec<DriveItem>, String>;
    async fn download(&self, id: &str) -> Result<Vec<u8>, String>;
    async fn upload(&self, parent: &str, name: &str, mime: &str, bytes: Vec<u8>, id: &str) -> Result<DriveItem, String>;
    async fn generate_ids(&self, count: u32) -> Result<Vec<String>, String>;
    async fn find(&self, id: &str) -> Result<Option<DriveItem>, String>;
    async fn create_folder(&self, parent: &str, name: &str) -> Result<String, String>;
    async fn rename(&self, id: &str, new_name: &str, reparent: Option<(&str, &str)>) -> Result<DriveItem, String>;
    async fn update_content(&self, id: &str, mime: &str, bytes: Vec<u8>) -> Result<DriveItem, String>;
    async fn start_page_token(&self) -> Result<String, String>;
}

impl Drive for Google {
    async fn list_children(&self, parent: &str) -> Result<Vec<DriveItem>, String> {
        Google::list_children(self, parent).await
    }
    async fn download(&self, id: &str) -> Result<Vec<u8>, String> {
        Google::download(self, id).await
    }
    async fn upload(&self, parent: &str, name: &str, mime: &str, bytes: Vec<u8>, id: &str) -> Result<DriveItem, String> {
        Google::upload(self, parent, name, mime, bytes, Some(id)).await
    }
    async fn generate_ids(&self, count: u32) -> Result<Vec<String>, String> {
        Google::generate_ids(self, count).await
    }
    async fn find(&self, id: &str) -> Result<Option<DriveItem>, String> {
        Google::find(self, id).await
    }
    async fn create_folder(&self, parent: &str, name: &str) -> Result<String, String> {
        Google::create_folder(self, parent, name).await
    }
    async fn rename(&self, id: &str, new_name: &str, reparent: Option<(&str, &str)>) -> Result<DriveItem, String> {
        Google::rename(self, id, new_name, reparent).await
    }
    async fn update_content(&self, id: &str, mime: &str, bytes: Vec<u8>) -> Result<DriveItem, String> {
        Google::update_content(self, id, mime, bytes).await
    }
    async fn start_page_token(&self) -> Result<String, String> {
        Google::start_page_token(self).await
    }
}

// ---------- files sync just wrote ----------

/// How long the file watcher ignores a path sync just wrote.
const OWN_WRITE_WINDOW: Duration = Duration::from_secs(10);

fn own_writes() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static WRITES: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    WRITES.get_or_init(Default::default)
}

/// Notes that sync itself just created/replaced/moved `path`, so the watcher doesn't
/// answer it with yet another full sync.
fn note_own_write(path: &Path) {
    let mut writes = own_writes().lock().unwrap();
    writes.retain(|_, at| at.elapsed() < OWN_WRITE_WINDOW);
    writes.insert(path.to_path_buf(), Instant::now());
}

/// True if sync wrote `path` in the last few seconds (for the file watcher).
pub fn is_own_write(path: &Path) -> bool {
    own_writes().lock().unwrap().get(path).is_some_and(|at| at.elapsed() < OWN_WRITE_WINDOW)
}

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
    /// Unix millis when the previous full sync started. Files untouched since then
    /// can't have been edited locally, so they aren't re-hashed.
    #[serde(default)]
    last_scan: Option<u64>,
    /// Files whose upload may not match what's on disk (they changed while uploading).
    /// Re-checked on every pass whatever their timestamps say.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    dirty: BTreeSet<String>,
    /// New files whose upload may or may not have reached Drive (e.g. the connection
    /// dropped before Drive answered), with the Drive id reserved for them. Retrying
    /// with the same id can't create a second copy.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending: BTreeMap<String, PendingUpload>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct PendingUpload {
    id: String,
    md5: String,
}

/// Reserved Drive ids, fetched in batches.
const ID_BATCH: u32 = 50;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
                last_scan: None,
                dirty: BTreeSet::new(),
                pending: BTreeMap::new(),
            })
    }

    fn save(&self, data_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        crate::config::write_atomic(&Self::file(data_dir), &serde_json::to_vec(self).expect("state serializes"))
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
    if changed || !report.errors.is_empty() {
        let c = report.counts;
        log::info!(
            "sync: {} down, {} up, {} removed, {} error(s)",
            c.downloaded,
            c.uploaded,
            c.removed,
            report.errors.len()
        );
    }
    for e in report.errors.iter().take(20) {
        log::warn!("sync: {e}");
    }
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

async fn cycle<D: Drive>(g: &D, folder_id: &str, root: &Path, data_dir: &Path) -> Report {
    let mut report = Report { counts: Counts::default(), errors: Vec::new(), retry_soon: false, downloaded: Vec::new() };

    if let Err(e) = tokio::fs::create_dir_all(root.join(TMP_DIR)).await {
        report.errors.push(format!("Couldn't create {}: {e}", root.display()));
        return report;
    }
    let started = now_ms();
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
    // Local files modified after this may have been edited here since the last sync.
    let edited_after = st
        .last_scan
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms).saturating_sub(MTIME_SLACK));

    // 0a. Renamed/moved on Drive -> rename the local file to match (same Drive id, new path).
    follow_remote_renames(root, &remote, &mut st, &mut report);
    // 0b. Renamed/moved locally -> rename on Drive (a file vanished and one with the same
    //     bytes appeared). Without this, a rename looks like "delete + new file" and
    //     we'd end up with two copies. Renames Drive refused are left alone this pass.
    let held_back = push_local_renames(g, root, &mut remote, &mut st, &mut report).await;

    // 1. Gone from Drive -> gone from this device (to the Recycle Bin).
    let gone: Vec<String> = st.files.keys().filter(|rel| !remote.files.contains_key(*rel)).cloned().collect();
    for rel in gone {
        let path = rel_path(root, &rel);
        if path.exists()
            && let Err(e) = to_recycle_bin(&path)
        {
            report.errors.push(format!("Couldn't remove {rel}: {e}"));
            continue;
        }
        st.files.remove(&rel);
        report.counts.removed += 1;
    }

    // Contents Drive already has (under any name). A local file with one of these is
    // never worth keeping a second "(this device)" copy of.
    let on_drive: HashSet<String> = remote.files.values().filter_map(|f| f.md5_checksum.clone()).collect();
    let backed_up = |md5: &Option<String>| md5.as_ref().is_some_and(|m| on_drive.contains(m));

    // 2. Download anything new, changed, or missing locally (incl. local deletes).
    for (rel, item) in &remote.files {
        if held_back.contains(rel) {
            continue;
        }
        let path = rel_path(root, rel);
        let is_sidecar = sidecar::is_sidecar_rel(rel);
        let need = if !path.is_file() {
            true
        } else if let Some(s) = st.files.get(rel).cloned() {
            if s.id == item.id && s.md5 == item.md5_checksum {
                false // unchanged on Drive; local edits (if any) upload in step 3
            } else {
                // Changed on Drive. Was it changed here too?
                let local = md5_of(&path).await.ok();
                if local.is_some() && local == item.md5_checksum {
                    // Both sides already have the same bytes.
                    st.files.insert(rel.clone(), Synced { id: item.id.clone(), md5: item.md5_checksum.clone() });
                    false
                } else if (local.is_some() && local == s.md5) || backed_up(&local) {
                    true // only Drive changed (or ours is on Drive anyway): take its copy
                } else if is_sidecar {
                    match merge_sidecar(g, item, &path).await {
                        Ok(true) => {
                            // Kept text the user typed here; it uploads once settled.
                            st.files.insert(rel.clone(), Synced { id: item.id.clone(), md5: item.md5_checksum.clone() });
                            report.retry_soon = true;
                            report.counts.downloaded += 1;
                            false
                        }
                        Ok(false) => true, // nothing of ours worth keeping: take Drive's
                        Err(e) => {
                            report.errors.push(format!("Download {rel}: {e}"));
                            continue;
                        }
                    }
                } else {
                    // Edited on both sides: keep this device's version under a new name.
                    if let Err(e) = set_aside(&path) {
                        report.errors.push(format!("Couldn't set aside local {rel}: {e}"));
                        continue;
                    }
                    true
                }
            }
        } else {
            let local = md5_of(&path).await.ok();
            if item.md5_checksum.is_some() && local == item.md5_checksum {
                // Same bytes already here (e.g. first sync, or state was lost). Just adopt it.
                st.files.insert(rel.clone(), Synced { id: item.id.clone(), md5: item.md5_checksum.clone() });
                false
            } else if is_sidecar {
                // Two devices wrote the same sidecar: take Drive's, no "(this device)" copy.
                true
            } else if backed_up(&local) {
                // Different from Drive's file of this name, but identical to another file
                // on Drive: nothing would be lost, so don't make (and upload) another copy.
                true
            } else {
                // A different local file with the same name: keep it under a new name
                // (it'll upload in step 3) and download Drive's version.
                if let Err(e) = set_aside(&path) {
                    report.errors.push(format!("Couldn't set aside local {rel}: {e}"));
                    continue;
                }
                true
            }
        };
        if need {
            match download(g, item, root, &path).await {
                Ok(()) => {
                    st.files.insert(rel.clone(), Synced { id: item.id.clone(), md5: item.md5_checksum.clone() });
                    report.counts.downloaded += 1;
                    if !is_sidecar {
                        report.downloaded.push(rel.clone());
                    }
                }
                Err(e) => report.errors.push(format!("Download {rel}: {e}")),
            }
        }
    }

    // 3. Upload local files Drive doesn't know about, and local edits to ones it does.
    let mut folders = remote.folders;
    let mut ids: Vec<String> = Vec::new();
    let now = SystemTime::now();
    for local in local_files(root).await {
        if held_back.contains(&local.rel) {
            continue;
        }
        // `changed` is the newer of modified and created time: File Explorer keeps a copied
        // file's original modified time, so that alone can't tell a copy still in progress.
        let settled = !local.busy && now.duration_since(local.changed).is_ok_and(|age| age >= SETTLE);
        if let Some(synced) = st.files.get(&local.rel).cloned() {
            // Already on Drive. Push local edits (a re-read or retyped sidecar, an image
            // edited in place), but only while Drive still has the version we last
            // synced. If another device changed it, step 2 already dealt with that.
            let remote_unchanged = remote
                .files
                .get(&local.rel)
                .is_some_and(|r| r.id == synced.id && r.md5_checksum == synced.md5);
            let maybe_edited =
                st.dirty.contains(&local.rel) || edited_after.is_none_or(|t| local.changed >= t);
            if !remote_unchanged || !maybe_edited {
                continue;
            }
            let Ok(local_md5) = md5_of(&local.path).await else { continue };
            if Some(&local_md5) == synced.md5.as_ref() {
                continue;
            }
            if !settled {
                report.retry_soon = true; // still being written; try again shortly
                continue;
            }
            let result = async {
                let bytes = tokio::fs::read(&local.path).await.map_err(|e| e.to_string())?;
                g.update_content(&synced.id, mime_for(&local.rel), bytes).await
            }
            .await;
            match result {
                Ok(item) => {
                    check_uploaded(&mut st, &mut report, &local, &item).await;
                    st.files.insert(local.rel.clone(), Synced { id: item.id, md5: item.md5_checksum });
                    report.counts.uploaded += 1;
                }
                Err(e) => report.errors.push(format!("Update {}: {e}", local.rel)),
            }
            continue;
        }
        if !settled {
            report.retry_soon = true;
            continue;
        }
        // A "(this device)" copy sync made earlier, whose contents Drive has anyway (e.g.
        // the conflict resolved itself): uploading it would only add a duplicate.
        if is_conflict_copy(&local.rel)
            && let Ok(md5) = md5_of(&local.path).await
            && on_drive.contains(&md5)
        {
            log::info!("sync: removing {}: identical to a file already on Drive", local.rel);
            match to_recycle_bin(&local.path) {
                Ok(()) => report.counts.removed += 1,
                Err(e) => report.errors.push(format!("Couldn't remove duplicate {}: {e}", local.rel)),
            }
            continue;
        }
        let (dir, name) = local.rel.rsplit_once('/').unwrap_or(("", &local.rel));
        let result = async {
            let parent = ensure_folder(g, &mut folders, dir).await?;
            let bytes = tokio::fs::read(&local.path).await.map_err(|e| e.to_string())?;
            let md5 = format!("{:x}", Md5::digest(&bytes));
            // Same file as an upload that may have gone through earlier: same id.
            let id = match st.pending.get(&local.rel) {
                Some(p) if p.md5 == md5 => p.id.clone(),
                _ => {
                    if ids.is_empty() {
                        ids = g.generate_ids(ID_BATCH).await?;
                    }
                    ids.pop().ok_or("Drive didn't reserve an id")?
                }
            };
            st.pending.insert(local.rel.clone(), PendingUpload { id: id.clone(), md5 });
            match g.upload(&parent, name, mime_for(name), bytes, &id).await {
                Ok(item) => Ok(item),
                // It may have arrived anyway (or on an earlier try): then that's our file.
                Err(e) => match g.find(&id).await {
                    Ok(Some(item)) => {
                        log::info!("sync: {} was already on Drive; not uploading it again", local.rel);
                        Ok(item)
                    }
                    _ => Err(e),
                },
            }
        }
        .await;
        match result {
            Ok(item) => {
                st.pending.remove(&local.rel);
                check_uploaded(&mut st, &mut report, &local, &item).await;
                st.files.insert(local.rel.clone(), Synced { id: item.id, md5: item.md5_checksum });
                report.counts.uploaded += 1;
            }
            Err(e) => report.errors.push(format!("Upload {}: {e}", local.rel)),
        }
    }
    st.dirty.retain(|rel| st.files.contains_key(rel));
    st.pending.retain(|rel, _| !st.files.contains_key(rel) && rel_path(root, rel).is_file());

    // Only move the change-feed bookmark past this pass if it fully succeeded. Otherwise
    // the next 30-second check sees "changes" and retries the failures right away,
    // instead of waiting for the ~10-minute safety-net sync.
    if report.errors.is_empty() {
        st.page_token = fresh_token;
    }
    st.last_scan = Some(started);
    if let Err(e) = st.save(data_dir) {
        report.errors.push(e);
    }
    report
}

/// A sidecar changed on Drive *and* here. Drive's copy wins, except for text the user
/// typed on this device, which is kept (unless it was also retyped elsewhere). Returns
/// true if the merged sidecar was written locally (and still needs uploading), false
/// if Drive's copy should simply be downloaded.
async fn merge_sidecar<D: Drive>(g: &D, item: &DriveItem, path: &Path) -> Result<bool, String> {
    let local: Option<sidecar::Sidecar> =
        tokio::fs::read(path).await.ok().and_then(|b| serde_json::from_slice(&b).ok());
    let Some(local_ocr) = local.and_then(|sc| sc.ocr).filter(|o| o.edited) else {
        return Ok(false);
    };
    let bytes = g.download(&item.id).await?;
    let Ok(mut merged) = serde_json::from_slice::<sidecar::Sidecar>(&bytes) else {
        return Ok(false); // Drive's copy is unreadable; downloading it as-is changes nothing
    };
    if merged.ocr.as_ref().is_some_and(|o| o.edited) {
        return Ok(false); // retyped on another device too: the newest edit (Drive's) wins
    }
    merged.ocr = Some(local_ocr);
    let json = serde_json::to_vec_pretty(&merged).map_err(|e| e.to_string())?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        crate::config::write_atomic(&path, &json)?;
        note_own_write(&path);
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Sends a file to the Recycle Bin / Trash (tests delete it outright, so they don't
/// fill up the real Trash).
fn to_recycle_bin(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    return std::fs::remove_file(path).map_err(|e| e.to_string());
    #[cfg(not(test))]
    return trash::delete(path).map_err(|e| e.to_string());
}

/// Moves a local file out of the way (`cat.png` -> `cat (this device).png`).
fn set_aside(path: &Path) -> std::io::Result<()> {
    let to = conflict_path(path);
    std::fs::rename(path, &to)?;
    log::info!("sync: kept local {} as {}", path.display(), to.display());
    note_own_write(path);
    note_own_write(&to);
    Ok(())
}

/// After an upload: if the file on disk no longer matches what Drive got (it was still
/// being written), mark it so the next pass uploads it again, whatever its timestamps.
async fn check_uploaded(st: &mut SyncState, report: &mut Report, local: &LocalFile, item: &DriveItem) {
    let now = md5_of(&local.path).await.ok();
    if item.md5_checksum.is_some() && now != item.md5_checksum {
        log::info!("sync: {} changed while uploading; will upload again", local.rel);
        st.dirty.insert(local.rel.clone());
        report.retry_soon = true;
    } else {
        st.dirty.remove(&local.rel);
    }
}

/// A file sync renamed out of the way in a conflict: `cat (this device).png`,
/// `cat (this device 2).png`, also after Drive added its own ` (1AbCdE)` suffix.
fn is_conflict_copy(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.contains(" (this device)") || name.contains(" (this device ")
}

/// True if a file with exactly this name (same letter case) exists. On case-insensitive
/// filesystems (Windows, macOS) `Path::exists` also matches `Cat.png` for `cat.png`.
fn exists_exact(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return path.exists() };
    std::fs::read_dir(dir).is_ok_and(|entries| entries.flatten().any(|e| e.file_name() == name))
}

/// `a` and `b` differ only in letter case, and the filesystem treats them as one file.
fn same_file_other_case(a: &Path, b: &Path) -> bool {
    a != b && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase() && !exists_exact(b)
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
        if !from.is_file() {
            // Nothing here to move: forget the old entry, and step 2 treats the new path
            // like any other (download it, or adopt/set aside whatever is already there).
            st.files.remove(&old);
            continue;
        }
        let case_only = same_file_other_case(&from, &to);
        if to.exists() && !case_only {
            // An unrelated local file is in the way: keep it under another name.
            if let Err(e) = set_aside(&to) {
                report.errors.push(format!("Couldn't rename {old} to {new}: {e}"));
                continue;
            }
        }
        let moved = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::rename(&from, &to));
        if let Err(e) = moved {
            report.errors.push(format!("Couldn't rename {old} to {new}: {e}"));
            continue;
        }
        note_own_write(&from);
        note_own_write(&to);
        if let Some(entry) = st.files.remove(&old) {
            st.files.insert(new, entry);
        }
    }
}

/// Renames on Drive whatever was renamed locally. Returns the paths (old and new) of
/// renames Drive refused, which the rest of this pass leaves alone so a failed rename
/// doesn't turn into a duplicate upload plus a re-download of the old name.
async fn push_local_renames<D: Drive>(
    g: &D,
    root: &Path,
    remote: &mut RemoteTree,
    st: &mut SyncState,
    report: &mut Report,
) -> HashSet<String> {
    let mut held_back = HashSet::new();
    // Synced files that are missing locally but still on Drive, unchanged, at the same
    // path. "Missing" means no file with exactly that name, so a case-only rename
    // (`Cat.png` -> `cat.png`) counts too.
    let missing: Vec<(String, Synced)> = st
        .files
        .iter()
        .filter(|(rel, s)| {
            s.md5.is_some() && remote.files.get(*rel).is_some_and(|r| r.id == s.id) && !exists_exact(&rel_path(root, rel))
        })
        .map(|(rel, s)| (rel.clone(), s.clone()))
        .collect();
    if missing.is_empty() {
        return held_back;
    }

    // Local files memeity hasn't seen before, by content hash.
    let mut untracked: HashMap<String, String> = HashMap::new();
    for meme in scan(root).await.memes {
        if !st.files.contains_key(&meme.rel)
            && !remote.files.contains_key(&meme.rel)
            && let Ok(md5) = md5_of(&meme.path).await
        {
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
            Err(e) => {
                report.errors.push(format!("Rename {old} to {new} on Drive: {e}"));
                held_back.insert(old);
                held_back.insert(new);
            }
        }
    }
    held_back
}

/// Renames a meme on this device and on Drive right away (under the sync lock, so it
/// can't interleave with a sync pass). If Drive can't be reached, the next sync spots
/// the rename (same bytes, new name) and finishes it.
pub async fn rename(app: &AppHandle, from: &Path, to: &Path) -> Result<(), String> {
    let state = app.state::<AppState>();
    let (folder, root) = sync_target(app).ok_or("No meme folder selected.")?;
    let rel_of = |p: &Path| -> Result<String, String> {
        Ok(p.strip_prefix(&root)
            .map_err(|_| "That file isn't in your meme folder.".to_string())?
            .to_string_lossy()
            .replace('\\', "/"))
    };
    let (old_rel, new_rel) = (rel_of(from)?, rel_of(to)?);
    let new_name = to.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("Give it a name.")?;
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;

    let _guard = state.sync_lock.lock().await;
    if to.exists() && !same_file_other_case(from, to) {
        return Err(format!("There's already a meme called {new_name}."));
    }
    tokio::fs::rename(from, to).await.map_err(|e| format!("Couldn't rename: {e}"))?;

    let mut st = SyncState::load(&data_dir, &folder.id);
    let Some(synced) = st.files.get(&old_rel).cloned() else { return Ok(()) };
    if !state.google.signed_in() {
        return Ok(());
    }
    match tokio::time::timeout(Duration::from_secs(10), state.google.rename(&synced.id, &new_name, None)).await {
        Ok(Ok(item)) => {
            st.files.remove(&old_rel);
            st.files.insert(new_rel, Synced { id: item.id, md5: item.md5_checksum });
            st.save(&data_dir)?;
        }
        Ok(Err(e)) => log::warn!("rename on Drive failed, sync will retry: {e}"),
        Err(_) => log::warn!("rename on Drive timed out, sync will retry"),
    }
    Ok(())
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
        log::warn!("couldn't move {} to the Recycle Bin: {e}", held.display());
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

async fn remote_tree<D: Drive>(g: &D, root_id: &str) -> Result<RemoteTree, String> {
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

async fn ensure_folder<D: Drive>(g: &D, folders: &mut HashMap<String, String>, dir: &str) -> Result<String, String> {
    if let Some(id) = folders.get(dir) {
        return Ok(id.clone());
    }
    let (parent_dir, name) = dir.rsplit_once('/').unwrap_or(("", dir));
    let parent = Box::pin(ensure_folder(g, folders, parent_dir)).await?;
    let id = g.create_folder(&parent, name).await?;
    folders.insert(dir.to_string(), id.clone());
    Ok(id)
}

async fn download<D: Drive>(g: &D, item: &DriveItem, root: &Path, dest: &Path) -> Result<(), String> {
    let bytes = g.download(&item.id).await?;
    let tmp = root.join(TMP_DIR).join(&item.id);
    let dest = dest.to_path_buf();
    let modified = item.modified_time.as_deref().and_then(|t| humantime::parse_rfc3339_weak(t).ok());
    tokio::task::spawn_blocking(move || {
        std::fs::write(&tmp, &bytes)?;
        // Keep Drive's modified time so "newest first" means newest on Drive.
        if let Some(t) = modified
            && let Ok(f) = std::fs::File::options().write(true).open(&tmp)
        {
            let _ = f.set_modified(t);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&tmp, &dest)?;
        note_own_write(&dest);
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
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
    /// The newer of its modified and created times.
    changed: SystemTime,
    /// Another program (e.g. File Explorer mid-copy) has it open for writing.
    busy: bool,
}

fn local_file(rel: String, path: PathBuf, meta: &std::fs::Metadata) -> LocalFile {
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    let changed = meta.created().map_or(modified, |c| c.max(modified));
    let busy = being_written(&path);
    LocalFile { rel, path, changed, busy }
}

/// Windows: a file another program is still writing can't be opened exclusively.
#[cfg(windows)]
fn being_written(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .is_err_and(|e| e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

#[cfg(not(windows))]
fn being_written(_path: &Path) -> bool {
    false
}

/// `Library::scan`, off the async runtime's worker threads.
async fn scan(root: &Path) -> Library {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || Library::scan(&root)).await.unwrap_or_default()
}

async fn local_files(root: &Path) -> Vec<LocalFile> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || local_files_blocking(&root)).await.unwrap_or_default()
}

fn local_files_blocking(root: &Path) -> Vec<LocalFile> {
    let mut files: Vec<LocalFile> = Library::scan(root)
        .memes
        .into_iter()
        .filter_map(|m| {
            let meta = std::fs::metadata(&m.path).ok()?;
            Some(local_file(m.rel, m.path, &meta))
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir(root.join(META_DIR)) {
        for entry in entries.flatten() {
            let rel = format!("{META_DIR}/{}", entry.file_name().to_string_lossy());
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_file() && sidecar::is_sidecar_rel(&rel) {
                files.push(local_file(rel, entry.path(), &meta));
            }
        }
    }
    files
}

/// `file_md5`, off the async runtime's worker threads.
async fn md5_of(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || file_md5(&path)).await.map_err(std::io::Error::other)?
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

    // ---------- whole sync passes against a fake Drive ----------

    #[derive(Clone)]
    struct FakeFile {
        parent: String,
        name: String,
        mime: String,
        bytes: Vec<u8>,
    }

    /// An in-memory Drive. Folder "root" is the synced folder.
    #[derive(Default)]
    struct FakeDrive {
        files: Mutex<HashMap<String, FakeFile>>,
        next_id: Mutex<u32>,
        fail_downloads: bool,
        renames: Mutex<u32>,
        /// Runs during an upload, e.g. to finish writing the local file meanwhile.
        on_upload: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// This many uploads reach Drive but report an error (connection dropped).
        lost_replies: Mutex<u32>,
        /// `find` fails (offline) while set.
        find_offline: Mutex<bool>,
        /// Ids left out of listings, like Drive's listing lagging behind.
        unlisted: Mutex<HashSet<String>>,
        /// Upload attempts, successful or not.
        uploads: Mutex<u32>,
    }

    impl FakeDrive {
        fn add(&self, parent: &str, name: &str, bytes: &[u8]) -> String {
            let mime = if name.ends_with(".json") { "application/json" } else { "image/png" };
            self.insert(FakeFile { parent: parent.into(), name: name.into(), mime: mime.into(), bytes: bytes.to_vec() })
        }
        fn insert(&self, f: FakeFile) -> String {
            let id = self.new_id();
            self.files.lock().unwrap().insert(id.clone(), f);
            id
        }
        fn new_id(&self) -> String {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            format!("id{n:06}")
        }
        fn item(id: &str, f: &FakeFile) -> DriveItem {
            let is_folder = f.mime == FOLDER_MIME;
            DriveItem {
                id: id.into(),
                name: f.name.clone(),
                mime_type: f.mime.clone(),
                md5_checksum: (!is_folder).then(|| format!("{:x}", Md5::digest(&f.bytes))),
                modified_time: None,
            }
        }
        fn set_bytes(&self, id: &str, bytes: &[u8]) {
            self.files.lock().unwrap().get_mut(id).unwrap().bytes = bytes.to_vec();
        }
        fn rename_to(&self, id: &str, name: &str) {
            self.files.lock().unwrap().get_mut(id).unwrap().name = name.into();
        }
        fn get(&self, id: &str) -> FakeFile {
            self.files.lock().unwrap()[id].clone()
        }
        fn names(&self) -> Vec<String> {
            let mut v: Vec<String> = self.files.lock().unwrap().values().map(|f| f.name.clone()).collect();
            v.sort();
            v
        }
    }

    impl Drive for FakeDrive {
        async fn list_children(&self, parent: &str) -> Result<Vec<DriveItem>, String> {
            let files = self.files.lock().unwrap();
            let unlisted = self.unlisted.lock().unwrap();
            Ok(files
                .iter()
                .filter(|(id, f)| f.parent == parent && !unlisted.contains(*id))
                .map(|(id, f)| Self::item(id, f))
                .collect())
        }
        async fn download(&self, id: &str) -> Result<Vec<u8>, String> {
            if self.fail_downloads {
                return Err("offline".into());
            }
            Ok(self.get(id).bytes)
        }
        async fn upload(&self, parent: &str, name: &str, mime: &str, bytes: Vec<u8>, id: &str) -> Result<DriveItem, String> {
            *self.uploads.lock().unwrap() += 1;
            if let Some(hook) = self.on_upload.lock().unwrap().take() {
                hook();
            }
            if self.files.lock().unwrap().contains_key(id) {
                return Err("409: a file already exists with the provided id".into());
            }
            let f = FakeFile { parent: parent.into(), name: name.into(), mime: mime.into(), bytes };
            self.files.lock().unwrap().insert(id.to_string(), f.clone());
            let mut lost = self.lost_replies.lock().unwrap();
            if *lost > 0 {
                *lost -= 1;
                return Err("connection reset".into());
            }
            Ok(Self::item(id, &f))
        }
        async fn generate_ids(&self, count: u32) -> Result<Vec<String>, String> {
            Ok((0..count).map(|_| self.new_id()).collect())
        }
        async fn find(&self, id: &str) -> Result<Option<DriveItem>, String> {
            if *self.find_offline.lock().unwrap() {
                return Err("offline".into());
            }
            Ok(self.files.lock().unwrap().get(id).map(|f| Self::item(id, f)))
        }
        async fn create_folder(&self, parent: &str, name: &str) -> Result<String, String> {
            Ok(self.insert(FakeFile { parent: parent.into(), name: name.into(), mime: FOLDER_MIME.into(), bytes: vec![] }))
        }
        async fn rename(&self, id: &str, new_name: &str, reparent: Option<(&str, &str)>) -> Result<DriveItem, String> {
            *self.renames.lock().unwrap() += 1;
            let mut files = self.files.lock().unwrap();
            let f = files.get_mut(id).ok_or("no such file")?;
            f.name = new_name.into();
            if let Some((to, _)) = reparent {
                f.parent = to.into();
            }
            Ok(Self::item(id, f))
        }
        async fn update_content(&self, id: &str, _mime: &str, bytes: Vec<u8>) -> Result<DriveItem, String> {
            let mut files = self.files.lock().unwrap();
            let f = files.get_mut(id).ok_or("no such file")?;
            f.bytes = bytes;
            Ok(Self::item(id, f))
        }
        async fn start_page_token(&self) -> Result<String, String> {
            Ok(format!("token-{}", now_ms()))
        }
    }

    /// A fresh library folder and app-data dir.
    fn dirs(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("memeity-{name}-{}-{}", std::process::id(), now_ms()));
        let (root, data) = (base.join("lib"), base.join("data"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        (root, data)
    }

    /// Writes a local file that looks "settled" (old enough for sync to touch) but
    /// still newer than the previous sync. A file's creation time can't be backdated
    /// (macOS moves it back along with the modified time, Windows doesn't), so a
    /// recently created file waits out SETTLE for real.
    fn write_settled(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        let t = SystemTime::now() - SETTLE - Duration::from_millis(500);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
        let created = std::fs::metadata(path).unwrap().created().unwrap_or(UNIX_EPOCH);
        let age = SystemTime::now().duration_since(created).unwrap_or_default();
        if age < SETTLE {
            std::thread::sleep(SETTLE - age + Duration::from_millis(200));
        }
    }

    /// Sets a file's modified time to long ago, as File Explorer does when copying.
    fn backdate(path: &Path) {
        let t = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[tokio::test]
    async fn first_sync_downloads_and_uploads() {
        let (root, data) = dirs("first");
        let g = FakeDrive::default();
        g.add("root", "remote.png", b"from drive");
        write_settled(&root.join("local.png"), b"from here");

        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(std::fs::read(root.join("remote.png")).unwrap(), b"from drive");
        assert_eq!(g.names(), ["local.png", "remote.png"]);
        cleanup(&root);
    }

    #[tokio::test]
    async fn edited_on_both_sides_keeps_both() {
        let (root, data) = dirs("conflict");
        let g = FakeDrive::default();
        let id = g.add("root", "cat.png", b"v1");
        cycle(&g, "root", &root, &data).await;

        write_settled(&root.join("cat.png"), b"edited here");
        g.set_bytes(&id, b"edited on drive");
        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(std::fs::read(root.join("cat.png")).unwrap(), b"edited on drive");
        assert_eq!(std::fs::read(root.join("cat (this device).png")).unwrap(), b"edited here");
        assert!(g.names().contains(&"cat (this device).png".to_string()), "local edit uploaded too");
        cleanup(&root);
    }

    #[tokio::test]
    async fn local_edit_uploads_in_place() {
        let (root, data) = dirs("edit");
        let g = FakeDrive::default();
        let id = g.add("root", "cat.png", b"v1");
        cycle(&g, "root", &root, &data).await;

        write_settled(&root.join("cat.png"), b"cropped");
        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(g.get(&id).bytes, b"cropped");
        assert_eq!(g.names(), ["cat.png"], "updated, not uploaded as a second file");
        cleanup(&root);
    }

    #[tokio::test]
    async fn unsettled_edit_retries_soon() {
        let (root, data) = dirs("unsettled");
        let g = FakeDrive::default();
        let id = g.add("root", "cat.png", b"v1");
        cycle(&g, "root", &root, &data).await;

        std::fs::write(root.join("cat.png"), b"just written").unwrap();
        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.retry_soon, "a fresh local edit must schedule a quick retry");
        assert_eq!(g.get(&id).bytes, b"v1");
        cleanup(&root);
    }

    fn sidecar_json(text: &str, edited: bool) -> Vec<u8> {
        let sc = sidecar::Sidecar {
            ocr: Some(sidecar::Ocr { text: text.into(), engine: "test".into(), lang: None, at: None, edited }),
            ..Default::default()
        };
        serde_json::to_vec_pretty(&sc).unwrap()
    }

    #[tokio::test]
    async fn typed_text_survives_a_remote_sidecar_change() {
        let (root, data) = dirs("sidecar");
        let g = FakeDrive::default();
        let meta = g.create_folder("root", META_DIR).await.unwrap();
        let id = g.add(&meta, "abc.json", &sidecar_json("ocr v1", false));
        cycle(&g, "root", &root, &data).await;

        // Typed here; meanwhile another device re-read the text with OCR.
        write_settled(&root.join(META_DIR).join("abc.json"), &sidecar_json("typed by me", true));
        g.set_bytes(&id, &sidecar_json("ocr v2", false));
        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let local: sidecar::Sidecar =
            serde_json::from_slice(&std::fs::read(root.join(META_DIR).join("abc.json")).unwrap()).unwrap();
        assert_eq!(local.ocr.unwrap().text, "typed by me");
        assert!(r.retry_soon, "the merged sidecar uploads on the next pass");
        cleanup(&root);
    }

    #[tokio::test]
    async fn local_rename_renames_on_drive() {
        let (root, data) = dirs("rename");
        let g = FakeDrive::default();
        let id = g.add("root", "Cat.png", b"meow");
        cycle(&g, "root", &root, &data).await;

        // Case-only on purpose: on Windows/macOS "Cat.png" still "exists" afterwards.
        std::fs::rename(root.join("Cat.png"), root.join("cat.png")).unwrap();
        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(g.get(&id).name, "cat.png");
        assert_eq!(g.names(), ["cat.png"], "renamed, no duplicate upload");
        cleanup(&root);
    }

    #[tokio::test]
    async fn drive_rename_onto_an_occupied_name_keeps_both() {
        let (root, data) = dirs("occupied");
        let g = FakeDrive::default();
        let id = g.add("root", "a.png", b"synced");
        cycle(&g, "root", &root, &data).await;

        std::fs::write(root.join("b.png"), b"unrelated").unwrap();
        g.rename_to(&id, "b.png");
        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(std::fs::read(root.join("b.png")).unwrap(), b"synced");
        assert_eq!(std::fs::read(root.join("b (this device).png")).unwrap(), b"unrelated");
        assert!(!root.join("a.png").exists());
        cleanup(&root);
    }

    #[tokio::test]
    async fn file_still_being_copied_doesnt_snowball_into_duplicates() {
        let (root, data) = dirs("half-copied");
        let g = FakeDrive::default();
        let cat = root.join("cat.png");
        // Explorer: the copy keeps the source's (old) modified time from the start.
        write_settled(&cat, b"half");
        backdate(&cat);
        // ...and finishes writing while the half-copied file is being uploaded.
        let finish = cat.clone();
        *g.on_upload.lock().unwrap() = Some(Box::new(move || {
            std::fs::write(&finish, b"the whole meme").unwrap();
            backdate(&finish);
        }));

        let first = cycle(&g, "root", &root, &data).await;
        assert!(first.retry_soon, "an upload that no longer matches the file is redone");
        for _ in 0..3 {
            let r = cycle(&g, "root", &root, &data).await;
            assert!(r.errors.is_empty(), "{:?}", r.errors);
        }

        assert_eq!(g.names(), ["cat.png"], "one file on Drive, no conflict copies");
        let on_drive = g.files.lock().unwrap().values().next().unwrap().bytes.clone();
        assert_eq!(on_drive, b"the whole meme");
        let local: Vec<String> =
            Library::scan(&root).memes.into_iter().map(|m| m.rel).collect();
        assert_eq!(local, ["cat.png"]);
        cleanup(&root);
    }

    #[tokio::test]
    async fn no_conflict_copy_when_drive_already_has_the_bytes() {
        let (root, data) = dirs("backed-up");
        let g = FakeDrive::default();
        // Drive has two different files; this device has dog.png's bytes under cat.png
        // (e.g. state was lost after a half-finished conflict).
        g.add("root", "cat.png", b"cat");
        g.add("root", "dog.png", b"dog");
        write_settled(&root.join("cat.png"), b"dog");

        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(g.names(), ["cat.png", "dog.png"], "nothing new uploaded");
        assert!(!root.join("cat (this device).png").exists());
        assert_eq!(std::fs::read(root.join("cat.png")).unwrap(), b"cat");
        cleanup(&root);
    }

    #[tokio::test]
    async fn leftover_conflict_copies_already_on_drive_are_removed() {
        let (root, data) = dirs("leftover");
        let g = FakeDrive::default();
        g.add("root", "cat.png", b"meow");
        write_settled(&root.join("cat (this device) (1AbCdE).png"), b"meow");

        let r = cycle(&g, "root", &root, &data).await;

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(g.names(), ["cat.png"], "the duplicate isn't uploaded");
        assert!(!root.join("cat (this device) (1AbCdE).png").exists());
        cleanup(&root);
    }

    #[test]
    fn recognizes_conflict_copies() {
        assert!(is_conflict_copy("a/cat (this device).png"));
        assert!(is_conflict_copy("cat (this device 3).png"));
        assert!(is_conflict_copy("cat (this device) (1AbCdE).png"));
        assert!(!is_conflict_copy("this device.png"));
    }

    #[tokio::test]
    async fn upload_whose_reply_was_lost_isnt_repeated() {
        let (root, data) = dirs("lost-reply");
        let g = FakeDrive::default();
        write_settled(&root.join("cat.png"), b"meow");
        // The upload reaches Drive but the reply is lost, and we can't check right away.
        *g.lost_replies.lock().unwrap() = 1;
        *g.find_offline.lock().unwrap() = true;
        let first = cycle(&g, "root", &root, &data).await;
        assert!(!first.errors.is_empty());

        // Next pass: Drive's listing hasn't caught up yet, so it looks missing.
        let id = g.files.lock().unwrap().keys().next().unwrap().clone();
        g.unlisted.lock().unwrap().insert(id.clone());
        *g.find_offline.lock().unwrap() = false;
        let second = cycle(&g, "root", &root, &data).await;
        assert!(second.errors.is_empty(), "{:?}", second.errors);

        g.unlisted.lock().unwrap().clear();
        let third = cycle(&g, "root", &root, &data).await;
        assert!(third.errors.is_empty(), "{:?}", third.errors);
        assert_eq!(g.names(), ["cat.png"], "retried with the same id: one file on Drive");
        assert_eq!(Library::scan(&root).memes.len(), 1, "no duplicate downloaded either");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_pass_keeps_the_old_bookmark() {
        let (root, data) = dirs("bookmark");
        let g = FakeDrive { fail_downloads: true, ..Default::default() };
        g.add("root", "cat.png", b"meow");

        let r = cycle(&g, "root", &root, &data).await;

        assert!(!r.errors.is_empty());
        assert_eq!(SyncState::load(&data, "root").page_token, None, "next check must retry, not skip");
        cleanup(&root);
    }

}
