//! Semantic embedding tier for Findr.
//!
//! Optional layer: only active when user provides an OpenRouter API key.
//! Uses pplx-embed-v1-0.6b (512d) to embed file content and match by meaning.

use anyhow::{anyhow, Result};
use std::sync::OnceLock;

pub const EMBED_MODEL: &str = "perplexity/pplx-embed-v1-0.6b";
pub const EMBED_DIMS: usize = 512;
pub const VECTOR_BYTES: usize = EMBED_DIMS * 4;
pub const COSINE_THRESHOLD: f32 = 0.15;
pub const API_BATCH_SIZE: usize = 20;
const API_URL: &str = "https://openrouter.ai/api/v1/embeddings";
const API_TIMEOUT_MS: u64 = 10_000;

// [Tier 2 fix #8] Removed doc/ppt/pptx — no extractor exists for these formats.
pub const EMBEDDABLE_EXTENSIONS: &[&str] = &[
    "md", "txt",
    "pdf", "docx", "xlsx",
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "rb", "java", "c", "cpp", "h", "swift",
    "csv",
    "html", "htm",
];

// ─── API Key ───

static API_KEY: OnceLock<Option<String>> = OnceLock::new();

pub fn get_api_key() -> Option<String> {
    API_KEY.get_or_init(|| {
        // 1. Environment variable
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
        // 2. Config file
        let home = match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => h,
            _ => return None, // No HOME = no key file
        };
        let key_path = std::path::PathBuf::from(&home).join(".findr").join("openrouter_key");

        // [Tier 1 fix #5] Check file permissions — warn if too open
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&key_path) {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    eprintln!("Warning: ~/.findr/openrouter_key has insecure permissions ({:o}). Run: chmod 600 ~/.findr/openrouter_key", mode);
                }
            }
        }

        if let Ok(key) = std::fs::read_to_string(key_path) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
        None
    }).clone()
}

// ─── Embed Text Builder ───

/// Build the text to embed for a given file. Returns None if file type should be skipped.
pub fn build_embed_text(filename: &str, content: &str, ext: &str) -> Option<String> {
    match ext {
        "md" | "txt" => {
            let title = first_meaningful_line(content);
            let body = strip_md(content);
            let body_trunc = safe_truncate(&body, 800);
            Some(format!("File: {}\n{}\n\n{}", filename, title, body_trunc))
        }
        "pdf" | "docx" | "xlsx" => {
            let trunc = safe_truncate(content, 800);
            Some(format!("File: {}\n\n{}", filename, trunc))
        }
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "rb"
        | "java" | "c" | "cpp" | "h" | "swift" => {
            let trunc = safe_truncate(content, 200);
            Some(format!("File: {}\n\n{}", filename, trunc))
        }
        "csv" => {
            let trunc = safe_truncate(content, 400);
            Some(format!("File: {}\n\n{}", filename, trunc))
        }
        "html" | "htm" => {
            let stripped = strip_html_tags(content);
            let trunc = safe_truncate(&stripped, 800);
            Some(format!("File: {}\n\n{}", filename, trunc))
        }
        _ => None,
    }
}

/// First non-empty line >3 chars, with leading # stripped.
fn first_meaningful_line(text: &str) -> &str {
    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches('#').trim();
        if trimmed.len() > 3 {
            return trimmed;
        }
    }
    ""
}

/// Truncate at char boundary. Note: limit is in bytes, not characters.
/// For ASCII-dominated content (filenames, markdown, code) this is equivalent.
fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    crate::content::truncate_str(s, max_bytes)
}

/// Strip markdown formatting: headers, bold, italic, code, links, lists, tables.
fn strip_md(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        if trimmed.starts_with("|--") || trimmed.starts_with("| --") || trimmed.starts_with("---") || trimmed.starts_with("===") {
            continue;
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            continue;
        }
        let line = trimmed.trim_start_matches('#').trim();
        let line = line.replace("**", "").replace("__", "");
        let line = strip_inline_code(&line);
        let line = strip_md_links(&line);
        let line = line.trim_start_matches("- ")
            .trim_start_matches("* ")
            .trim_start_matches("+ ");

        if !line.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(line);
        }
    }
    out
}

fn strip_inline_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_code = false;
    for ch in s.chars() {
        if ch == '`' {
            in_code = !in_code;
        } else if !in_code {
            out.push(ch);
        }
    }
    out
}

fn strip_md_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch == '[' {
            // Look for closing ] in the remaining string
            if let Some(close_pos) = s[i+1..].find(']') {
                let close = i + 1 + close_pos;
                // Check for (url) after ]
                if s.as_bytes().get(close + 1) == Some(&b'(') {
                    if let Some(paren_len) = s[close+2..].find(')') {
                        // Output link text only, skip [, ], (url)
                        out.push_str(&s[i+1..close]);
                        // Advance past the entire [text](url) construct
                        let skip_to = close + 2 + paren_len + 1;
                        while let Some(&(pos, _)) = chars.peek() {
                            if pos >= skip_to { break; }
                            chars.next();
                        }
                        continue;
                    }
                }
            }
        }
        out.push(ch);
    }
    out
}

/// Strip HTML tags — delegates to content::strip_xml_tags.
fn strip_html_tags(html: &str) -> String {
    crate::content::strip_xml_tags(html)
}

// ─── Embed Hash ───

/// Stable hash of embed text to detect content changes without re-embedding.
/// Uses a simple FNV-1a hash — stable across Rust versions unlike DefaultHasher.
pub fn embed_hash(text: &str) -> String {
    // [Tier 2 fix #7] FNV-1a instead of DefaultHasher for cross-version stability
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    format!("{:016x}", hash)
}

// ─── OpenRouter API Client ───

/// Embed a batch of texts via OpenRouter. Returns one vector per input text.
pub fn embed_texts(api_key: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let mut all_vectors: Vec<Option<Vec<f32>>> = vec![None; texts.len()];

    for batch_start in (0..texts.len()).step_by(API_BATCH_SIZE) {
        let batch_end = (batch_start + API_BATCH_SIZE).min(texts.len());
        let batch: Vec<&str> = texts[batch_start..batch_end].iter().map(|s| s.as_str()).collect();

        let body = ureq::json!({
            "model": EMBED_MODEL,
            "input": batch,
            "dimensions": EMBED_DIMS,
        });

        let mut last_err = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(1000 * (1 << attempt)));
            }
            match ureq::post(API_URL)
                .set("Authorization", &format!("Bearer {}", api_key))
                .set("Content-Type", "application/json")
                .timeout(std::time::Duration::from_millis(API_TIMEOUT_MS))
                .send_json(body.clone())
            {
                Ok(resp) => {
                    let json: serde_json::Value = resp.into_json()?;
                    let data = json["data"].as_array()
                        .ok_or_else(|| anyhow!("Missing 'data' in API response"))?;

                    for item in data {
                        let idx = item["index"].as_u64()
                            .ok_or_else(|| anyhow!("Missing 'index'"))? as usize;

                        // [Tier 1 fix #4] Bounds check on API response index
                        let abs_idx = batch_start + idx;
                        if abs_idx >= all_vectors.len() {
                            return Err(anyhow!("API returned out-of-bounds index {}", abs_idx));
                        }

                        let embedding = item["embedding"].as_array()
                            .ok_or_else(|| anyhow!("Missing 'embedding'"))?;
                        let vec: Result<Vec<f32>, _> = embedding.iter()
                            .map(|v| v.as_f64()
                                .ok_or_else(|| anyhow!("Non-numeric embedding value"))
                                .map(|f| f as f32))
                            .collect();
                        let vec = vec?;
                        if vec.len() == EMBED_DIMS {
                            all_vectors[abs_idx] = Some(vec);
                        }
                    }
                    last_err = None;
                    break;
                }
                Err(e) => {
                    // [Tier 1 fix #3] Sanitize error — don't leak API key
                    let err_msg = format!("{}", e);
                    let sanitized = if err_msg.contains(api_key) {
                        err_msg.replace(api_key, "[REDACTED]")
                    } else {
                        err_msg
                    };
                    last_err = Some(format!("Embedding API error (attempt {}): {}", attempt + 1, sanitized));
                }
            }
        }
        if let Some(err) = last_err {
            return Err(anyhow!(err));
        }
    }

    all_vectors.into_iter()
        .enumerate()
        .map(|(i, v)| v.ok_or_else(|| anyhow!("Missing vector for index {}", i)))
        .collect()
}

// ─── Vector Serialization ───

pub fn vec_to_bytes(vec: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vec.len() * 4);
    for &f in vec {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

pub fn bytes_to_vec(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() != VECTOR_BYTES {
        return None;
    }
    let mut vec = Vec::with_capacity(EMBED_DIMS);
    for chunk in bytes.chunks_exact(4) {
        vec.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(vec)
}

// ─── Cosine Similarity ───

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    // Iterator form — LLVM auto-vectorizes this reliably with opt-level=3 + LTO
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum();
    let norm_b: f32 = b.iter().map(|x| x * x).sum();
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

// ─── Query Embedding ───

// [Tier 2 fix #9] Removed in-process cache — CLI exits after each invocation,
// cache never serves a hit. Just call the API directly.
pub fn embed_query(api_key: &str, query: &str) -> Result<Vec<f32>> {
    let texts = vec![query.to_string()];
    let mut vectors = embed_texts(api_key, &texts)?;
    if vectors.is_empty() {
        return Err(anyhow!("No vector returned for query"));
    }
    Ok(vectors.remove(0))
}

// ─── Content Reader for Embedding ───

/// Read file content for embedding. Uses proper extraction for binary formats.
/// [Tier 1 fix #1] PDF/DOCX/XLSX now use content extractors, not raw byte read.
pub fn read_file_for_embed(path: &str, ext: &str) -> Option<String> {
    match ext {
        // Binary formats — use proper extraction
        "pdf" | "docx" | "xlsx" => {
            crate::content::extract_content_for_embed(std::path::Path::new(path), ext).ok()
        }
        // Text formats — read directly, cap at 5KB
        _ => {
            let mut file = std::fs::File::open(path).ok()?;
            let mut buf = vec![0u8; 5000];
            let n = std::io::Read::read(&mut file, &mut buf).ok()?;
            buf.truncate(n);
            match String::from_utf8(buf) {
                Ok(s) => Some(s),
                Err(e) => {
                    let bytes = e.into_bytes();
                    Some(String::from_utf8_lossy(&bytes[..n]).into_owned())
                }
            }
        }
    }
}
