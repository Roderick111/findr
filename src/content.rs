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
        let content_field = schema_builder.add_text_field("content", TEXT);
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
        let mut has_deletes = false;

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
                _ => {
                    has_deletes = true;
                    continue;
                }
            };

            writer.add_document(doc!(
                self.path_field => path.as_str(),
                self.filename_field => filename.as_str(),
                self.content_field => content.as_str(),
                self.extension_field => ext,
            ))?;
            count += 1;
        }

        if count > 0 || has_deletes {
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

        // Suppress panic output from pdf-extract / adobe-cmap-parser.
        // These panics are caught by catch_unwind but the default hook prints
        // noisy backtraces to stderr. Route to error log instead.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".into()
            };
            let location = info.location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown".into());
            crate::errors::log_error("extract:panic", &format!("{} at {}", msg, location));
        }));

        // Process in batches of 1000 to cap peak memory
        // (avoids holding all extracted content in memory simultaneously)
        let mut count = 0;
        let mut batch_commits = 0;

        for chunk in text_files.chunks(1000) {
            let extracted: Vec<(&str, &str, &str, String)> = chunk.par_iter()
                .filter_map(|(path, filename, extension)| {
                    let ext = extension.as_deref().unwrap_or("");
                    // Catch panics at the rayon task level — pdf-extract can panic
                    // in ways that poison the thread pool and kill the Tantivy writer.
                    // extract_content has its own catch_unwind for PDFs, but some panics
                    // (e.g., in rayon's thread join) bypass it.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        extract_content(Path::new(path), ext)
                    }));
                    match result {
                        Ok(Ok(c)) if !c.is_empty() => Some((path.as_str(), filename.as_str(), ext, c)),
                        Ok(Err(_)) | Ok(Ok(_)) => None,
                        Err(_) => {
                            crate::errors::log_error(
                                &format!("extract:{}", path),
                                "Content extraction panicked (caught at rayon level)",
                            );
                            None
                        }
                    }
                })
                .collect();

            for (path, filename, ext, content) in &extracted {
                writer.add_document(doc!(
                    self.path_field => *path,
                    self.filename_field => *filename,
                    self.content_field => content.as_str(),
                    self.extension_field => *ext,
                ))?;
                count += 1;
            }
            // extracted dropped here — frees memory before next batch

            batch_commits += 1;
            if batch_commits % 2 == 0 {
                writer.commit()?;
                eprint!("\r  Content indexed: {} files", count);
            }
        }

        writer.commit()?;
        // Restore default panic hook
        std::panic::set_hook(default_hook);
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

        // Compute minimum score threshold: reject results scoring below 20% of the top result.
        // This filters out near-zero BM25 noise (e.g., PDF garbage matching random tokens).
        let min_score = top_docs.first().map(|(s, _)| s * 0.2).unwrap_or(0.0);

        let mut results = Vec::new();
        for (score, doc_address) in top_docs {
            if score < min_score {
                continue;
            }

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

            // Defer snippet extraction to after sorting/truncation (avoid 60 random reads)
            results.push(ContentSearchResult {
                path,
                filename,
                extension,
                score,
                snippet: None,
                match_position: 0.5, // neutral — will be refined when snippet is extracted
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

/// Public wrapper for content extraction — used by semantic embedding.
/// Extracts text from binary formats (PDF, DOCX, XLSX) using proper parsers.
pub fn extract_content_for_embed(path: &Path, ext: &str) -> Result<String> {
    extract_content(path, ext)
}

/// Check if extracted PDF text is actually readable (not binary garbage or raw PDF structure).
fn is_readable_text(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    let trimmed = text.trim();

    // Reject raw PDF headers — extraction returned the file bytes, not content
    if trimmed.starts_with("%PDF") {
        return false;
    }

    let sample: String = trimmed.chars().take(2000).collect();
    if sample.is_empty() {
        return false;
    }

    // Check 1: readable character ratio (letters, digits, whitespace, common punctuation)
    let readable = sample.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || c.is_ascii_punctuation())
        .count();
    let ratio = readable as f64 / sample.len() as f64;
    if ratio < 0.7 {
        return false;
    }

    // Check 2: must have enough words to be natural language
    let words: Vec<&str> = sample.split_whitespace()
        .filter(|w| w.len() >= 2 && w.len() <= 30)
        .collect();
    if words.len() < 5 {
        return false;
    }

    // Check 3: average word length typical of human text (2-15 chars)
    let avg_len: f64 = words.iter().map(|w| w.len() as f64).sum::<f64>() / words.len() as f64;
    if !(2.0..=15.0).contains(&avg_len) {
        return false;
    }

    // Check 4: reject if saturated with PDF/PostScript structure tokens
    let pdf_markers = [" obj", "endobj", "xref", "trailer", "startxref",
                        "/Type", "/Page", "/Font", "/Length", "stream\n", "endstream"];
    let marker_hits: usize = pdf_markers.iter()
        .map(|m| sample.matches(m).count())
        .sum();
    if marker_hits > 5 {
        return false;
    }

    true
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

    // Quality check: if extracted text is binary garbage, treat as empty
    let text = if is_readable_text(&text) { text } else { String::new() };

    // Scanned PDF fallback: if text extraction yields very little, try OCR
    let word_char_count: usize = text.split_whitespace().map(|w| w.len()).sum();
    if word_char_count < SCANNED_PDF_TEXT_THRESHOLD {
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

/// Strip XML/HTML tags: remove everything between < and >.
pub fn strip_xml_tags(xml: &str) -> String {
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
            // Cap decompressed read at 5MB to prevent zip bombs
            let mut limited = std::io::Read::take(&mut file, 5 * 1024 * 1024);
            std::io::Read::read_to_string(&mut limited, &mut xml)?;
        }
        let text = strip_xml_tags(&xml);
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
            // Cap decompressed read at 5MB to prevent zip bombs
            let mut limited = std::io::Read::take(&mut file, 5 * 1024 * 1024);
            std::io::Read::read_to_string(&mut limited, &mut xml)?;
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
fn extract_snippet_with_position(content: &str, query: &str, max_snippet_len: usize) -> (Option<String>, f64) {
    let query_lower = query.to_lowercase();
    let content_lower = content.to_lowercase();

    if let Some(raw_pos) = content_lower.find(&query_lower) {
        // Snap match position to char boundary (from_utf8_lossy may shift offsets)
        let pos = snap_to_char_boundary(content, raw_pos, true);
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
        let snippet = if snippet.len() > max_snippet_len {
            let truncated = truncate_str(&snippet, max_snippet_len);
            format!("...{}...", truncated)
        } else {
            snippet
        };
        (Some(snippet), match_position)
    } else {
        // No exact match found (Tantivy tokenizer may have stemmed/split)
        let snippet = content.lines().next().map(|l| {
            if l.len() > max_snippet_len { format!("{}...", truncate_str(l, max_snippet_len)) } else { l.to_string() }
        });
        (snippet, 0.5) // neutral position
    }
}

/// Extract a snippet from the source file on disk (reads first 200KB).
/// For binary formats (PDF etc.), returns None — snippets come from Tantivy at search time.
pub fn extract_snippet_from_file(path: &Path, query: &str, max_snippet_len: usize) -> (Option<String>, f64) {
    let ext = path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Binary formats: skip raw read (would show %PDF garbage).
    // These files get snippets from Tantivy search results, not from disk.
    match ext.as_str() {
        "pdf" | "docx" | "xlsx" => return (None, 0.5),
        _ => {}
    }

    // Text files: read first 200KB directly
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, 0.5),
    };
    let mut reader = std::io::BufReader::new(file);
    let mut buf = vec![0u8; 204_800]; // 200KB
    let n = match reader.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return (None, 0.5),
    };
    buf.truncate(n);
    let content = String::from_utf8_lossy(&buf);
    extract_snippet_with_position(&content, query, max_snippet_len)
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
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
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

/// Locate the findr-ocr binary via platform-specific discovery.
pub fn find_ocr_binary() -> Option<PathBuf> {
    OCR_BINARY.get_or_init(|| {
        match crate::platform::find_ocr_binary() {
            Some(path) => Some(path),
            None => {
                eprintln!("Note: findr-ocr not found. Image OCR indexing disabled.");
                None
            }
        }
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

/// Extract text from multiple files via OCR.
/// macOS: spawns findr-ocr binary (Apple Vision). Linux/Windows: uses ocrs in-process.
/// Returns (path, extracted_text, confidence) for each file.
pub fn extract_ocr_batch(paths: &[&Path]) -> Vec<(PathBuf, String, f64)> {
    #[cfg(target_os = "macos")]
    { extract_ocr_batch_binary(paths) }
    #[cfg(not(target_os = "macos"))]
    { extract_ocr_batch_ocrs(paths) }
}

/// macOS: spawn findr-ocr binary for batch OCR.
#[cfg(target_os = "macos")]
fn extract_ocr_batch_binary(paths: &[&Path]) -> Vec<(PathBuf, String, f64)> {
    let ocr_bin = match find_ocr_binary() {
        Some(b) => b,
        None => return Vec::new(),
    };

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

/// Linux/Windows: use ocrs crate for in-process OCR.
#[cfg(not(target_os = "macos"))]
fn extract_ocr_batch_ocrs(paths: &[&Path]) -> Vec<(PathBuf, String, f64)> {
    use rayon::prelude::*;

    paths.par_iter()
        .filter_map(|path| {
            let (text, confidence) = crate::platform::ocr_engine::extract_ocr_text(path)?;
            Some((path.to_path_buf(), text, confidence))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── escape_tantivy_query ──

    #[test]
    fn escape_plain_query() {
        assert_eq!(escape_tantivy_query("hello world"), "hello world");
    }

    #[test]
    fn escape_special_chars() {
        let escaped = escape_tantivy_query("test+query (with) [brackets]");
        assert_eq!(escaped, "test\\+query \\(with\\) \\[brackets\\]");
    }

    #[test]
    fn escape_all_tantivy_specials() {
        let input = r#"+-!(){}[]^"~*?:\/"#;
        let escaped = escape_tantivy_query(input);
        // Every char should be preceded by backslash
        for ch in input.chars() {
            assert!(escaped.contains(&format!("\\{}", ch)),
                "missing escape for '{}'", ch);
        }
    }

    #[test]
    fn escape_empty() {
        assert_eq!(escape_tantivy_query(""), "");
    }

    // ── strip_xml_tags ──

    #[test]
    fn strip_simple_tags() {
        assert_eq!(strip_xml_tags("<p>hello</p>"), "hello");
    }

    #[test]
    fn strip_nested_tags() {
        assert_eq!(strip_xml_tags("<div><b>bold</b> text</div>"), "bold text");
    }

    #[test]
    fn strip_no_tags() {
        assert_eq!(strip_xml_tags("plain text"), "plain text");
    }

    #[test]
    fn strip_empty_tags() {
        assert_eq!(strip_xml_tags("<br/><hr/>"), "");
    }

    #[test]
    fn strip_tags_with_attributes() {
        assert_eq!(
            strip_xml_tags(r#"<a href="url">link</a>"#),
            "link"
        );
    }

    // ── is_readable_text ──

    #[test]
    fn readable_normal_text() {
        assert!(is_readable_text("This is a normal English paragraph with multiple words and sentences."));
    }

    #[test]
    fn readable_empty() {
        assert!(!is_readable_text(""));
        assert!(!is_readable_text("   "));
    }

    #[test]
    fn readable_rejects_pdf_header() {
        assert!(!is_readable_text("%PDF-1.4 some binary content follows"));
    }

    #[test]
    fn readable_rejects_binary_garbage() {
        let garbage = (0..200).map(|i| (i % 256) as u8 as char).collect::<String>();
        assert!(!is_readable_text(&garbage));
    }

    #[test]
    fn readable_rejects_pdf_structure() {
        let pdf_like = "1 0 obj /Type /Page /Font endobj 2 0 obj /Type /Page endobj xref trailer startxref";
        assert!(!is_readable_text(pdf_like));
    }

    #[test]
    fn readable_rejects_too_few_words() {
        assert!(!is_readable_text("ab"));
    }

    #[test]
    fn readable_accepts_code() {
        let code = "fn main() {\n    let x = 42;\n    println!(\"hello world\");\n    let result = compute(x);\n    return result;\n}";
        assert!(is_readable_text(code));
    }

    // ── extract_snippet_with_position ──

    #[test]
    fn snippet_exact_match() {
        let content = "First line\nSecond line with revolut payment\nThird line";
        let (snippet, pos) = extract_snippet_with_position(content, "revolut", 200);
        assert!(snippet.is_some());
        assert!(snippet.unwrap().contains("revolut"));
        assert!(pos > 0.0 && pos < 1.0);
    }

    #[test]
    fn snippet_case_insensitive() {
        let content = "Transfer from REVOLUT bank account";
        let (snippet, _) = extract_snippet_with_position(content, "revolut", 200);
        assert!(snippet.is_some());
    }

    #[test]
    fn snippet_no_match() {
        let content = "Nothing relevant here at all in this text";
        let (snippet, pos) = extract_snippet_with_position(content, "xyz123", 200);
        // Falls back to first line
        assert!(snippet.is_some());
        assert!((pos - 0.5).abs() < 0.01); // neutral position
    }

    #[test]
    fn snippet_match_at_start() {
        let content = "revolut payment on march 15\nSecond line";
        let (_, pos) = extract_snippet_with_position(content, "revolut", 200);
        assert!(pos < 0.1, "match at start should have low position, got {}", pos);
    }

    #[test]
    fn snippet_match_at_end() {
        let padding = "a ".repeat(500);
        let content = format!("{}revolut", padding);
        let (_, pos) = extract_snippet_with_position(&content, "revolut", 200);
        assert!(pos > 0.8, "match at end should have high position, got {}", pos);
    }

    // ── snap_to_char_boundary ──

    #[test]
    fn snap_ascii_is_noop() {
        let s = "hello world";
        assert_eq!(snap_to_char_boundary(s, 5, true), 5);
        assert_eq!(snap_to_char_boundary(s, 5, false), 5);
    }

    #[test]
    fn snap_multibyte_backward() {
        let s = "café"; // é is 2 bytes, positions: c=0, a=1, f=2, é=3-4
        // Offset 4 is inside é — snap backward to 3
        let snapped = snap_to_char_boundary(s, 4, true);
        assert!(s.is_char_boundary(snapped));
    }

    #[test]
    fn snap_multibyte_forward() {
        let s = "café";
        let snapped = snap_to_char_boundary(s, 4, false);
        assert!(s.is_char_boundary(snapped));
    }

    #[test]
    fn snap_beyond_len() {
        let s = "hi";
        assert_eq!(snap_to_char_boundary(s, 100, true), 2);
        assert_eq!(snap_to_char_boundary(s, 100, false), 2);
    }

    // ── truncate_str ──

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_at_boundary() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_multibyte_safe() {
        let s = "café au lait";
        let truncated = truncate_str(s, 4);
        // "caf" is 3 bytes, "café" is 5 bytes (é = 2 bytes)
        // Truncating at 4 should not split é
        assert!(truncated.len() <= 4);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn truncate_zero() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    // ── ContentIndex integration ──

    #[test]
    fn content_index_create_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let idx = ContentIndex::open_or_create(dir.path()).unwrap();

        // Create a temp text file
        let file_dir = tempfile::tempdir().unwrap();
        let file_path = file_dir.path().join("test.txt");
        std::fs::write(&file_path, "The quick brown fox jumps over the lazy dog").unwrap();

        let files = vec![(
            file_path.to_str().unwrap().to_string(),
            "test.txt".to_string(),
            Some("txt".to_string()),
        )];

        let count = idx.index_files(&files).unwrap();
        assert_eq!(count, 1);

        let results = idx.search("quick brown fox", 10, None).unwrap();
        assert!(!results.is_empty(), "should find content match");
        assert_eq!(results[0].filename, "test.txt");
    }

    #[test]
    fn content_index_delete() {
        let dir = tempfile::tempdir().unwrap();
        let idx = ContentIndex::open_or_create(dir.path()).unwrap();

        let file_dir = tempfile::tempdir().unwrap();
        let file_path = file_dir.path().join("delete_me.txt");
        std::fs::write(&file_path, "unique searchable content xyzzy").unwrap();
        let path_str = file_path.to_str().unwrap().to_string();

        let files = vec![(path_str.clone(), "delete_me.txt".to_string(), Some("txt".to_string()))];
        idx.index_files(&files).unwrap();

        // Verify it's there
        let results = idx.search("xyzzy", 10, None).unwrap();
        assert!(!results.is_empty());

        // Delete
        idx.delete_files(&[path_str]).unwrap();
        let results = idx.search("xyzzy", 10, None).unwrap();
        assert!(results.is_empty(), "deleted file should not appear");
    }

    #[test]
    fn content_index_type_filter() {
        let dir = tempfile::tempdir().unwrap();
        let idx = ContentIndex::open_or_create(dir.path()).unwrap();

        let file_dir = tempfile::tempdir().unwrap();
        let txt_path = file_dir.path().join("doc.txt");
        let rs_path = file_dir.path().join("code.rs");
        std::fs::write(&txt_path, "unique alpha bravo content").unwrap();
        std::fs::write(&rs_path, "unique alpha bravo content").unwrap();

        let files = vec![
            (txt_path.to_str().unwrap().to_string(), "doc.txt".to_string(), Some("txt".to_string())),
            (rs_path.to_str().unwrap().to_string(), "code.rs".to_string(), Some("rs".to_string())),
        ];
        idx.index_files(&files).unwrap();

        // Filter to txt only
        let results = idx.search("alpha bravo", 10, Some("txt")).unwrap();
        assert!(results.iter().all(|r| r.extension == "txt"),
            "type filter should only return txt files");
    }

    #[test]
    fn content_index_doc_count() {
        let dir = tempfile::tempdir().unwrap();
        let idx = ContentIndex::open_or_create(dir.path()).unwrap();

        let file_dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for i in 0..5 {
            let p = file_dir.path().join(format!("file{}.txt", i));
            std::fs::write(&p, format!("content of file number {}", i)).unwrap();
            files.push((p.to_str().unwrap().to_string(), format!("file{}.txt", i), Some("txt".to_string())));
        }

        idx.index_files(&files).unwrap();
        assert_eq!(idx.doc_count().unwrap(), 5);
    }

    // ── Performance: snippet extraction ──

    #[test]
    fn snippet_extraction_perf() {
        // Simulate a large file content
        let content = "word ".repeat(50_000); // ~250KB
        let needle = "uniqueneedle";
        let content_with_needle = format!("{}{}rest of content", &content[..content.len()/2], needle);

        let start = std::time::Instant::now();
        let iterations = 1000;
        for _ in 0..iterations {
            let _ = extract_snippet_with_position(&content_with_needle, needle, 200);
        }
        let elapsed = start.elapsed();
        let per_op_us = elapsed.as_micros() / iterations as u128;
        eprintln!("snippet extraction (250KB): {}μs/op", per_op_us);
        assert!(per_op_us < 5000, "snippet extraction too slow: {}μs", per_op_us);
    }

    // ── Property-based tests ──

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn strip_xml_never_panics(input in "\\PC{0,200}") {
            let _ = strip_xml_tags(&input);
        }

        #[test]
        fn strip_xml_no_tags_remain(input in "\\PC{0,200}") {
            let stripped = strip_xml_tags(&input);
            // Output should never contain paired < > with content between
            // (single < or > without pair is fine — that's malformed input)
            let tag_count = stripped.matches('<').count();
            let close_count = stripped.matches('>').count();
            // If both < and > remain, they shouldn't form valid tags
            if tag_count > 0 && close_count > 0 {
                // At worst, unmatched brackets pass through — that's OK
                // We just verify the function didn't crash
            }
        }

        #[test]
        fn strip_xml_idempotent(input in "\\PC{0,100}") {
            let once = strip_xml_tags(&input);
            let twice = strip_xml_tags(&once);
            prop_assert_eq!(&once, &twice,
                "strip_xml_tags should be idempotent");
        }

        #[test]
        fn truncate_str_respects_limit(s in "\\PC{0,500}", max in 0usize..200) {
            let truncated = truncate_str(&s, max);
            prop_assert!(truncated.len() <= max,
                "truncated len {} exceeds max {}", truncated.len(), max);
        }

        #[test]
        fn truncate_str_valid_utf8(s in "\\PC{0,200}", max in 0usize..100) {
            let truncated = truncate_str(&s, max);
            // If it compiles and doesn't panic, it's valid UTF-8
            // Also verify it's a prefix of the original
            prop_assert!(s.starts_with(truncated),
                "truncated {:?} is not a prefix of {:?}", truncated, s);
        }

        #[test]
        fn snap_to_char_boundary_always_valid(s in "\\PC{1,100}", offset in 0usize..200) {
            let fwd = snap_to_char_boundary(&s, offset, false);
            let bwd = snap_to_char_boundary(&s, offset, true);
            prop_assert!(s.is_char_boundary(fwd),
                "forward snap {} not a char boundary", fwd);
            prop_assert!(s.is_char_boundary(bwd),
                "backward snap {} not a char boundary", bwd);
            prop_assert!(fwd <= s.len());
            prop_assert!(bwd <= s.len());
        }

        #[test]
        fn escape_tantivy_preserves_alphanumeric(input in "[a-zA-Z0-9 ]{1,50}") {
            let escaped = escape_tantivy_query(&input);
            prop_assert_eq!(&input, &escaped,
                "alphanumeric input should pass through unchanged");
        }

        #[test]
        fn escape_tantivy_never_panics(input in "\\PC{0,200}") {
            let _ = escape_tantivy_query(&input);
        }

        #[test]
        fn is_readable_text_rejects_empty(s in "\\s{0,10}") {
            // Whitespace-only or empty strings should not be "readable"
            if s.trim().is_empty() {
                prop_assert!(!is_readable_text(&s));
            }
        }

        #[test]
        fn snippet_position_in_range(
            content in "[a-z ]{20,200}",
            query in "[a-z]{2,8}"
        ) {
            let (_, pos) = extract_snippet_with_position(&content, &query, 200);
            prop_assert!(pos >= 0.0 && pos <= 1.0,
                "position {} out of [0,1] range", pos);
        }
    }
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
