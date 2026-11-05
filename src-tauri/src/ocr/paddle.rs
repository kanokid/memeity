//! The PaddleOCR engine (PP-OCRv6 "small") on ONNX Runtime, via `oar-ocr`.
//! Only compiled with the `ocr` feature; see `ocr.rs`.
//!
//! The ~30 MB of model files aren't bundled: they're downloaded once, on first use, into
//! the app-data folder and verified against pinned SHA-256 checksums.
//!
//! Models: PaddlePaddle's PaddleOCR project (Apache-2.0), ONNX exports published by
//! oar-ocr (https://github.com/GreatV/oar-ocr).

use oar_ocr::core::config::OrtSessionConfig;
use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::oarocr::{OAROCRBuilder, OAROCR};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Recognized lines below this confidence are dropped as likely noise.
const MIN_CONFIDENCE: f32 = 0.5;
/// Keep OCR from hogging the CPU: it runs in the background while you work.
const OCR_THREADS: usize = 2;

const RELEASE: &str = "https://github.com/GreatV/oar-ocr/releases/download/v0.7.0";

struct ModelFile {
    name: &'static str,
    sha256: &'static str,
}

const DET: ModelFile = ModelFile {
    name: "pp-ocrv6_small_det.onnx",
    sha256: "d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e",
};
const REC: ModelFile = ModelFile {
    name: "pp-ocrv6_small_rec.onnx",
    sha256: "5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634",
};
const DICT: ModelFile = ModelFile {
    name: "ppocrv6_dict.txt",
    sha256: "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d",
};
const FILES: [&ModelFile; 3] = [&DET, &REC, &DICT];

pub struct Engine {
    ocr: OAROCR,
}

impl Engine {
    /// True if every model file is already downloaded (so `new` won't hit the network).
    pub fn is_downloaded(dir: &Path) -> bool {
        FILES.iter().all(|f| dir.join(f.name).is_file())
    }

    /// Loads the models from `dir`, downloading any that are missing first.
    pub fn new(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
        for file in FILES {
            ensure_file(dir, file)?;
        }

        // PaddleOCR's official PP-OCRv6 detection settings (as in oar-ocr's preset).
        let detection = TextDetectionConfig {
            score_threshold: 0.2,
            box_threshold: 0.45,
            unclip_ratio: 1.4,
            max_candidates: 3000,
            ..TextDetectionConfig::default()
        };
        let ocr = OAROCRBuilder::new(dir.join(DET.name), dir.join(REC.name), dir.join(DICT.name))
            .text_detection_config(detection)
            .ort_session(OrtSessionConfig::new().with_intra_threads(OCR_THREADS))
            .build()
            .map_err(|e| format!("Couldn't load the text recognition model: {e}"))?;
        Ok(Engine { ocr })
    }

    pub fn recognize(&self, path: &Path) -> Result<String, String> {
        let image = flatten(crate::clipboard::decode(path)?);
        let results = self.ocr.predict(vec![image]).map_err(|e| format!("Text recognition failed: {e}"))?;
        let lines: Vec<&str> = results
            .first()
            .map(|r| {
                r.text_regions
                    .iter()
                    .filter_map(|region| region.text_with_confidence())
                    .filter(|(text, confidence)| *confidence >= MIN_CONFIDENCE && !text.trim().is_empty())
                    .map(|(text, _)| text.trim())
                    .collect()
            })
            .unwrap_or_default();
        Ok(lines.join("\n"))
    }
}

/// Composites transparency onto white: dark-on-light reads best, and the model wants RGB.
fn flatten(img: image::RgbaImage) -> image::RgbImage {
    let (w, h) = img.dimensions();
    let mut out = image::RgbImage::new(w, h);
    for (src, dst) in img.pixels().zip(out.pixels_mut()) {
        let a = src.0[3] as u16;
        for c in 0..3 {
            dst.0[c] = ((src.0[c] as u16 * a + 255 * (255 - a)) / 255) as u8;
        }
    }
    out
}

// ---------- model download ----------

fn sha256_of(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Makes sure `file` exists in `dir` with the expected checksum, downloading it if not.
fn ensure_file(dir: &Path, file: &ModelFile) -> Result<(), String> {
    let dest = dir.join(file.name);
    if dest.is_file() {
        // Trust files we verified when downloading; re-hashing 30 MB every launch is wasteful.
        return Ok(());
    }
    let part: PathBuf = dir.join(format!("{}.part", file.name));
    let url = format!("{RELEASE}/{}", file.name);

    let mut last_err = String::new();
    for attempt in 1..=3 {
        match download(&url, &part) {
            Ok(()) => match sha256_of(&part) {
                Ok(hash) if hash == file.sha256 => {
                    return std::fs::rename(&part, &dest).map_err(|e| e.to_string());
                }
                Ok(hash) => last_err = format!("{} failed its integrity check (got {hash})", file.name),
                Err(e) => last_err = e.to_string(),
            },
            Err(e) => last_err = e,
        }
        let _ = std::fs::remove_file(&part);
        log::warn!("model download attempt {attempt} for {} failed: {last_err}", file.name);
        std::thread::sleep(Duration::from_secs(2 * attempt));
    }
    Err(format!("Couldn't download the text recognition model: {last_err}"))
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    tauri::async_runtime::block_on(async {
        let resp = reqwest::Client::new()
            .get(url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        std::fs::write(to, &bytes).map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_composites_onto_white() {
        let img = image::RgbaImage::from_raw(2, 1, vec![0, 0, 0, 0, 10, 20, 30, 255]).unwrap();
        let out = flatten(img);
        assert_eq!(out.get_pixel(0, 0).0, [255, 255, 255], "transparent becomes white");
        assert_eq!(out.get_pixel(1, 0).0, [10, 20, 30], "opaque unchanged");
    }

    /// Runs the real model (downloads it on first run).
    /// `cargo test real_ocr -- --ignored --nocapture`, with an image at the printed path.
    #[test]
    #[ignore]
    fn real_ocr() {
        let dir = std::env::temp_dir().join("memeity-paddle-models");
        let started = std::time::Instant::now();
        let engine = Engine::new(&dir).expect("engine");
        println!("loaded in {:?}", started.elapsed());
        let path = std::env::temp_dir().join("memeity-ocr-probe.png");
        assert!(path.exists(), "put an image with text at {}", path.display());
        let started = std::time::Instant::now();
        println!("text: {:?} ({:?})", engine.recognize(&path), started.elapsed());
    }
}
