//! Adding memes from outside: files or images dropped on the pane, images pasted with
//! Ctrl/Cmd+V, and URLs (a direct image link, or a web page, whose preview image we use).
//!
//! Imported files land in the library folder like any other new meme, so the watcher,
//! OCR and sync pick them up as usual.

use crate::{refresh_library, sync, sync_target, AppState};
use md5::{Digest, Md5};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Manager};

/// Refuse anything bigger than this (memes, not movies).
const MAX_BYTES: usize = 50 * 1024 * 1024;
const TMP_DIR: &str = ".memeity-tmp";

#[derive(Serialize, Default)]
pub struct ImportResult {
    /// Library-relative paths of memes that were added.
    pub added: Vec<String>,
    /// Library-relative paths of existing memes that were identical to an import.
    pub duplicates: Vec<String>,
}

/// File extension for a supported image, from its bytes (not its name).
fn extension_for(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Some("png"),
        image::ImageFormat::Jpeg => Some("jpg"),
        image::ImageFormat::Gif => Some("gif"),
        image::ImageFormat::WebP => Some("webp"),
        image::ImageFormat::Bmp => Some("bmp"),
        _ => None,
    }
}

/// A clean, filesystem-safe base name (no extension), or `fallback`.
fn clean_stem(hint: &str, fallback: &str) -> String {
    let name = hint.rsplit(['/', '\\']).next().unwrap_or(hint);
    let stem = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    let stem = crate::sync::sanitize(stem);
    let stem: String = stem.chars().take(80).collect();
    if stem.trim().is_empty() { fallback.to_string() } else { stem.trim().to_string() }
}

/// `root/stem.ext`, or `root/stem (2).ext`, ... whichever is free.
fn free_path(root: &Path, stem: &str, ext: &str) -> PathBuf {
    (1..)
        .map(|n| {
            let name = if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
            root.join(name)
        })
        .find(|p| !p.exists())
        .expect("some name is free")
}

fn pasted_name() -> String {
    let now = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
    // 2026-11-04T18:52:03Z -> "pasted 2026-11-04 18.52.03"
    format!("pasted {}", now.trim_end_matches('Z').replace('T', " ").replace(':', "."))
}

/// Adds one image to the library. `name_hint` is a filename or URL path, if known.
pub fn add_bytes(app: &AppHandle, name_hint: Option<&str>, bytes: &[u8]) -> Result<ImportResult, String> {
    let (_, root) = sync_target(app).ok_or("No meme folder yet. Finish setting up memeity first.")?;
    if bytes.len() > MAX_BYTES {
        return Err("That file is too big (over 50 MB).".into());
    }
    let ext = extension_for(bytes).ok_or("That isn't a supported image (PNG, JPG, GIF, WebP or BMP).")?;

    // Same bytes already in the library? Don't add a copy.
    let md5 = format!("{:x}", Md5::digest(bytes));
    let existing = app
        .state::<AppState>()
        .index
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|idx| idx.rel_for_md5(&md5).ok().flatten());
    if let Some(rel) = existing {
        return Ok(ImportResult { duplicates: vec![rel], ..Default::default() });
    }

    let stem = clean_stem(name_hint.unwrap_or(""), &pasted_name());
    let dest = free_path(&root, &stem, ext);
    // Write to a hidden temp file, then rename: the watcher/sync never see a partial file.
    let tmp_dir = root.join(TMP_DIR);
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
    let tmp = tmp_dir.join(format!("import-{md5}.part"));
    std::fs::write(&tmp, bytes).map_err(|e| format!("Couldn't save the meme: {e}"))?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("Couldn't save the meme: {e}"))?;

    let rel = dest.strip_prefix(&root).unwrap_or(&dest).to_string_lossy().replace('\\', "/");
    Ok(ImportResult { added: vec![rel], ..Default::default() })
}

/// After a batch of imports: show them right away and start indexing/uploading.
fn finish(app: &AppHandle, result: &ImportResult) {
    if !result.added.is_empty() {
        refresh_library(app);
        sync::trigger(app);
    }
}

// ---------- URLs ----------

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("memeity (https://memeity.stelle.codes)")
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client")
}

/// Downloads `url` (capped at MAX_BYTES). Returns the bytes, content type and final URL.
async fn fetch(client: &reqwest::Client, url: &reqwest::Url) -> Result<(Vec<u8>, String, reqwest::Url), String> {
    let mut resp = client.get(url.clone()).send().await.map_err(|e| format!("Couldn't download that link: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("That link returned {}.", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
        return Err("That file is too big (over 50 MB).".into());
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let final_url = resp.url().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("Download failed: {e}"))? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_BYTES {
            return Err("That file is too big (over 50 MB).".into());
        }
    }
    Ok((bytes, content_type, final_url))
}

/// The preview image a page advertises (`og:image`, else `twitter:image`).
fn page_image(html: &str) -> Option<String> {
    for key in ["og:image:secure_url", "og:image", "twitter:image"] {
        let mut from = 0;
        while let Some(pos) = html[from..].find(key).map(|p| p + from) {
            from = pos + key.len();
            // The surrounding <meta ...> tag.
            let start = html[..pos].rfind('<')?;
            let end = pos + html[pos..].find('>')?;
            let tag = &html[start..end];
            if !tag.to_ascii_lowercase().starts_with("<meta") {
                continue;
            }
            // Make sure the key is the property/name (not part of another attribute).
            let quoted = [format!("\"{key}\""), format!("'{key}'")];
            if !quoted.iter().any(|q| tag.contains(q.as_str())) {
                continue;
            }
            if let Some(content) = attr(tag, "content") {
                return Some(content.replace("&amp;", "&"));
            }
        }
    }
    None
}

/// The value of `name="..."` (or single-quoted) in an HTML tag.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let quote = tag[at..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &tag[at + 1..];
    Some(rest[..rest.find(quote)?].to_string())
}

/// Adds the image at `url`. Web pages (tweets, Reddit, Imgur...) use their preview image.
pub async fn add_url(app: &AppHandle, url: &str) -> Result<ImportResult, String> {
    let url = reqwest::Url::parse(url.trim()).map_err(|_| "That doesn't look like a link.".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Only http(s) links can be added.".into());
    }
    let client = client();
    let (mut bytes, content_type, mut final_url) = fetch(&client, &url).await?;

    if extension_for(&bytes).is_none() && content_type.contains("html") {
        let html = String::from_utf8_lossy(&bytes);
        let image = page_image(&html).ok_or("That page doesn't have an image memeity can find.")?;
        let image_url = final_url.join(&image).map_err(|_| "That page's image link is broken.".to_string())?;
        (bytes, _, final_url) = fetch(&client, &image_url).await?;
    }

    let hint = final_url
        .path_segments()
        .and_then(|mut s| s.next_back().map(str::to_owned))
        .filter(|s| !s.is_empty())
        .map(|s| crate::thumbs::percent_decode_str(&s))
        .unwrap_or_else(|| format!("image from {}", final_url.host_str().unwrap_or("the web")));
    add_bytes(app, Some(&hint), &bytes)
}

// ---------- commands ----------

/// Raw image bytes in the request body (from a drop or a paste); the original filename,
/// if any, comes percent-encoded in the `x-filename` header.
#[tauri::command]
pub fn import_bytes(app: AppHandle, request: tauri::ipc::Request<'_>) -> Result<ImportResult, String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("Expected raw image bytes.".into());
    };
    let name = request
        .headers()
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .map(crate::thumbs::percent_decode_str);
    let result = add_bytes(&app, name.as_deref(), bytes)?;
    finish(&app, &result);
    Ok(result)
}

#[tauri::command]
pub async fn import_url(app: AppHandle, url: String) -> Result<ImportResult, String> {
    let result = add_url(&app, &url).await?;
    finish(&app, &result);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_page_preview_images() {
        let html = r#"<html><head><meta property="og:title" content="x">
            <meta property="og:image" content="https://i.example.com/a.jpg?x=1&amp;y=2" /></head>"#;
        assert_eq!(page_image(html).as_deref(), Some("https://i.example.com/a.jpg?x=1&y=2"));
        let tw = r#"<meta name='twitter:image' content='/img/b.png'>"#;
        assert_eq!(page_image(tw).as_deref(), Some("/img/b.png"));
        assert_eq!(page_image("<p>og:image is cool</p>"), None);
    }

    #[test]
    fn names_are_clean_and_unique() {
        assert_eq!(clean_stem("C:\\Downloads\\this is fine.PNG", "x"), "this is fine");
        assert_eq!(clean_stem("what?.jpg", "x"), "what_");
        assert_eq!(clean_stem("", "pasted"), "pasted");
        let root = std::env::temp_dir().join(format!("memeity-import-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("cat.png"), b"x").unwrap();
        assert_eq!(free_path(&root, "cat", "png"), root.join("cat (2).png"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn detects_formats_from_bytes() {
        let mut png = Vec::new();
        image::RgbaImage::new(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(extension_for(&png), Some("png"));
        assert_eq!(extension_for(b"GIF89a...."), Some("gif"));
        assert_eq!(extension_for(b"<html>"), None);
    }
}
