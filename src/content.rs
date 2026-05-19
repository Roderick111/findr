use anyhow::Result;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, QueryParser};
use tantivy::schema::*;
use tantivy::directory::MmapDirectory;
use tantivy::{doc, Index, IndexWriter, ReloadPolicy, Term};

const CONTENT_EXTRACTABLE: &[&str] = &[
    "pdf", "txt", "md", "csv", "json", "yml", "yaml", "xml",
    "rs", "ts", "js", "py", "go", "rb", "java", "c", "cpp", "h",
    "html", "css", "toml", "ini", "cfg", "conf", "sh", "zsh",
    "log", "sql", "tsx", "jsx",
];

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
    pub fn index_new_files(&self, files: &[(String, String, Option<String>)]) -> Result<usize> {
        let mut writer: IndexWriter = self.index.writer(50_000_000)?;
        let mut count = 0;

        for (path, filename, extension) in files {
            let ext = extension.as_deref().unwrap_or("");
            if !CONTENT_EXTRACTABLE.contains(&ext) {
                continue;
            }

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

    /// Full reindex — deletes all, then indexes everything.
    pub fn index_files(&self, files: &[(String, String, Option<String>)]) -> Result<usize> {
        let mut writer: IndexWriter = self.index.writer(50_000_000)?; // 50MB heap
        writer.delete_all_documents()?;
        writer.commit()?;

        let mut count = 0;
        let mut batch = 0;

        for (path, filename, extension) in files {
            let ext = extension.as_deref().unwrap_or("");
            if !CONTENT_EXTRACTABLE.contains(&ext) {
                continue;
            }

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

        // Try exact match first
        let query_parser = QueryParser::for_index(&self.index, vec![self.content_field, self.filename_field]);
        let query = query_parser.parse_query(query_str)?;
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

fn extract_content(path: &Path, ext: &str) -> Result<String> {
    match ext {
        "pdf" => extract_pdf(path),
        _ => {
            // Text-based files: read directly, cap at 100KB
            let content = std::fs::read_to_string(path)?;
            Ok(content.chars().take(100_000).collect())
        }
    }
}

fn extract_pdf(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let result = std::panic::catch_unwind(|| {
        pdf_extract::extract_text_from_mem(&bytes)
    });
    match result {
        Ok(Ok(text)) => Ok(text.chars().take(200_000).collect()),
        Ok(Err(e)) => {
            let msg = format!("PDF extraction error: {}", e);
            crate::errors::log_error(&format!("pdf:{}", path.display()), &msg);
            Err(anyhow::anyhow!("{}", msg))
        }
        Err(_) => {
            let msg = "PDF extraction panicked (malformed PDF)";
            crate::errors::log_error(&format!("pdf:{}", path.display()), msg);
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

        let start = content[..pos].rfind('\n').map(|p| p + 1).unwrap_or(
            pos.saturating_sub(80)
        );
        let end = content[pos..].find('\n').map(|p| pos + p).unwrap_or(
            (pos + 160).min(content.len())
        );
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
