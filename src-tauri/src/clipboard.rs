//! "Copy to Clipboard" output.
//!
//! - Animated memes (GIFs, animated WebPs) are copied as the *file itself*, like Ctrl+C
//!   in Explorer. Pasting into Discord, Slack, browsers etc. uploads the real file, so
//!   the animation survives. (Raw pixels on the clipboard can only hold one frame.)
//! - Still images are copied as pixels, which pastes everywhere, including apps that
//!   don't accept files (Paint, Docs, image editors).

use std::borrow::Cow;
use std::io::Read;
use std::path::Path;

const COPY_ATTEMPTS: u32 = 3;

/// Copies a meme in whichever form keeps it intact.
pub fn copy(path: &Path) -> Result<(), String> {
    if is_animated(path) {
        // Copying a file needs its full path, which `path` already is.
        with_retries(path, |cb| cb.set().file_list(&[path]))
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
                eprintln!("[memeity] copy attempt {attempt}/{COPY_ATTEMPTS} for {} failed: {e}", path.display());
                last_err = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
    }
    Err(format!("Couldn't copy {}: {last_err}", path.file_name().unwrap_or_default().to_string_lossy()))
}

/// Sniffs the file header (not the extension, since memes are often misnamed).
fn is_animated(path: &Path) -> bool {
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
    use super::is_animated_header;

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
