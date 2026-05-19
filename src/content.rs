use anyhow::Result;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, QueryParser};
use tantivy::schema::*;
use tantivy::directory::MmapDirectory;
use tantivy::{doc, Index, IndexWriter, ReloadPolicy, Term};

const CONTENT_EXTRACTABLE: &[&str] = &[
    "pdf", "docx", "xlsx", "txt", "md", "csv", "json", "yml", "yaml", "xml",
    "rs", "ts", "js", "py", "go", "rb", "java", "c", "cpp", "h",
    "html", "css", "toml", "ini", "cfg", "conf", "sh", "zsh",
    "log", "sql", "tsx", "jsx",
    "png", "jpg", "jpeg", "heic",
];

pub const OCR_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "heic"];

const SCANNED_PDF_TEXT_THRESHOLD: usize = 50;

/// Tantivy IndexWriter heap size in bytes.
const TANTIVY_WRITER_HEAP: usize = 50_000_000;

/// Escape Tantivy query syntax characters so user input is treated as literal text.
fn escape_tantivy_query(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len() + 8);
    for ch in query.chars() {
        match ch {
            '+' | '-' | '!' | '(' | ')' | '{' | '}' | '[' | ']'
            | '^' | '"' | '~' | '*' | '?' | ':' | '\\' | '/' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub struct ContentIndex {
    index: Index,
    path_field: Field,
    filename_field: Field,
    content_field: Field,
    extension_field: Field,
}

pub struct ContentSearchResult {
    pub path: String,
    pub filename: String,
    pub extension: String,
    #[allow(dead_code)]
    pub score: f32,
    pub snippet: Option<String>,
    /// Position of first match as fraction of document (0.0 = start, 1.0 = end)
    pub match_position: f64,
}

impl ContentIndex {
    pub fn open_or_create(index_dir: &Path) -> Result<Self> {
        let mut schema_builder = Schema::builder();
        let path_field = schema_builder.add_text_field("path", STRING | STORED);
        let filename_field = schema_builder.add_text_field("filename", TEXT | STORED);
        let content_field = schema_builder.add_text_field("content", TEXT | STORED);
        let extension_field = schema_builder.add_text_field("extension", STRING | STORED);
        let schema = schema_builder.build();

        std::fs::create_dir_all(index_dir)?;

        let mmap_dir = MmapDirectory::open(index_dir)?;
        let index = if Index::exists(&mmap_dir)? {
            Index::open_in_dir(index_dir)?
        } else {
            Index::create_in_dir(index_dir, schema.clone())?
        };

        Ok(Self {
            index,
            path_field,
            filename_field,
            content_field,
            extension_field,
        })
    }

    /// Incrementally add new files to the content index (no delete, just append).
    /// Delete-by-term + re-add for new and modified files.
    /// Safe for new files (delete on non-existent term is a no-op).
    /// Eliminates duplicate docs for modified files.
    pub fn update_files(&self, files: &[(String, String, Option<String>)]) -> Result<usize> {
        let mut writer: IndexWriter = self.index.writer(TANTIVY_WRITER_HEAP)?;
        let mut count = 0;

        for (path, filename, extension) in files {
            let ext = extension.as_deref().unwrap_or("");
            if !CONTENT_EXTRACTABLE.contains(&ext) {
                continue;
            }

            // Delete existing doc for this path (no-op if not present)
            let delete_term = Term::from_field_text(self.path_field, path);
            writer.delete_term(delete_term);

            let content = match extract_content(Path::new(path), ext) {
                Ok(c) if !c.is_empty() => c,
                _ => continue,
            };

            writer.add_document(doc!(
                self.path_field => path.as_str(),
                self.filename_field => filename.as_str(),
                self.content_field => content.as_str(),
                self.extension_field => ext,
            ))?;
            count += 1;
        }

        if count > 0 {
            writer.commit()?;
        }
        Ok(count)
    }

    /// Add pre-extracted content to Tantivy (skip re-extraction).
    /// Used by OCR batch to avoid re-running OCR per file.
    pub fn update_files_with_content(&self, files: &[(String, String, Option<String>, String)]) -> Result<usize> {
        let mut writer: IndexWriter = self.index.writer(TANTIVY_WRITER_HEAP)?;
        let mut count = 0;

        for (path, filename, extension, content) in files {
            if content.is_empty() {
                continue;
            }
            let ext = extension.as_deref().unwrap_or("");

            let delete_term = Term::from_field_text(self.path_field, path);
            writer.delete_term(delete_term);

            writer.add_document(doc!(
                self.path_field => path.as_str(),
                self.filename_field => filename.as_str(),
                self.content_field => content.as_str(),
                self.extension_field => ext,
            ))?;
            count += 1;
        }

        if count > 0 {
            writer.commit()?;
        }
        Ok(count)
    }

    /// Remove documents from Tantivy for paths that no longer exist on disk.
    pub fn delete_files(&self, paths: &[String]) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }
        let mut writer: IndexWriter = self.index.writer(TANTIVY_WRITER_HEAP)?;
        for path in paths {
            let delete_term = Term::from_field_text(self.path_field, path);
            writer.delete_term(delete_term);
        }
        writer.commit()?;
        Ok(paths.len())
    }

    /// Full reindex — deletes all, then indexes everything.
    /// Text files extracted in parallel via rayon. OCR files skipped (handled separately).
    pub fn index_files(&self, files: &[(String, String, Option<String>)]) -> Result<usize> {
        let mut writer: IndexWriter = self.index.writer(TANTIVY_WRITER_HEAP)?; // 50MB heap
        writer.delete_all_documents()?;
        writer.commit()?;

        // Filter to text-extractable files only (skip OCR — handled by background phase)
        let text_files: Vec<&(String, String, Option<String>)> = files.iter()
            .filter(|(_, _, ext)| {
                let e = ext.as_deref().unwrap_or("");
                CONTENT_EXTRACTABLE.contains(&e) && !OCR_EXTENSIONS.contains(&e)
            })
            .collect();

        eprintln!("  Extracting content from {} files (parallel)...", text_files.len());

        // Parallel extraction — extract_content is a pure function, safe to parallelize
        let extracted: Vec<(&str, &str, &str, String)> = text_files.par_iter()
            .filter_map(|(path, filename, extension)| {
                let ext = extension.as_deref().unwrap_or("");
                match extract_content(Path::new(path), ext) {
                    Ok(c) if !c.is_empty() => Some((path.as_str(), filename.as_str(), ext, c)),
                    _ => None,
                }
            })
            .collect();

        // Sequential Tantivy write (IndexWriter is not thread-safe)
        let mut count = 0;
        let mut batch = 0;

        for (path, filename, ext, content) in &extracted {
            writer.add_document(doc!(
                self.path_field => *path,
                self.filename_field => *filename,
                self.content_field => content.as_str(),
                self.extension_field => *ext,
            ))?;

            count += 1;
            batch += 1;

            if batch >= 500 {
                writer.commit()?;
                batch = 0;
                if count % 2000 == 0 {
                    eprint!("\r  Content indexed: {} files", count);
                }
            }
        }

        writer.commit()?;
        eprintln!("\r  Content indexed: {} files", count);
        Ok(count)
    }

    pub fn search(&self, query_str: &str, limit: usize, type_filter: Option<&str>) -> Result<Vec<ContentSearchResult>> {
        let reader = self.index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let searcher = reader.searcher();

        // Escape Tantivy query syntax characters to treat input as literal text
        let escaped = escape_tantivy_query(query_str);
        let query_parser = QueryParser::for_index(&self.index, vec![self.content_field, self.filename_field]);
        let query = query_parser.parse_query(&escaped)?;
        let mut top_docs = searcher.search(&query, &TopDocs::with_limit(limit * 2))?;

        // Only fall back to fuzzy when exact search found very few results.
        // Tantivy fuzzy with distance 2 matches too many irrelevant tokens.
        if top_docs.len() < 3 {
            let distance = if query_str.len() <= 6 { 1 } else { 2 };
            let term = Term::from_field_text(self.content_field, &query_str.to_lowercase());
            let fuzzy_content = FuzzyTermQuery::new(term, distance, true);
            let term_fn = Term::from_field_text(self.filename_field, &query_str.to_lowercase());
            let fuzzy_filename = FuzzyTermQuery::new(term_fn, distance, true);

            let fuzzy_query = BooleanQuery::new(vec![
                (Occur::Should, Box::new(fuzzy_content)),
                (Occur::Should, Box::new(fuzzy_filename)),
            ]);

            let fuzzy_docs = searcher.search(&fuzzy_query, &TopDocs::with_limit(limit * 2))?;

            // Merge, avoiding duplicates
            let existing: std::collections::HashSet<_> = top_docs.iter().map(|(_, addr)| *addr).collect();
            for (score, addr) in fuzzy_docs {
                if !existing.contains(&addr) {
                    // Significantly lower score for fuzzy matches
                    top_docs.push((score * 0.5, addr));
                }
            }
            top_docs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            top_docs.truncate(limit * 2);
        }

        let mut results = Vec::new();
        for (score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;

            let path = doc.get_first(self.path_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let filename = doc.get_first(self.filename_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let extension = doc.get_first(self.extension_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Apply type filter
            if let Some(filter) = type_filter {
                if extension != filter {
                    continue;
                }
            }

            let content = doc.get_first(self.content_field)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let (snippet, match_position) = extract_snippet_with_position(content, query_str);

            results.push(ContentSearchResult {
                path,
                filename,
                extension,
                score,
                snippet,
                match_position,
            });

            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    pub fn doc_count(&self) -> Result<u64> {
        let reader = self.index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let searcher = reader.searcher();
        Ok(searcher.num_docs())
    }
}

/// Max bytes to read for document files (PDF, DOCX, XLSX).
const MAX_DOC_READ_SIZE: u64 = 20 * 1024 * 1024; // 20MB

fn extract_content(path: &Path, ext: &str) -> Result<String> {
    match ext {
        "pdf" => extract_pdf(path),
        "docx" => extract_docx(path),
        "xlsx" => extract_xlsx(path),
        "png" | "jpg" | "jpeg" | "heic" => extract_ocr(path),
        _ => {
            // Text-based files: read only first 100KB via BufReader
            use std::io::Read;
            let file = std::fs::File::open(path)?;
            let mut reader = std::io::BufReader::new(file);
            let mut buf = vec![0u8; 102_400]; // 100KB
            let n = reader.read(&mut buf)?;
            buf.truncate(n);
            let content = String::from_utf8_lossy(&buf);
            Ok(content.into_owned())
        }
    }
}

fn extract_pdf(path: &Path) -> Result<String> {
    // Skip oversized files to avoid OOM during parallel extraction
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_DOC_READ_SIZE {
        return Err(anyhow::anyhow!("PDF too large ({} bytes)", meta.len()));
    }
    let bytes = std::fs::read(path)?;

    // Note: pdf-extract prints "unknown glyph name" warnings to stderr via eprintln!.
    // These can't be suppressed from within the process because Rust's Stderr caches fd 2.
    // The warnings are harmless and don't affect extraction quality.
    let result = std::panic::catch_unwind(|| {
        pdf_extract::extract_text_from_mem(&bytes)
    });

    let text = match result {
        Ok(Ok(text)) => text.chars().take(200_000).collect::<String>(),
        Ok(Err(e)) => {
            let msg = format!("PDF extraction error: {}", e);
            crate::errors::log_error(&format!("pdf:{}", path.display()), &msg);
            String::new()
        }
        Err(_) => {
            let msg = "PDF extraction panicked (malformed PDF)";
            crate::errors::log_error(&format!("pdf:{}", path.display()), msg);
            String::new()
        }
    };

    // Scanned PDF fallback: if text extraction yields very little, try OCR
    let trimmed = text.split_whitespace().collect::<String>();
    if trimmed.len() < SCANNED_PDF_TEXT_THRESHOLD {
        if let Ok(ocr_text) = extract_ocr(path) {
            if !ocr_text.is_empty() {
                // Combine any extracted text with OCR text
                if text.trim().is_empty() {
                    return Ok(ocr_text);
                }
                return Ok(format!("{}\n{}", text.trim(), ocr_text));
            }
        }
    }

    if text.trim().is_empty() {
        Err(anyhow::anyhow!("No text extracted from PDF"))
    } else {
        Ok(text)
    }
}

/// Strip XML tags: remove everything between < and >.
fn strip_xml_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut inside_tag = false;
    for ch in xml.chars() {
        if ch == '<' {
            inside_tag = true;
        } else if ch == '>' {
            inside_tag = false;
        } else if !inside_tag {
            out.push(ch);
        }
    }
    out
}

fn extract_docx(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_DOC_READ_SIZE {
        return Err(anyhow::anyhow!("DOCX too large ({} bytes)", meta.len()));
    }
    let bytes = std::fs::read(path)?;
    let result = std::panic::catch_unwind(|| -> Result<String> {
        let cursor = std::io::Cursor::new(&bytes);
        let mut archive = zip::ZipArchive::new(cursor)?;
        let mut xml = String::new();
        {
            let mut file = archive.by_name("word/document.xml")?;
            std::io::Read::read_to_string(&mut file, &mut xml)?;
        }
        let text = strip_xml_tags(&xml);
        // Collapse whitespace runs
        let clean: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        Ok(clean.chars().take(200_000).collect())
    });
    match result {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => {
            let msg = format!("DOCX extraction error: {}", e);
            crate::errors::log_error(&format!("docx:{}", path.display()), &msg);
            Err(anyhow::anyhow!("{}", msg))
        }
        Err(_) => {
            let msg = "DOCX extraction panicked";
            crate::errors::log_error(&format!("docx:{}", path.display()), msg);
            Err(anyhow::anyhow!("{}", msg))
        }
    }
}

fn extract_xlsx(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_DOC_READ_SIZE {
        return Err(anyhow::anyhow!("XLSX too large ({} bytes)", meta.len()));
    }
    let bytes = std::fs::read(path)?;
    let result = std::panic::catch_unwind(|| -> Result<String> {
        let cursor = std::io::Cursor::new(&bytes);
        let mut archive = zip::ZipArchive::new(cursor)?;
        let mut xml = String::new();
        {
            let mut file = archive.by_name("xl/sharedStrings.xml")?;
            std::io::Read::read_to_string(&mut file, &mut xml)?;
        }
        let text = strip_xml_tags(&xml);
        let clean: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        Ok(clean.chars().take(200_000).collect())
    });
    match result {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => {
            let msg = format!("XLSX extraction error: {}", e);
            crate::errors::log_error(&format!("xlsx:{}", path.display()), &msg);
            Err(anyhow::anyhow!("{}", msg))
        }
        Err(_) => {
            let msg = "XLSX extraction panicked";
            crate::errors::log_error(&format!("xlsx:{}", path.display()), msg);
            Err(anyhow::anyhow!("{}", msg))
        }
    }
}

/// Returns (snippet, match_position) where match_position is 0.0-1.0
/// (0.0 = match at very start of document, 1.0 = match at very end)
fn extract_snippet_with_position(content: &str, query: &str) -> (Option<String>, f64) {
    let query_lower = query.to_lowercase();
    let content_lower = content.to_lowercase();

    if let Some(pos) = content_lower.find(&query_lower) {
        let match_position = if content.is_empty() {
            0.5
        } else {
            pos as f64 / content.len() as f64
        };

        let raw_start = content[..pos].rfind('\n').map(|p| p + 1).unwrap_or(
            pos.saturating_sub(80)
        );
        let raw_end = content[pos..].find('\n').map(|p| pos + p).unwrap_or(
            (pos + 160).min(content.len())
        );
        // Snap to char boundaries to avoid panics on multibyte UTF-8
        let start = snap_to_char_boundary(content, raw_start, true);
        let end = snap_to_char_boundary(content, raw_end, false);
        let snippet = content[start..end].trim().to_string();
        let snippet = if snippet.len() > 200 {
            let truncated = truncate_str(&snippet, 200);
            format!("...{}...", truncated)
        } else {
            snippet
        };
        (Some(snippet), match_position)
    } else {
        // No exact match found (Tantivy tokenizer may have stemmed/split)
        let snippet = content.lines().next().map(|l| {
            if l.len() > 200 { format!("{}...", truncate_str(l, 200)) } else { l.to_string() }
        });
        (snippet, 0.5) // neutral position
    }
}

/// Snap a byte offset to the nearest char boundary.
/// If `backward` is true, snaps backward (for start offsets); otherwise forward (for end offsets).
fn snap_to_char_boundary(s: &str, offset: usize, backward: bool) -> usize {
    let offset = offset.min(s.len());
    if s.is_char_boundary(offset) {
        return offset;
    }
    if backward {
        let mut i = offset;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    } else {
        let mut i = offset;
        while i < s.len() && !s.is_char_boundary(i) {
            i += 1;
        }
        i
    }
}

/// Truncate a string to at most `max_bytes` without splitting a UTF-8 character.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── OCR via findr-ocr Swift CLI ──────────────────────────────────────

use std::sync::OnceLock;

/// Cached result of OCR binary lookup. None = not found (logged once).
static OCR_BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Cached reverse geocoder for GPS → city/country resolution.
static GEOCODER: OnceLock<reverse_geocoder::ReverseGeocoder> = OnceLock::new();

/// Locate the findr-ocr binary. Checks same dir as current exe, then ~/.local/bin.
pub fn find_ocr_binary() -> Option<PathBuf> {
    OCR_BINARY.get_or_init(|| {
        // 1. Same directory as current executable (works for Raycast assets/ and dev builds)
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join("findr-ocr");
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }

        // 2. ~/.local/bin
        if let Ok(home) = std::env::var("HOME") {
            let candidate = PathBuf::from(home).join(".local/bin/findr-ocr");
            if candidate.exists() {
                return Some(candidate);
            }
        }

        eprintln!("Note: findr-ocr not found. Image OCR indexing disabled.");
        None
    }).clone()
}

#[derive(serde::Deserialize)]
struct OcrOutput {
    #[allow(dead_code)]
    path: String,
    text: Option<String>,
    confidence: Option<f64>,
    exif: Option<OcrExif>,
    error: Option<String>,
}

#[derive(serde::Deserialize)]
struct OcrExif {
    date_taken: Option<String>,
    gps: Option<String>,
}

/// Format EXIF metadata as searchable text to prepend to OCR content.
/// GPS coordinates are resolved to city/country via offline reverse geocoding.
fn format_exif(exif: &OcrExif) -> String {
    let mut parts = Vec::new();
    if let Some(ref date) = exif.date_taken {
        let date_short = date.split('T').next().unwrap_or(date);
        parts.push(format!("[Date: {}]", date_short));
    }
    if let Some(ref gps) = exif.gps {
        if let Some(location) = resolve_gps(gps) {
            parts.push(format!("[Location: {}]", location));
        } else {
            parts.push(format!("[Location: {}]", gps));
        }
    }
    parts.join(" ")
}

/// Resolve "lat,lon" string to "City, Region, Country" via offline geocoder.
fn resolve_gps(gps: &str) -> Option<String> {
    let mut coords = gps.split(',');
    let lat: f64 = coords.next()?.trim().parse().ok()?;
    let lon: f64 = coords.next()?.trim().parse().ok()?;

    let geocoder = GEOCODER.get_or_init(|| {
        reverse_geocoder::ReverseGeocoder::new()
    });

    let result = geocoder.search((lat, lon));
    let record = &result.record;

    let mut location = record.name.clone();
    if !record.admin1.is_empty() {
        location.push_str(", ");
        location.push_str(&record.admin1);
    }
    if !record.cc.is_empty() {
        location.push_str(", ");
        location.push_str(&record.cc);
    }
    Some(location)
}

/// Extract text from a single file via findr-ocr.
fn extract_ocr(path: &Path) -> Result<String> {
    let ocr_bin = find_ocr_binary()
        .ok_or_else(|| anyhow::anyhow!("findr-ocr binary not found"))?;

    let output = std::process::Command::new(&ocr_bin)
        .arg(path.as_os_str())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            crate::errors::log_error(
                &format!("ocr:{}", path.display()),
                &format!("Failed to run findr-ocr: {}", e),
            );
            return Err(e.into());
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Take the first JSON line (single file mode)
    let line = stdout.lines().next().unwrap_or("");
    parse_ocr_line(line, path)
}

/// Extract text from multiple files in one findr-ocr invocation.
/// Runs multiple findr-ocr processes concurrently via rayon for parallelism.
/// Returns (path, extracted_text, confidence) for each file.
pub fn extract_ocr_batch(paths: &[&Path]) -> Vec<(PathBuf, String, f64)> {
    let ocr_bin = match find_ocr_binary() {
        Some(b) => b,
        None => return Vec::new(),
    };

    // Split into chunks of 50, run in parallel (rayon uses num_cpus workers)
    let chunks: Vec<&[&Path]> = paths.chunks(50).collect();

    chunks.par_iter()
        .flat_map(|chunk| {
            let mut cmd = std::process::Command::new(&ocr_bin);
            for p in *chunk {
                cmd.arg(p.as_os_str());
            }

            let output = cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .output();

            let output = match output {
                Ok(o) => o,
                Err(e) => {
                    crate::errors::log_error("ocr:batch", &format!("Failed to run findr-ocr: {}", e));
                    return Vec::new();
                }
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines()
                .filter_map(|line| {
                    let ocr: OcrOutput = serde_json::from_str(line).ok()?;
                    if ocr.error.is_some() {
                        return None;
                    }
                    let confidence = ocr.confidence.unwrap_or(0.0);
                    if confidence < 0.3 {
                        return Some((PathBuf::from(&ocr.path), String::new(), confidence));
                    }
                    let mut text = String::new();
                    if let Some(ref exif) = ocr.exif {
                        let meta = format_exif(exif);
                        if !meta.is_empty() {
                            text.push_str(&meta);
                            text.push(' ');
                        }
                    }
                    if let Some(ref t) = ocr.text {
                        text.push_str(t);
                    }
                    Some((PathBuf::from(&ocr.path), text, confidence))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Parse a single JSON line from findr-ocr output into extracted text.
fn parse_ocr_line(line: &str, path: &Path) -> Result<String> {
    let ocr: OcrOutput = serde_json::from_str(line).map_err(|e| {
        crate::errors::log_error(
            &format!("ocr:{}", path.display()),
            &format!("Invalid JSON from findr-ocr: {}", e),
        );
        anyhow::anyhow!("Invalid OCR output")
    })?;

    if let Some(err) = ocr.error {
        crate::errors::log_error(&format!("ocr:{}", path.display()), &err);
        return Err(anyhow::anyhow!("{}", err));
    }

    let confidence = ocr.confidence.unwrap_or(0.0);
    if confidence < 0.3 {
        return Ok(String::new());
    }

    let mut text = String::new();
    if let Some(ref exif) = ocr.exif {
        let meta = format_exif(exif);
        if !meta.is_empty() {
            text.push_str(&meta);
            text.push(' ');
        }
    }
    if let Some(ref t) = ocr.text {
        text.push_str(t);
    }

    Ok(text)
}
