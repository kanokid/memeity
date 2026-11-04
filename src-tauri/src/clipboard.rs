//! "Copy to clipboard" output: decode the image and put pixels on the clipboard.

use std::borrow::Cow;
use std::path::Path;

pub fn copy_image(path: &Path) -> Result<(), String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("Couldn't open {}: {e}", path.display()))?
        // Trust the bytes, not the extension (memes are often misnamed).
        .with_guessed_format()
        .map_err(|e| format!("Couldn't read {}: {e}", path.display()))?
        .decode()
        .map_err(|e| format!("Couldn't decode {}: {e}", path.display()))?
        .into_rgba8();

    let (width, height) = img.dimensions();
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("Clipboard unavailable: {e}"))?;
    clipboard
        .set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: Cow::Owned(img.into_raw()),
        })
        .map_err(|e| format!("Couldn't copy image: {e}"))
}
