//! User tags, synced across devices.
//!
//! Each tag is its own small file in the synced folder, `<root>/.meta/tag-<id>.json`,
//! so renaming or recolouring one tag never touches another, and two devices editing
//! different tags never conflict. A meme's sidecar lists the *ids* of its tags (see
//! `sidecar.rs`), so a rename is one file, not one per tagged meme.
//!
//! Deleting a tag doesn't remove its file (sync never deletes on Drive for a local
//! delete): it's rewritten as a tombstone (`"deleted": true`), which every device then
//! hides. The tag is also taken off every meme that had it.
//!
//! File format (version 1):
//! ```json
//! { "version": 1, "id": "a1b2c3d4", "name": "reaction", "color": "blue", "updated": 1793935817389 }
//! ```
//!
//! Each device mirrors which memes have which tags in its index (`meme_tags`), read
//! from the sidecars, so search never opens files.

use crate::config::write_atomic;
use crate::sidecar::{self, META_DIR};
use crate::{library_rel, sync, sync_target, AppState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager};

const VERSION: u32 = 1;
const PREFIX: &str = "tag-";
/// Longest tag name, in characters.
const MAX_NAME: usize = 32;

/// Colours a tag can have. New tags take the next one in turn (gray last).
pub const COLORS: [&str; 9] = ["blue", "green", "yellow", "orange", "red", "pink", "purple", "teal", "gray"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagFile {
    #[serde(default)]
    pub version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// When it was last changed (ms since the epoch).
    #[serde(default)]
    pub updated: i64,
    /// Anything a newer version wrote that we don't know about. Kept as-is.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A tag as the UI sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Tag {
    pub id: String,
    pub name: String,
    pub color: String,
    /// How many memes in the library have it.
    pub count: usize,
}

fn file_path(root: &Path, id: &str) -> PathBuf {
    root.join(META_DIR).join(format!("{PREFIX}{id}.json"))
}

/// The tag id a `.meta/` file name belongs to, if it's a tag file.
fn id_of(file_name: &str) -> Option<&str> {
    file_name
        .strip_prefix(PREFIX)?
        .strip_suffix(".json")
        .filter(|d| !d.is_empty() && d.len() <= 32 && d.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Tidies a typed tag name: lowercase, no leading `#`, spaces become `-`, and only
/// letters, digits, `-` and `_` are kept (so `#name` always works in the search box).
pub fn normalize(name: &str) -> Result<String, String> {
    let mut out = String::new();
    for c in name.trim().trim_start_matches('#').chars() {
        if c.is_whitespace() || c == '-' {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        } else if c.is_alphanumeric() || c == '_' {
            out.extend(c.to_lowercase());
        }
    }
    let out: String = out.trim_matches('-').chars().take(MAX_NAME).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        return Err("Tag names need at least one letter or number.".into());
    }
    Ok(out)
}

fn valid_color(color: &str) -> Result<(), String> {
    if COLORS.contains(&color) { Ok(()) } else { Err("Unknown colour.".into()) }
}

fn root(app: &AppHandle) -> Result<PathBuf, String> {
    sync_target(app).map(|(_, root)| root).ok_or_else(|| "memeity isn't set up yet.".to_string())
}

fn write(root: &Path, tag: &TagFile) -> Result<(), String> {
    let path = file_path(root, &tag.id);
    let bytes = serde_json::to_vec_pretty(tag).expect("tag serializes");
    std::fs::create_dir_all(path.parent().expect("has a parent"))
        .and_then(|_| write_atomic(&path, &bytes))
        .map_err(|e| format!("Couldn't save the tag: {e}"))
}

/// Reads every tag file into memory. Called at startup, after a sync brought something
/// new, and never on a keystroke. Tells the windows if anything changed.
pub fn load(app: &AppHandle) {
    let Some((_, root)) = sync_target(app) else { return };
    let mut live = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(root.join(META_DIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = id_of(&name) else { continue };
            match std::fs::read(entry.path()).map(|b| serde_json::from_slice::<TagFile>(&b)) {
                Ok(Ok(tag)) if tag.id == id && !tag.deleted => {
                    live.insert(tag.id.clone(), tag);
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => log::warn!("skipping unreadable {name}: {e}"),
                Err(_) => {}
            }
        }
    }
    let state = app.state::<AppState>();
    let changed = {
        let mut tags = state.tags.lock().unwrap();
        let changed = *tags != live;
        *tags = live;
        changed
    };
    if changed {
        let _ = app.emit("tags-changed", ());
    }
}

/// Every live tag, with how many memes have it, sorted by name.
fn list(app: &AppHandle) -> Vec<Tag> {
    let state = app.state::<AppState>();
    let counts = state
        .index
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|idx| idx.tag_counts().ok())
        .unwrap_or_default();
    let mut out: Vec<Tag> = state
        .tags
        .lock()
        .unwrap()
        .values()
        .map(|t| Tag {
            id: t.id.clone(),
            name: t.name.clone(),
            color: t.color.clone(),
            count: counts.get(&t.id).copied().unwrap_or(0),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Id -> name of every live tag (for search).
pub fn names(app: &AppHandle) -> HashMap<String, String> {
    app.state::<AppState>().tags.lock().unwrap().values().map(|t| (t.id.clone(), t.name.clone())).collect()
}

fn find_by_name(app: &AppHandle, name: &str) -> Option<TagFile> {
    app.state::<AppState>().tags.lock().unwrap().values().find(|t| t.name == name).cloned()
}

fn changed(app: &AppHandle) {
    let _ = app.emit("tags-changed", ());
    sync::trigger(app); // upload the tag file / sidecars
}

#[tauri::command]
pub fn list_tags(app: AppHandle) -> Vec<Tag> {
    list(&app)
}

/// Makes a tag (or returns the existing one with that name).
#[tauri::command]
pub fn create_tag(app: AppHandle, name: String, color: Option<String>) -> Result<Tag, String> {
    let root = root(&app)?;
    let name = normalize(&name)?;
    if let Some(existing) = find_by_name(&app, &name) {
        return list(&app).into_iter().find(|t| t.id == existing.id).ok_or_else(|| "Tag vanished.".into());
    }
    let color = match color {
        Some(c) => {
            valid_color(&c)?;
            c
        }
        None => {
            let n = app.state::<AppState>().tags.lock().unwrap().len();
            COLORS[n % (COLORS.len() - 1)].to_string() // gray only when picked
        }
    };
    let tag = TagFile {
        version: VERSION,
        id: format!("{:08x}", rand::random::<u32>()),
        name,
        color,
        deleted: false,
        updated: crate::now_ms(),
        extra: serde_json::Map::new(),
    };
    write(&root, &tag)?;
    app.state::<AppState>().tags.lock().unwrap().insert(tag.id.clone(), tag.clone());
    changed(&app);
    Ok(Tag { id: tag.id, name: tag.name, color: tag.color, count: 0 })
}

/// Renames and/or recolours a tag.
#[tauri::command]
pub fn update_tag(app: AppHandle, id: String, name: Option<String>, color: Option<String>) -> Result<Vec<Tag>, String> {
    let root = root(&app)?;
    let mut tag = app.state::<AppState>().tags.lock().unwrap().get(&id).cloned().ok_or("That tag doesn't exist anymore.")?;
    if let Some(name) = name {
        let name = normalize(&name)?;
        if find_by_name(&app, &name).is_some_and(|t| t.id != id) {
            return Err(format!("There's already a tag called “{name}”."));
        }
        tag.name = name;
    }
    if let Some(color) = color {
        valid_color(&color)?;
        tag.color = color;
    }
    tag.version = VERSION;
    tag.updated = crate::now_ms();
    write(&root, &tag)?;
    app.state::<AppState>().tags.lock().unwrap().insert(id, tag);
    changed(&app);
    Ok(list(&app))
}

/// Deletes a tag everywhere: leaves a tombstone (so other devices drop it too) and
/// takes it off every meme.
#[tauri::command(async)]
pub fn delete_tag(app: AppHandle, id: String) -> Result<Vec<Tag>, String> {
    let root = root(&app)?;
    let mut tag = app.state::<AppState>().tags.lock().unwrap().get(&id).cloned().ok_or("That tag doesn't exist anymore.")?;
    let md5s = {
        let state = app.state::<AppState>();
        let index = state.index.lock().unwrap();
        index.as_ref().map(|idx| idx.memes_with_tag(&id)).transpose().map_err(|e| e.to_string())?.unwrap_or_default()
    };
    for md5 in md5s {
        edit_sidecar(&app, &root, &md5, |tags| tags.retain(|t| t != &id))?;
    }
    tag.deleted = true;
    tag.updated = crate::now_ms();
    write(&root, &tag)?;
    app.state::<AppState>().tags.lock().unwrap().remove(&id);
    changed(&app);
    let _ = app.emit("library-changed", ());
    Ok(list(&app))
}

/// Adds and/or removes tags (by id) on one or many memes at once.
#[tauri::command(async)]
pub fn set_tags(app: AppHandle, paths: Vec<String>, add: Vec<String>, remove: Vec<String>) -> Result<(), String> {
    let root = root(&app)?;
    let state = app.state::<AppState>();
    if let Some(missing) = add.iter().find(|id| !state.tags.lock().unwrap().contains_key(*id)) {
        return Err(format!("Unknown tag {missing}."));
    }
    let mut md5s = Vec::new();
    let mut seen = HashSet::new();
    for path in &paths {
        let (_, rel) = library_rel(&app, Path::new(path))?;
        let md5 = state
            .index
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|idx| idx.lookup(&rel).ok().flatten())
            .map(|e| e.md5)
            .ok_or("memeity is still indexing some of these memes. Try again in a moment.")?;
        if seen.insert(md5.clone()) {
            md5s.push(md5);
        }
    }
    for md5 in md5s {
        edit_sidecar(&app, &root, &md5, |tags| {
            tags.retain(|t| !remove.contains(t));
            for id in &add {
                if !tags.contains(id) {
                    tags.push(id.clone());
                }
            }
        })?;
    }
    changed(&app);
    Ok(())
}

/// Changes one meme's tag list in its sidecar (if it actually changes) and the index.
fn edit_sidecar(app: &AppHandle, root: &Path, md5: &str, edit: impl FnOnce(&mut Vec<String>)) -> Result<(), String> {
    let mut sc = sidecar::read(root, md5).unwrap_or_default();
    let before = sc.tags.clone();
    edit(&mut sc.tags);
    if sc.tags == before {
        return Ok(());
    }
    sidecar::write(root, md5, &sc).map_err(|e| format!("Couldn't save: {e}"))?;
    if let Some(idx) = app.state::<AppState>().index.lock().unwrap().as_ref() {
        idx.set_meme_tags(md5, &sc.tags).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The query with its `#tag` words taken out, and those words (without the `#`).
pub fn split_query(query: &str) -> (String, Vec<String>) {
    let mut words = Vec::new();
    let mut tags = Vec::new();
    for w in query.split_whitespace() {
        match w.strip_prefix('#') {
            Some(t) => {
                let t = t.to_lowercase();
                if !t.is_empty() {
                    tags.push(t);
                }
            }
            None => words.push(w),
        }
    }
    (words.join(" "), tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize("  #Reaction Images ").unwrap(), "reaction-images");
        assert_eq!(normalize("cat!!  -- dog").unwrap(), "cat-dog");
        assert_eq!(normalize("Ünïcode_ok").unwrap(), "ünïcode_ok");
        assert!(normalize(" # !! ").is_err());
        assert_eq!(normalize(&"a".repeat(50)).unwrap().len(), MAX_NAME);
    }

    #[test]
    fn recognises_tag_files() {
        assert_eq!(id_of("tag-a1b2c3d4.json"), Some("a1b2c3d4"));
        assert_eq!(id_of("tag-.json"), None);
        assert_eq!(id_of("tag-../x.json"), None);
        assert_eq!(id_of("usage-abc.json"), None);
    }

    #[test]
    fn splits_tags_out_of_queries() {
        assert_eq!(split_query("when #Cat the #"), ("when the".to_string(), vec!["cat".to_string()]));
        assert_eq!(split_query("#a #b"), (String::new(), vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn reads_files_and_keeps_unknown_fields() {
        let t: TagFile = serde_json::from_str(r#"{"id":"ab","name":"x","emoji":"🐱"}"#).unwrap();
        assert!(!t.deleted && t.color.is_empty());
        let out = serde_json::to_value(&t).unwrap();
        assert_eq!(out["emoji"], "🐱");
        assert!(out.get("deleted").is_none());
    }
}
