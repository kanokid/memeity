//! "Copy to Clipboard" output.
//!
//! - Animated memes (GIFs, animated WebPs) are copied as the *file itself*, like Ctrl+C
//!   in Explorer. Pasting into Discord, Slack, browsers etc. uploads the real file, so
//!   the animation survives. (Raw pixels on the clipboard can only hold one frame.)
//! - Still images are copied as pixels, which pastes everywhere, including apps that
//!   don't accept files (Paint, Docs, image editors).
//!
//! Wherever a *file* is handed to another app (copied as a file, pasted, dragged out),
//! formats many apps reject get a converted copy instead (see `shareable`): animated
//! WebP becomes a GIF, still WebP and BMP become PNG. The meme itself never changes.

use std::borrow::Cow;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const COPY_ATTEMPTS: u32 = 3;

/// Copies a meme in whichever form keeps it intact.
pub fn copy(path: &Path) -> Result<(), String> {
    if is_animated(path) {
        let file = shareable(path)?;
        with_retries(path, |cb| cb.set().file_list(std::slice::from_ref(&file)))
    } else {
        copy_image(path)
    }
}

/// Decodes an image file into RGBA pixels.
pub fn decode(path: &Path) -> Result<image::RgbaImage, String> {
    Ok(image::ImageReader::open(path)
        .map_err(|e| format!("Couldn't open {}: {e}", path.display()))?
        // Trust the bytes, not the extension (memes are often misnamed).
        .with_guessed_format()
        .map_err(|e| format!("Couldn't read {}: {e}", path.display()))?
        .decode()
        .map_err(|e| format!("Couldn't decode {}: {e}", path.display()))?
        .into_rgba8())
}

/// Puts the meme's pixels on the clipboard.
fn copy_image(path: &Path) -> Result<(), String> {
    let img = decode(path)?;
    let (width, height) = img.dimensions();
    let image = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: Cow::Owned(img.into_raw()),
    };
    with_retries(path, |cb| cb.set_image(image.clone()))
}

/// Other apps (Win+V history, clipboard managers) briefly lock the clipboard while
/// they read it, so a write can fail transiently. Retry a few times before giving up.
fn with_retries(
    path: &Path,
    write: impl Fn(&mut arboard::Clipboard) -> Result<(), arboard::Error>,
) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 1..=COPY_ATTEMPTS {
        match arboard::Clipboard::new().and_then(|mut cb| write(&mut cb)) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!("copy attempt {attempt}/{COPY_ATTEMPTS} for {} failed: {e}", path.display());
                last_err = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
    }
    Err(format!("Couldn't copy {}: {last_err}", path.file_name().unwrap_or_default().to_string_lossy()))
}

// ---------- shareable copies ----------

/// Converted copies are kept here (in the app cache dir). Set once at startup.
static SHARE_DIR: OnceLock<PathBuf> = OnceLock::new();
/// Converted copies older than this are cleared out.
const SHARE_KEEP: Duration = Duration::from_secs(24 * 3600);

pub fn set_share_dir(dir: PathBuf) {
    let _ = SHARE_DIR.set(dir);
}

/// What a file's bytes say it is (not its name).
#[derive(Debug, PartialEq)]
enum Kind {
    /// GIF, PNG, JPEG: accepted everywhere, shared as-is.
    Fine,
    AnimatedWebp,
    StillWebp,
    Bmp,
}

fn kind_of(head: &[u8]) -> Kind {
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        if is_animated_header(head) { Kind::AnimatedWebp } else { Kind::StillWebp }
    } else if head.starts_with(b"BM") {
        Kind::Bmp
    } else {
        Kind::Fine
    }
}

/// The file to hand another app for `path`: the meme itself if its format is widely
/// accepted, otherwise a converted copy (GIF for animated WebP, PNG for still WebP/BMP)
/// with the same name. Copies are cached until the meme changes.
pub fn shareable(path: &Path) -> Result<PathBuf, String> {
    let mut head = [0u8; 32];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).map_err(|e| e.to_string())?;
    let kind = kind_of(&head[..n]);
    let ext = match kind {
        Kind::Fine => return Ok(path.to_path_buf()),
        Kind::AnimatedWebp => "gif",
        Kind::StillWebp | Kind::Bmp => "png",
    };
    // No cache dir (shouldn't happen): share the original rather than fail.
    let Some(root) = SHARE_DIR.get() else { return Ok(path.to_path_buf()) };

    let modified = std::fs::metadata(path).and_then(|m| m.modified()).map_err(|e| e.to_string())?;
    let dir = root.join(crate::thumbs::key(path, modified));
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "meme".into());
    let out = dir.join(format!("{stem}.{ext}"));
    if out.is_file() {
        return Ok(out);
    }
    prune_shared(root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let part = dir.join(format!(".{stem}.{ext}.{}.part", std::process::id()));
    match kind {
        Kind::AnimatedWebp => webp_to_gif(path, &part)?,
        _ => decode(path)?.save_with_format(&part, image::ImageFormat::Png).map_err(|e| e.to_string())?,
    }
    std::fs::rename(&part, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn webp_to_gif(src: &Path, dest: &Path) -> Result<(), String> {
    use image::AnimationDecoder;
    let file = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let decoder = image::codecs::webp::WebPDecoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("Couldn't read {}: {e}", src.display()))?;
    let frames = decoder.into_frames().collect_frames().map_err(|e| e.to_string())?;
    let out = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(std::io::BufWriter::new(out), 10);
    encoder.set_repeat(image::codecs::gif::Repeat::Infinite).map_err(|e| e.to_string())?;
    encoder.encode_frames(frames).map_err(|e| e.to_string())
}

/// Removes converted copies nobody has made in a day.
/// Saves an image that isn't in the library (the editor's result, copied or pasted
/// without saving) as a file other apps can be handed. Cleared out after a day.
pub fn temp_file(name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let root = SHARE_DIR.get().ok_or("No cache folder.")?;
    prune_shared(root);
    let nanos = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = root.join(format!("edited-{nanos}"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = crate::sync::sanitize(name);
    let path = dir.join(if name.trim().is_empty() { "meme.png".to_string() } else { name });
    std::fs::write(&path, bytes).map_err(|e| format!("Couldn't prepare the image: {e}"))?;
    Ok(path)
}

fn prune_shared(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > SHARE_KEEP);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Sniffs the file header (not the extension, since memes are often misnamed).
pub(crate) fn is_animated(path: &Path) -> bool {
    let mut head = [0u8; 32];
    let Ok(n) = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)) else {
        return false;
    };
    is_animated_header(&head[..n])
}

fn is_animated_header(head: &[u8]) -> bool {
    // Any GIF: even single-frame ones are better pasted as the original file.
    if head.starts_with(b"GIF8") {
        return true;
    }
    // Animated WebP: RIFF....WEBP, extended "VP8X" chunk with the animation flag (0x02).
    head.len() > 20 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" && &head[12..16] == b"VP8X" && head[20] & 0x02 != 0
}

#[cfg(test)]
mod tests {
    use super::{is_animated_header, kind_of, Kind};

    #[test]
    fn picks_formats_to_convert() {
        assert_eq!(kind_of(b"GIF89a\x01\x00"), Kind::Fine);
        assert_eq!(kind_of(b"\x89PNG\r\n\x1a\n"), Kind::Fine);
        assert_eq!(kind_of(b"\xff\xd8\xff\xe0"), Kind::Fine);
        assert_eq!(kind_of(b"BM\0\0"), Kind::Bmp);
        let mut webp = *b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0\0\0\0\0\0\0\0\0";
        assert_eq!(kind_of(&webp), Kind::AnimatedWebp);
        webp[20] = 0;
        assert_eq!(kind_of(&webp), Kind::StillWebp);
    }

    #[test]
    fn converts_still_webp_and_bmp_to_png_and_keeps_others() {
        let dir = std::env::temp_dir().join(format!("memeity-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        super::set_share_dir(dir.join("shared"));
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 128, 255, 255]));

        let bmp = dir.join("cat.bmp");
        image::DynamicImage::ImageRgba8(img.clone()).to_rgb8().save(&bmp).unwrap();
        let out = super::shareable(&bmp).unwrap();
        assert_eq!(out.file_name().unwrap(), "cat.png");
        assert_eq!(image::open(&out).unwrap().width(), 8);
        assert_eq!(super::shareable(&bmp).unwrap(), out, "cached");

        let png = dir.join("dog.png");
        img.save(&png).unwrap();
        assert_eq!(super::shareable(&png).unwrap(), png, "PNG is shared as-is");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detects_animated_formats_by_header() {
        assert!(is_animated_header(b"GIF89a\x01\x00\x01\x00"));
        let mut webp = *b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0\0\0\0\0\0\0\0\0";
        assert!(is_animated_header(&webp), "animated WebP");
        webp[20] = 0x10; // alpha flag only
        assert!(!is_animated_header(&webp), "still WebP");
        assert!(!is_animated_header(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_animated_header(b""));
    }

    /// Writes to the real system clipboard, so it only runs on request:
    /// `cargo test copy_to_real_clipboard -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn copy_to_real_clipboard() {
        let path = std::env::temp_dir().join(format!("memeity-copy-{}.png", std::process::id()));
        image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 0, 0, 255])).save(&path).unwrap();
        let result = super::copy(&path);
        std::fs::remove_file(&path).unwrap();
        println!("copy result: {result:?}");
        result.unwrap();
    }
}
