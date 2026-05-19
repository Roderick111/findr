use anyhow::Result;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::*;
use tantivy::directory::MmapDirectory;
use tantivy::{doc, Index, IndexWriter, ReloadPolicy};

const CONTENT_EXTRACTABLE: &[&str] = &[
    "pdf", "txt", "md", "csv", "json", "yml", "yaml", "xml",
    "rs", "ts", "js", "py", "go", "rb", "java", "c", "cpp", "h",
    "html", "css", "toml", "ini", "cfg", "conf", "sh", "zsh",
    "log", "sql", "tsx", "jsx",
];

pub struct ContentIndex {
    index: Index,
    schema: Schema,
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
    pub fn open_or_create(index_dir: &PathBuf) -> Result<Self> {
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
            schema,
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

        let query_parser = QueryParser::for_index(&self.index, vec![self.content_field, self.filename_field]);
        let query = query_parser.parse_query(query_str)?;

        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit * 2))?;

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
    // pdf-extract panics on some PDFs (e.g., DeviceN color spaces)
    let result = std::panic::catch_unwind(|| {
        pdf_extract::extract_text_from_mem(&bytes)
    });
    match result {
        Ok(Ok(text)) => Ok(text.chars().take(200_000).collect()),
        Ok(Err(e)) => Err(anyhow::anyhow!("PDF extraction error: {}", e)),
        Err(_) => Err(anyhow::anyhow!("PDF extraction panicked")),
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
            format!("...{}...", &snippet[..200])
        } else {
            snippet
        };
        (Some(snippet), match_position)
    } else {
        // No exact match found (Tantivy tokenizer may have stemmed/split)
        let snippet = content.lines().next().map(|l| {
            if l.len() > 200 { format!("{}...", &l[..200]) } else { l.to_string() }
        });
        (snippet, 0.5) // neutral position
    }
}
