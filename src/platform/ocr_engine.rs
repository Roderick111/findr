//! Cross-platform OCR via the `ocrs` crate (pure Rust, ONNX models).
//! Used on Linux and Windows. macOS uses Apple Vision via findr-ocr binary instead.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

static OCR_ENGINE: OnceLock<Option<OcrState>> = OnceLock::new();

const DETECTION_MODEL_URL: &str =
    "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten";
const RECOGNITION_MODEL_URL: &str =
    "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten";

/// Expected SHA-256 hashes for model files. Empty string = skip verification.
const DETECTION_MODEL_SHA256: &str = "";
const RECOGNITION_MODEL_SHA256: &str = "";

struct OcrState {
    engine: ocrs::OcrEngine,
}

fn models_dir() -> PathBuf {
    super::data_dir().join("models")
}

/// Compute SHA-256 hex digest of a byte slice.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Download a model file if not cached. Returns local path.
/// `expected_sha256` is checked after download; empty string skips verification.
fn ensure_model(url: &str, expected_sha256: &str) -> Option<PathBuf> {
    let dir = models_dir();
    let _ = std::fs::create_dir_all(&dir);

    let filename = url.rsplit('/').next()?;
    let local_path = dir.join(filename);

    if local_path.exists() {
        return Some(local_path);
    }

    eprintln!("Downloading OCR model: {}...", filename);
    let response = ureq::get(url).call().ok()?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes).ok()?;

    // Verify checksum when an expected hash is configured
    if !expected_sha256.is_empty() {
        let actual = sha256_hex(&bytes);
        if actual != expected_sha256 {
            eprintln!(
                "  Checksum mismatch for {}: expected {}, got {}",
                filename, expected_sha256, actual
            );
            return None;
        }
        eprintln!("  Checksum verified for {}", filename);
    }

    std::fs::write(&local_path, &bytes).ok()?;
    eprintln!("  Saved to {}", local_path.display());

    Some(local_path)
}

/// Initialize the OCR engine (lazy, once). Returns None if models can't be loaded.
fn get_engine() -> Option<&'static OcrState> {
    OCR_ENGINE
        .get_or_init(|| {
            let det_path = ensure_model(DETECTION_MODEL_URL, DETECTION_MODEL_SHA256)?;
            let rec_path = ensure_model(RECOGNITION_MODEL_URL, RECOGNITION_MODEL_SHA256)?;

            let det_model = rten::Model::load_file(det_path).ok()?;
            let rec_model = rten::Model::load_file(rec_path).ok()?;

            let engine = ocrs::OcrEngine::new(ocrs::OcrEngineParams {
                detection_model: Some(det_model),
                recognition_model: Some(rec_model),
                ..Default::default()
            })
            .ok()?;

            Some(OcrState { engine })
        })
        .as_ref()
}

/// Extract text from a single image using ocrs. Returns (text, confidence).
pub fn extract_ocr_text(path: &Path) -> Option<(String, f64)> {
    let state = get_engine()?;

    let img = image::open(path).ok()?.into_rgb8();
    let (w, h) = img.dimensions();
    let source = ocrs::ImageSource::from_bytes(img.as_raw(), (w, h)).ok()?;
    let input = state.engine.prepare_input(source).ok()?;

    let text = state.engine.get_text(&input).ok()?;
    let text = text.trim().to_string();

    if text.is_empty() {
        return Some((String::new(), 0.0));
    }

    // ocrs doesn't provide a confidence score directly.
    // Estimate based on text length / density as a rough proxy.
    let confidence = if text.len() > 10 { 0.7 } else { 0.4 };

    Some((text, confidence))
}
