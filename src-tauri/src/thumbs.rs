//! Grid thumbnails, served to the webview through a `thumb://` URI scheme.
//!
//! The grid used to load every full-size original, which gets slow (and memory-hungry)
//! with big or high-resolution libraries. Now each tile asks for `thumb://<path>`; the
//! first request decodes the image, shrinks it to `MAX_SIDE`, and saves it in the app
//! cache dir; later requests just read that file. Thumbnails are made lazily, as tiles
//! scroll into view, never in bulk.
//!
//! Cache files are keyed by the source's path and modified time, so an edited or
//! replaced meme gets a fresh thumbnail automatically, and `prune` can tell which cached
//! files still belong to a meme straight from the in-memory library (no disk reads).
//! Orphans (deleted or changed memes, a previous library folder) are removed after each
//! indexing pass.

use md5::{Digest, Md5};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::http::{header, Response, StatusCode};
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;

/// Longest side, in pixels: ~2x the grid's tile width, so tiles stay sharp on HiDPI.
const MAX_SIDE: u32 = 480;
/// At most this many thumbnails are generated at once (scrolling fast shouldn't
/// decode 40 photos in parallel).
const CONCURRENCY: usize = 4;
const JPEG_QUALITY: u8 = 82;

fn permits() -> &'static Semaphore {
    static PERMITS: OnceLock<Semaphore> = OnceLock::new();
    PERMITS.get_or_init(|| Semaphore::new(CONCURRENCY))
}

pub fn cache_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_cache_dir().ok().map(|d| d.join("thumbs"))
}

/// Handles one `thumb://` request (called on a worker thread).
pub async fn respond(app: AppHandle, uri_path: String) -> Response<Vec<u8>> {
    let _permit = permits().acquire().await;
    let result = tauri::async_runtime::spawn_blocking(move || serve(&app, &uri_path))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    match result {
        Ok((bytes, mime)) => Response::builder()
            .header(header::CONTENT_TYPE, mime)
            // The URL carries the file's modified time (?v=...), so it's safe to cache.
            .header(header::CACHE_CONTROL, "max-age=31536000, immutable")
            .body(bytes)
            .expect("valid response"),
        Err(message) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(message.into_bytes())
            .expect("valid response"),
    }
}

fn serve(app: &AppHandle, uri_path: &str) -> Result<(Vec<u8>, &'static str), String> {
    let path = PathBuf::from(percent_decode_str(uri_path.trim_start_matches('/')));
    // Only ever read files that are in the meme library.
    let known = app.state::<crate::AppState>().library.read().unwrap().contains(&path);
    if !known {
        return Err("not in the library".into());
    }
    let dir = cache_dir(app).ok_or("no cache dir")?;
    thumbnail(&dir, &path)
}

/// Returns the cached thumbnail for `path`, making it first if needed.
pub fn thumbnail(dir: &Path, path: &Path) -> Result<(Vec<u8>, &'static str), String> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).map_err(|e| e.to_string())?;
    let key = key(path, modified);

    for (ext, mime) in [("jpg", "image/jpeg"), ("png", "image/png")] {
        if let Ok(bytes) = std::fs::read(dir.join(format!("{key}.{ext}"))) {
            return Ok((bytes, mime));
        }
    }

    let (bytes, ext, mime) = make(path)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // Write-then-rename so a half-written file is never served.
    let part = dir.join(format!("{key}.{ext}.part"));
    if std::fs::write(&part, &bytes).is_ok() {
        let _ = std::fs::rename(&part, dir.join(format!("{key}.{ext}")));
    }
    Ok((bytes, mime))
}

/// Cache file name (without extension) for a meme at `path` last modified at `modified`.
pub fn key(path: &Path, modified: SystemTime) -> String {
    let nanos = modified.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{:x}", Md5::digest(format!("{}|{nanos}", path.display())))
}

/// Leftover partial writes older than this are cleared too.
const STALE_PART: Duration = Duration::from_secs(600);

/// Deletes cached thumbnails whose key isn't in `keep` (thumbnails of memes that were
/// deleted or changed, or of another library). Returns how many files were removed.
pub fn prune(dir: &Path, keep: &HashSet<String>) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let key = name.split('.').next().unwrap_or_default();
        let stale_part = name.ends_with(".part")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_PART);
        let orphan = !name.ends_with(".part") && !keep.contains(key);
        if (orphan || stale_part) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Decodes and shrinks an image: JPEG if it's opaque, PNG if it has transparency.
fn make(path: &Path) -> Result<(Vec<u8>, &'static str, &'static str), String> {
    let img = crate::clipboard::decode(path)?;
    let (w, h) = img.dimensions();
    let scale = (MAX_SIDE as f64 / w.max(h).max(1) as f64).min(1.0);
    let img = if scale < 1.0 {
        let nw = ((w as f64 * scale).round() as u32).max(1);
        let nh = ((h as f64 * scale).round() as u32).max(1);
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img
    };

    let mut out = Vec::new();
    let transparent = img.pixels().any(|p| p.0[3] < 255);
    if transparent {
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok((out, "png", "image/png"))
    } else {
        let rgb = image::DynamicImage::ImageRgba8(img).into_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .encode_image(&rgb)
            .map_err(|e| e.to_string())?;
        Ok((out, "jpg", "image/jpeg"))
    }
}

/// Decodes %XX escapes (the webview percent-encodes the whole file path).
pub fn percent_decode_str(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode_str("C%3A%5CUsers%5Cme%5Ccat%20meme.png"), r"C:\Users\me\cat meme.png");
        assert_eq!(percent_decode_str("plain"), "plain");
        assert_eq!(percent_decode_str("bad%zz"), "bad%zz");
    }

    #[test]
    fn makes_caches_and_reuses_thumbnails() {
        let root = std::env::temp_dir().join(format!("memeity-thumbs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let big = root.join("big.png");
        image::RgbaImage::from_pixel(1600, 800, image::Rgba([200, 10, 10, 255])).save(&big).unwrap();
        let clear = root.join("clear.png");
        image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 0])).save(&clear).unwrap();
        let cache = root.join("cache");

        let (bytes, mime) = thumbnail(&cache, &big).unwrap();
        assert_eq!(mime, "image/jpeg", "opaque -> JPEG");
        let thumb = image::load_from_memory(&bytes).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (480, 240), "longest side capped");
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1, "cached on disk");
        let (again, _) = thumbnail(&cache, &big).unwrap();
        assert_eq!(again, bytes, "second call served from cache");

        let (_, mime) = thumbnail(&cache, &clear).unwrap();
        assert_eq!(mime, "image/png", "transparency -> PNG");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prune_removes_only_orphans() {
        let root = std::env::temp_dir().join(format!("memeity-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cache = root.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let keep_me = root.join("keep.png");
        image::RgbaImage::from_pixel(50, 50, image::Rgba([1, 2, 3, 255])).save(&keep_me).unwrap();
        thumbnail(&cache, &keep_me).unwrap();
        std::fs::write(cache.join("deadbeef.jpg"), b"old").unwrap(); // a deleted meme's thumbnail
        std::fs::write(cache.join("fresh.jpg.part"), b"writing").unwrap(); // in progress: keep

        let modified = std::fs::metadata(&keep_me).unwrap().modified().unwrap();
        let keep = HashSet::from([key(&keep_me, modified)]);
        assert_eq!(prune(&cache, &keep), 1);
        let mut left: Vec<String> =
            std::fs::read_dir(&cache).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left.len(), 2, "current thumbnail and the fresh partial stay: {left:?}");
        assert!(!left.contains(&"deadbeef.jpg".to_string()));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
