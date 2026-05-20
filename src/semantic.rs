//! Semantic embedding tier for Findr.
//!
//! Optional layer: only active when user provides an OpenRouter API key.
//! Uses pplx-embed-v1-0.6b (512d) to embed file content and match by meaning.

use anyhow::{anyhow, Result};
use std::path::Path;
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── cosine_similarity ──

    #[test]
    fn cosine_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0, 4.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-5, "identical vectors should have sim ~1.0, got {}", sim);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-5, "orthogonal vectors should have sim ~0, got {}", sim);
    }

    #[test]
    fn cosine_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-5, "opposite vectors should have sim ~-1, got {}", sim);
    }

    #[test]
    fn cosine_zero_vector() {
        let a = vec![1.0, 2.0];
        let b = vec![0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0, "zero vector should give 0");
    }

    #[test]
    fn cosine_512_dim_perf() {
        let a: Vec<f32> = (0..512).map(|i| (i as f32).sin()).collect();
        let b: Vec<f32> = (0..512).map(|i| (i as f32).cos()).collect();

        let start = std::time::Instant::now();
        let iterations = 100_000;
        let mut result = 0.0f32;
        for _ in 0..iterations {
            result += cosine_similarity(&a, &b);
        }
        let elapsed = start.elapsed();
        let per_op_ns = elapsed.as_nanos() / iterations as u128;
        eprintln!("cosine_similarity(512d): {}ns/op (sum={})", per_op_ns, result);
        // Release: <1μs (SIMD), Debug: <100μs
        assert!(per_op_ns < 100_000, "cosine too slow: {}ns/op", per_op_ns);
    }

    // ── vec_to_bytes / bytes_to_vec roundtrip ──

    #[test]
    fn vector_serialization_roundtrip() {
        let original: Vec<f32> = (0..EMBED_DIMS).map(|i| i as f32 * 0.001).collect();
        let bytes = vec_to_bytes(&original);
        assert_eq!(bytes.len(), VECTOR_BYTES);

        let restored = bytes_to_vec(&bytes).unwrap();
        assert_eq!(original.len(), restored.len());
        for (a, b) in original.iter().zip(restored.iter()) {
            assert!((a - b).abs() < 1e-7, "mismatch: {} vs {}", a, b);
        }
    }

    #[test]
    fn bytes_to_vec_wrong_size() {
        assert!(bytes_to_vec(&[0u8; 100]).is_none());
        assert!(bytes_to_vec(&[]).is_none());
    }

    #[test]
    fn vec_serialization_special_values() {
        let special = vec![0.0f32, -0.0, f32::INFINITY, f32::NEG_INFINITY, 1.0];
        let bytes = vec_to_bytes(&special);
        // Wrong size for bytes_to_vec (expects VECTOR_BYTES), so test raw roundtrip
        let mut restored = Vec::new();
        for chunk in bytes.chunks_exact(4) {
            restored.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        assert_eq!(special.len(), restored.len());
        assert_eq!(restored[0], 0.0);
        assert!(restored[2].is_infinite());
    }

    // ── embed_hash ──

    #[test]
    fn hash_deterministic() {
        let h1 = embed_hash("hello world");
        let h2 = embed_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_different_inputs() {
        let h1 = embed_hash("hello");
        let h2 = embed_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_format() {
        let h = embed_hash("test");
        assert_eq!(h.len(), 16, "FNV-1a hash should be 16 hex chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── build_embed_text ──

    #[test]
    fn embed_text_markdown() {
        let text = build_embed_text("notes.md", "# Title\n\nSome **bold** content here", "md");
        assert!(text.is_some());
        let text = text.unwrap();
        assert!(text.contains("File: notes.md"));
        assert!(text.contains("Title"));
        assert!(!text.contains("**"), "markdown formatting should be stripped");
    }

    #[test]
    fn embed_text_code_truncated() {
        let long_code = "fn main() {\n".repeat(100);
        let text = build_embed_text("main.rs", &long_code, "rs");
        assert!(text.is_some());
        let text = text.unwrap();
        // Code should be truncated to ~200 chars
        assert!(text.len() < 400, "code embed should be short, got {} bytes", text.len());
    }

    #[test]
    fn embed_text_skips_images() {
        assert!(build_embed_text("photo.png", "", "png").is_none());
        assert!(build_embed_text("icon.svg", "", "svg").is_none());
    }

    #[test]
    fn embed_text_pdf() {
        let text = build_embed_text("report.pdf", "Financial results Q4 2024", "pdf");
        assert!(text.is_some());
        assert!(text.unwrap().contains("Financial results"));
    }

    #[test]
    fn embed_text_csv() {
        let csv = "name,email,company\njohn,john@example.com,Acme";
        let text = build_embed_text("contacts.csv", csv, "csv");
        assert!(text.is_some());
        // CSV gets 400 char truncation
    }

    #[test]
    fn embed_text_html() {
        let html = "<html><body><p>Hello World</p></body></html>";
        let text = build_embed_text("page.html", html, "html");
        assert!(text.is_some());
        let text = text.unwrap();
        assert!(!text.contains("<html>"), "HTML tags should be stripped");
        assert!(text.contains("Hello World"));
    }

    // ── strip_md ──

    #[test]
    fn strip_md_headers() {
        let md = "# Title\n## Subtitle\nContent here";
        let stripped = strip_md(md);
        assert!(!stripped.contains('#'));
        assert!(stripped.contains("Title"));
        assert!(stripped.contains("Content here"));
    }

    #[test]
    fn strip_md_bold_italic() {
        let md = "This is **bold** and __also bold__";
        let stripped = strip_md(md);
        assert!(!stripped.contains("**"));
        assert!(!stripped.contains("__"));
        assert!(stripped.contains("bold"));
    }

    #[test]
    fn strip_md_code_blocks() {
        let md = "Before\n```rust\nfn main() {}\n```\nAfter";
        let stripped = strip_md(md);
        assert!(!stripped.contains("fn main"));
        assert!(stripped.contains("Before"));
        assert!(stripped.contains("After"));
    }

    #[test]
    fn strip_md_tables() {
        let md = "| Col1 | Col2 |\n|---|---|\n| val1 | val2 |";
        let stripped = strip_md(md);
        assert!(!stripped.contains('|'));
    }

    #[test]
    fn strip_md_removes_links() {
        let md = "Check [this link](https://example.com) out";
        let stripped = strip_md(md);
        assert!(stripped.contains("this link"));
        assert!(!stripped.contains("https://"));
    }

    #[test]
    fn strip_md_removes_inline_code() {
        let md = "Use the `println!` macro";
        let stripped = strip_md(md);
        assert!(!stripped.contains('`'));
    }

    #[test]
    fn strip_md_list_markers() {
        let md = "- item one\n* item two\n+ item three";
        let stripped = strip_md(md);
        assert!(stripped.contains("item one"));
        // List markers removed
    }

    // ── strip_md_links (standalone function) ──

    #[test]
    fn links_basic() {
        assert_eq!(super::strip_md_links("[text](url)"), "text");
    }

    #[test]
    fn links_multiple() {
        let result = super::strip_md_links("[a](1) and [b](2)");
        assert_eq!(result, "a and b");
    }

    #[test]
    fn links_no_links() {
        assert_eq!(super::strip_md_links("no links here"), "no links here");
    }

    #[test]
    fn links_nested_brackets() {
        let result = super::strip_md_links("[text [inner]](url)");
        assert!(result.contains("text"));
    }

    // ── strip_inline_code (standalone function) ──

    #[test]
    fn inline_code_basic() {
        // strip_inline_code removes content between backticks
        assert_eq!(super::strip_inline_code("use `foo` here"), "use  here");
    }

    #[test]
    fn inline_code_multiple() {
        assert_eq!(super::strip_inline_code("`a` and `b`"), " and ");
    }

    // ── first_meaningful_line ──

    #[test]
    fn first_line_skips_short() {
        let text = "ab\n# Header\nContent";
        assert_eq!(first_meaningful_line(text), "Header");
    }

    #[test]
    fn first_line_strips_hash() {
        assert_eq!(first_meaningful_line("# My Title"), "My Title");
    }

    #[test]
    fn first_line_empty() {
        assert_eq!(first_meaningful_line(""), "");
        assert_eq!(first_meaningful_line("ab\ncd"), "");
    }

    // ── Constants validation ──

    #[test]
    fn embed_dims_consistent() {
        assert_eq!(VECTOR_BYTES, EMBED_DIMS * 4);
    }

    #[test]
    fn cosine_threshold_reasonable() {
        assert!(COSINE_THRESHOLD > 0.0 && COSINE_THRESHOLD < 1.0);
    }

    // ── Performance: vector serialization ──

    #[test]
    fn vector_serialization_perf() {
        let vec: Vec<f32> = (0..EMBED_DIMS).map(|i| i as f32 * 0.001).collect();

        let start = std::time::Instant::now();
        let iterations = 100_000;
        for _ in 0..iterations {
            let bytes = vec_to_bytes(&vec);
            let _ = bytes_to_vec(&bytes);
        }
        let elapsed = start.elapsed();
        let per_op_ns = elapsed.as_nanos() / iterations as u128;
        eprintln!("vec roundtrip (512d): {}ns/op", per_op_ns);
        // Release: <5μs, Debug: <200μs
        assert!(per_op_ns < 200_000, "serialization too slow: {}ns/op", per_op_ns);
    }

    // ── Performance: cosine on all docs ──

    #[test]
    fn cosine_bulk_search_perf() {
        // Simulate searching 5000 documents
        let query: Vec<f32> = (0..EMBED_DIMS).map(|i| (i as f32).sin()).collect();
        let docs: Vec<Vec<f32>> = (0..5000)
            .map(|d| (0..EMBED_DIMS).map(|i| ((i + d) as f32).cos()).collect())
            .collect();

        let start = std::time::Instant::now();
        let mut above_threshold = 0;
        for doc in &docs {
            if cosine_similarity(&query, doc) > COSINE_THRESHOLD {
                above_threshold += 1;
            }
        }
        let elapsed = start.elapsed();
        eprintln!("cosine scan 5000 docs (512d): {:?} ({} above threshold)",
            elapsed, above_threshold);
        // Release: <20ms, Debug: <2000ms
        assert!(elapsed.as_millis() < 2000,
            "bulk cosine too slow: {:?}", elapsed);
    }

    // ── Property-based tests ──

    use proptest::prelude::*;

    fn arb_vec(dims: usize) -> impl Strategy<Value = Vec<f32>> {
        proptest::collection::vec(-10.0f32..10.0, dims)
    }

    proptest! {
        #[test]
        fn cosine_self_similarity(v in arb_vec(32)) {
            // Skip zero vectors
            let norm: f32 = v.iter().map(|x| x * x).sum();
            if norm > 1e-10 {
                let sim = cosine_similarity(&v, &v);
                prop_assert!((sim - 1.0).abs() < 1e-4,
                    "self-similarity should be ~1.0, got {}", sim);
            }
        }

        #[test]
        fn cosine_symmetry(a in arb_vec(32), b in arb_vec(32)) {
            let ab = cosine_similarity(&a, &b);
            let ba = cosine_similarity(&b, &a);
            prop_assert!((ab - ba).abs() < 1e-6,
                "cosine not symmetric: {} vs {}", ab, ba);
        }

        #[test]
        fn cosine_bounded(a in arb_vec(32), b in arb_vec(32)) {
            let sim = cosine_similarity(&a, &b);
            prop_assert!(sim >= -1.0 - 1e-5 && sim <= 1.0 + 1e-5,
                "cosine out of [-1,1]: {}", sim);
        }

        #[test]
        fn cosine_negation(a in arb_vec(32), b in arb_vec(32)) {
            let neg_b: Vec<f32> = b.iter().map(|x| -x).collect();
            let sim = cosine_similarity(&a, &b);
            let neg_sim = cosine_similarity(&a, &neg_b);
            prop_assert!((sim + neg_sim).abs() < 1e-5,
                "cos(a,-b) should equal -cos(a,b): {} vs {}", neg_sim, -sim);
        }

        #[test]
        fn vec_roundtrip(v in arb_vec(EMBED_DIMS)) {
            let bytes = vec_to_bytes(&v);
            let restored = bytes_to_vec(&bytes).unwrap();
            for (a, b) in v.iter().zip(restored.iter()) {
                prop_assert!((a - b).abs() < 1e-7,
                    "roundtrip mismatch: {} vs {}", a, b);
            }
        }

        #[test]
        fn embed_hash_deterministic(input in "\\PC{0,200}") {
            let h1 = embed_hash(&input);
            let h2 = embed_hash(&input);
            prop_assert_eq!(h1, h2);
        }

        #[test]
        fn embed_hash_length(input in "\\PC{0,200}") {
            let h = embed_hash(&input);
            prop_assert_eq!(h.len(), 16, "hash should be 16 hex chars");
        }

        #[test]
        fn build_embed_text_never_panics(
            filename in "[a-z]{1,20}\\.[a-z]{1,5}",
            content in "\\PC{0,2000}",
            ext in prop_oneof![
                Just("md"), Just("txt"), Just("pdf"), Just("rs"),
                Just("csv"), Just("html"), Just("png"), Just("xyz")
            ]
        ) {
            let _ = build_embed_text(&filename, &content, ext);
        }

        #[test]
        fn build_embed_text_contains_filename(
            name in "[a-z]{3,10}",
            ext in prop_oneof![Just("md"), Just("pdf"), Just("rs"), Just("csv")]
        ) {
            let filename = format!("{}.{}", name, ext);
            if let Some(text) = build_embed_text(&filename, "some content here", &ext) {
                prop_assert!(text.contains(&filename),
                    "embed text should contain filename {:?}", filename);
            }
        }
    }

    // ── HNSW roundtrip ──

    #[test]
    fn hnsw_build_query_roundtrip() {
        let dir = tempfile::tempdir().unwrap();

        // Create 50 synthetic 512d vectors
        let vectors: Vec<(String, Vec<f32>)> = (0..50)
            .map(|i| {
                let vec: Vec<f32> = (0..EMBED_DIMS).map(|d| ((d + i) as f32 * 0.01).sin()).collect();
                (format!("/test/file_{}.txt", i), vec)
            })
            .collect();

        // Build and save
        build_and_save_hnsw(&vectors, dir.path()).unwrap();

        // Verify files exist
        assert!(hnsw_index_exists(dir.path()));

        // Query with the first vector — should find itself as top result
        let results = query_hnsw(&vectors[0].1, dir.path(), 5).unwrap();
        assert!(!results.is_empty(), "should find at least one neighbor");
        assert_eq!(results[0].0, "/test/file_0.txt", "top result should be the query vector itself");
        assert!(results[0].1 > 0.99, "self-similarity should be ~1.0, got {}", results[0].1);
    }

    #[test]
    fn hnsw_delete_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        let vectors: Vec<(String, Vec<f32>)> = vec![
            ("a.txt".into(), (0..EMBED_DIMS).map(|i| i as f32).collect()),
        ];
        build_and_save_hnsw(&vectors, dir.path()).unwrap();
        assert!(hnsw_index_exists(dir.path()));

        delete_hnsw_index(dir.path());
        assert!(!hnsw_index_exists(dir.path()));
    }

    #[test]
    fn hnsw_query_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let query: Vec<f32> = vec![0.0; EMBED_DIMS];
        let result = query_hnsw(&query, dir.path(), 5);
        assert!(result.is_err());
    }

    #[test]
    fn hnsw_empty_vectors_noop() {
        let dir = tempfile::tempdir().unwrap();
        let vectors: Vec<(String, Vec<f32>)> = vec![];
        build_and_save_hnsw(&vectors, dir.path()).unwrap();
        assert!(!hnsw_index_exists(dir.path()));
    }
}

// ─── HNSW Index ───

const HNSW_MAX_NB_CONNECTION: usize = 16;
const HNSW_MAX_LAYER: usize = 16;
const HNSW_EF_CONSTRUCTION: usize = 200;
const HNSW_EF_SEARCH: usize = 32;
const HNSW_BASENAME: &str = "semantic";

/// Build HNSW index from all vectors and save to a temp dir, then atomically
/// rename into place. The ID→path mapping is stored as `semantic.paths` (JSON).
///
/// Uses catch_unwind around hnsw_rs calls because the library panics on I/O errors.
pub fn build_and_save_hnsw(vectors: &[(String, Vec<f32>)], dir: &Path) -> Result<()> {
    use hnsw_rs::prelude::*;

    if vectors.is_empty() {
        return Ok(());
    }

    // Build into temp dir for atomic swap
    let tmp_dir = dir.join("hnsw.new");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir)?;

    let hnsw = Hnsw::<f32, DistCosine>::new(
        HNSW_MAX_NB_CONNECTION,
        vectors.len(),
        HNSW_MAX_LAYER,
        HNSW_EF_CONSTRUCTION,
        DistCosine,
    );

    // Parallel insert for large vector sets, sequential for small
    if vectors.len() >= 1000 {
        let data: Vec<(&Vec<f32>, usize)> = vectors.iter().enumerate()
            .map(|(i, (_, v))| (v, i))
            .collect();
        hnsw.parallel_insert(&data);
    } else {
        for (i, (_, vec)) in vectors.iter().enumerate() {
            hnsw.insert((vec.as_slice(), i));
        }
    }

    // Persist HNSW graph + data (catch_unwind: hnsw_rs panics on I/O errors)
    let dump_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hnsw.file_dump(&tmp_dir, HNSW_BASENAME)
    }));
    match dump_result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e);
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(anyhow!("HNSW file_dump panicked — disk full or permission error"));
        }
    }

    // Persist ID→path mapping (atomic: write to tmp then rename)
    let paths: Vec<&str> = vectors.iter().map(|(p, _)| p.as_str()).collect();
    let tmp_mapping = tmp_dir.join(format!("{}.paths", HNSW_BASENAME));
    std::fs::write(&tmp_mapping, serde_json::to_string(&paths)?)?;

    // Atomic swap: move files from tmp to live dir
    let files = [
        format!("{}.hnsw.data", HNSW_BASENAME),
        format!("{}.hnsw.graph", HNSW_BASENAME),
        format!("{}.paths", HNSW_BASENAME),
    ];
    for f in &files {
        let src = tmp_dir.join(f);
        let dst = dir.join(f);
        if src.exists() {
            std::fs::rename(&src, &dst)?;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp_dir);

    Ok(())
}

/// Query HNSW index for nearest neighbors. Returns (path, cosine_similarity) pairs.
///
/// Wraps load+search in catch_unwind because hnsw_rs panics on corrupt files.
pub fn query_hnsw(query_vec: &[f32], dir: &Path, top_k: usize) -> Result<Vec<(String, f32)>> {
    use hnsw_rs::prelude::*;

    // Load path mapping
    let mapping_path = dir.join(format!("{}.paths", HNSW_BASENAME));
    let mapping_json = std::fs::read_to_string(&mapping_path)?;
    let paths: Vec<String> = serde_json::from_str(&mapping_json)?;

    // Load + search inside catch_unwind (hnsw_rs asserts/panics on corrupt data).
    // Hnsw<'b> borrows from HnswIo, so both must live inside the closure.
    let dir_owned = dir.to_path_buf();
    let query_owned = query_vec.to_vec();
    let search_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut io = HnswIo::new(&dir_owned, HNSW_BASENAME);
        let hnsw: Hnsw<f32, DistCosine> = io.load_hnsw()?;
        let neighbours = hnsw.search(&query_owned, top_k, HNSW_EF_SEARCH);
        // Extract owned data before Hnsw is dropped
        let results: Vec<(usize, f32)> = neighbours.iter()
            .map(|n| (n.d_id, n.distance))
            .collect();
        Ok::<_, anyhow::Error>(results)
    }));

    let raw_results = match search_result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            delete_hnsw_index(dir);
            return Err(anyhow!("HNSW index corrupted — deleted, will rebuild on next embed"));
        }
    };

    // Convert IDs + distances to paths + similarities
    let results: Vec<(String, f32)> = raw_results
        .into_iter()
        .filter_map(|(id, distance)| {
            if id < paths.len() {
                let sim = 1.0 - distance;
                if sim >= COSINE_THRESHOLD {
                    Some((paths[id].clone(), sim))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    Ok(results)
}

/// Check if HNSW index files exist on disk and are non-empty.
pub fn hnsw_index_exists(dir: &Path) -> bool {
    let data = dir.join(format!("{}.hnsw.data", HNSW_BASENAME));
    let graph = dir.join(format!("{}.hnsw.graph", HNSW_BASENAME));
    let paths = dir.join(format!("{}.paths", HNSW_BASENAME));

    fn non_empty(p: &Path) -> bool {
        std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
    }
    non_empty(&data) && non_empty(&graph) && non_empty(&paths)
}

/// Delete HNSW index files.
pub fn delete_hnsw_index(dir: &Path) {
    for ext in &["hnsw.data", "hnsw.graph", "paths"] {
        let _ = std::fs::remove_file(dir.join(format!("{}.{}", HNSW_BASENAME, ext)));
    }
}

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
