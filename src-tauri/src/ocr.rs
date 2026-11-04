//! Text recognition with the OS's built-in engine.
//! Windows: `Windows.Media.Ocr`. macOS (Apple Vision) and Linux (Tesseract) come later.

#[cfg(not(windows))]
use std::path::Path;

pub const ENGINE_NAME: &str = "windows.media.ocr";

/// Images whose longest side is below this get upscaled 2x first. Meme captions in
/// small images are often too few pixels tall for the OCR engine to read reliably.
#[cfg_attr(not(windows), allow(dead_code))]
const UPSCALE_BELOW: u32 = 800;

#[cfg(windows)]
pub use win::Engine;

#[cfg(not(windows))]
pub struct Engine;

#[cfg(not(windows))]
impl Engine {
    pub fn new() -> Result<Self, String> {
        Err("Reading text in images isn't supported on this platform yet.".into())
    }
    pub fn language(&self) -> Option<String> {
        None
    }
    pub fn recognize(&self, _path: &Path) -> Result<String, String> {
        unreachable!("Engine::new always fails on this platform")
    }
}

/// Flattens transparency onto white (OCR reads dark-on-light best) and scales into
/// the engine's comfortable range.
#[cfg_attr(not(windows), allow(dead_code))]
fn prepare(img: image::RgbaImage, max_dim: u32) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    let longest = w.max(h).max(1);
    let target = if longest < UPSCALE_BELOW { (longest * 2).min(max_dim) } else { longest.min(max_dim) };
    let mut img = if target == longest {
        img
    } else {
        let scale = target as f64 / longest as f64;
        let nw = ((w as f64 * scale).round() as u32).max(1);
        let nh = ((h as f64 * scale).round() as u32).max(1);
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
    };
    for px in img.pixels_mut() {
        let a = px.0[3] as u16;
        for c in 0..3 {
            px.0[c] = ((px.0[c] as u16 * a + 255 * (255 - a)) / 255) as u8;
        }
        px.0[3] = 255;
    }
    img
}

#[cfg(windows)]
mod win {
    use super::prepare;
    use std::path::Path;
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

    pub struct Engine {
        engine: OcrEngine,
        max_dim: u32,
        language: Option<String>,
    }

    impl Engine {
        /// Uses the first of the user's Windows display languages that has OCR support.
        pub fn new() -> Result<Self, String> {
            // WinRT needs the calling thread initialized; "already initialized" is fine.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
                "Windows has no text-recognition language installed. Add one in Settings → \
                 Time & language → Language & region (e.g. English with “Optical character recognition”)."
                    .to_string()
            })?;
            let language = engine.RecognizerLanguage().and_then(|l| l.LanguageTag()).ok().map(|t| t.to_string());
            let max_dim = OcrEngine::MaxImageDimension().unwrap_or(4096);
            Ok(Engine { engine, max_dim, language })
        }

        pub fn language(&self) -> Option<String> {
            self.language.clone()
        }

        pub fn recognize(&self, path: &Path) -> Result<String, String> {
            // Decode with the `image` crate: handles WebP and misnamed files uniformly.
            let img = prepare(crate::clipboard::decode(path)?, self.max_dim);
            let (w, h) = img.dimensions();
            let mut bgra = img.into_raw();
            for px in bgra.chunks_exact_mut(4) {
                px.swap(0, 2); // RGBA -> BGRA
            }
            let err = |e: windows::core::Error| format!("Text recognition failed: {}", e.message());
            let writer = DataWriter::new().map_err(err)?;
            writer.WriteBytes(&bgra).map_err(err)?;
            let buffer = writer.DetachBuffer().map_err(err)?;
            let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w as i32, h as i32)
                .map_err(err)?;
            let result = self.engine.RecognizeAsync(&bitmap).map_err(err)?.join().map_err(err)?;
            Ok(result.Text().map_err(err)?.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_scales_and_flattens() {
        let small = image::RgbaImage::from_pixel(100, 50, image::Rgba([0, 0, 0, 0]));
        let out = prepare(small, 4096);
        assert_eq!(out.dimensions(), (200, 100), "small images are upscaled 2x");
        assert_eq!(out.get_pixel(0, 0).0, [255, 255, 255, 255], "transparent becomes white");

        let huge = image::RgbaImage::from_pixel(8000, 2000, image::Rgba([10, 20, 30, 255]));
        let out = prepare(huge, 4000);
        assert_eq!(out.dimensions(), (4000, 1000), "oversized images fit the engine limit");
        assert_eq!(out.get_pixel(0, 0).0, [10, 20, 30, 255], "opaque pixels unchanged");
    }

    /// Runs the real Windows OCR engine on a generated image with text.
    /// `cargo test real_ocr -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_ocr() {
        let engine = Engine::new().expect("OCR engine");
        println!("language: {:?}", engine.language());
        let path = std::env::temp_dir().join("memeity-ocr-probe.png");
        assert!(path.exists(), "put an image with text at {}", path.display());
        println!("text: {:?}", engine.recognize(&path));
    }
}
