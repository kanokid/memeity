//! Text recognition: reading the words in memes so search can find them.
//!
//! The engine is PaddleOCR (PP-OCRv6 "small"), running on-device through ONNX Runtime
//! (see `ocr/paddle.rs`). PaddleOCR is built for text in pictures (scene text), so it
//! copes far better than the OS engines with what memes are made of: Impact captions
//! over photos, stylized fonts, small or noisy text.
//!
//! It's behind the `ocr` Cargo feature (on by default) because ONNX Runtime has no
//! prebuilt binaries for some targets, notably Intel Macs. Builds without it still
//! search filenames, and still use text that other devices read (it arrives in synced
//! sidecars); they just can't read new memes themselves.

/// Recorded in each sidecar. Text from any other engine (e.g. the Windows OCR earlier
/// versions used) gets re-read with this one.
pub const ENGINE_NAME: &str = "paddleocr.pp-ocrv6-small";
/// Folder (inside the app-data dir) holding this model version.
pub const MODEL_DIR: &str = "models/pp-ocrv6-small";
/// Shown while downloading.
pub const DOWNLOAD_SIZE: &str = "30 MB";

#[cfg(feature = "ocr")]
mod paddle;
#[cfg(feature = "ocr")]
pub use paddle::Engine;

/// Builds without the `ocr` feature: no engine, explained in the UI.
#[cfg(not(feature = "ocr"))]
pub struct Engine;

#[cfg(not(feature = "ocr"))]
impl Engine {
    /// Nothing to download (so the UI doesn't flash "Downloading…").
    pub fn is_downloaded(_dir: &std::path::Path) -> bool {
        true
    }

    pub fn new(_dir: &std::path::Path) -> Result<Self, String> {
        Err("This build of memeity can't read text in images on this computer. Search by name still works, \
             and text read on your other devices still syncs here."
            .into())
    }

    pub fn recognize(&self, _path: &std::path::Path) -> Result<String, String> {
        unreachable!("Engine::new always fails without the `ocr` feature")
    }
}
