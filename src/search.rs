use anyhow::Result;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::Utf32Str;
use nucleo::Matcher;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::content::ContentIndex;
use crate::db::Database;

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub filename: String,
    pub score: f64,
    pub match_type: String,
    pub size_bytes: Option<u64>,
    pub modified: String,
    pub file_type: Option<String>,
    pub content_snippet: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub mode: String,
    pub elapsed_ms: u128,
    pub total_results: usize,
    pub results: Vec<SearchResult>,
}

// Tier score bases — exact/substring filename matches outrank content,
// but fuzzy-only filename matches rank BELOW content exact matches.
const TIER_FILENAME_PREFIX: f64 = 10000.0;  // filename starts with query
const TIER_FILENAME_CONTAINS: f64 = 5000.0; // filename contains query as substring
const TIER_CONTENT: f64 = 2000.0;           // content match (exact word match via Tantivy)
const TIER_FILENAME_FUZZY: f64 = 1000.0;    // filename fuzzy-only match (no exact substring)
const BOTH_MATCH_BOOST: f64 = 500.0;        // bonus when file matches both filename and content

/// Parse query for inline type filter.
fn parse_query(query: &str) -> (String, Option<String>) {
    let known_extensions = [
        "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
        "txt", "md", "csv", "json", "yml", "yaml", "xml",
        "png", "jpg", "jpeg", "gif", "svg", "webp", "ico",
        "mp3", "mp4", "mov", "avi", "wav",
        "zip", "tar", "gz", "rar", "7z",
        "rs", "ts", "js", "py", "go", "rb", "java", "c", "cpp", "h",
        "html", "css", "scss", "less",
        "sh", "zsh", "bash",
        "toml", "ini", "cfg", "conf", "env",
        "log", "sql",
    ];

    let parts: Vec<&str> = query.trim().split_whitespace().collect();
    if parts.len() >= 2 {
        let last = parts.last().unwrap().trim_start_matches('.');
        if known_extensions.contains(&last.to_lowercase().as_str()) {
            let search_part = parts[..parts.len() - 1].join(" ");
            return (search_part, Some(last.to_lowercase()));
        }
    }
    (query.to_string(), None)
}

fn recency_bonus(now_ts: i64, modified_ts: i64) -> f64 {
    let age_days = (now_ts - modified_ts).max(0) as f64 / 86400.0;
    100.0 / (1.0 + age_days.sqrt())
}

fn classify_filename_match(filename: &str, query: &str) -> Option<f64> {
    let fname_lower = filename.to_lowercase();
    let query_lower = query.to_lowercase();

    if fname_lower.starts_with(&query_lower) {
        Some(TIER_FILENAME_PREFIX)
    } else if fname_lower.contains(&query_lower) {
        Some(TIER_FILENAME_CONTAINS)
    } else {
        None // will check fuzzy separately
    }
}

/// Unified search: runs both filename (Nucleo) and content (Tantivy) searches,
/// merges results into tiered ranking.
pub fn unified_search(
    db: &Database,
    content_index_path: &PathBuf,
    query: &str,
    limit: usize,
) -> Result<SearchResponse> {
    let start = std::time::Instant::now();
    let (search_query, type_filter) = parse_query(query);

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Collect all candidates: path -> (score, filename, extension, modified, size, snippet)
    let mut candidates: HashMap<String, (f64, String, Option<String>, i64, u64, Option<String>)> = HashMap::new();

    // === Pass 1: Filename search via Nucleo ===
    let all_files = db.get_all_paths_with_size()?;

    let pattern = Pattern::parse(
        &search_query,
        CaseMatching::Ignore,
        Normalization::Smart,
    );
    let min_score: u32 = (search_query.len() as u32) * 12;
    let mut buf = Vec::new();
    let mut matcher = Matcher::default();

    for (path, filename, extension, modified_ts, size_bytes) in &all_files {
        if let Some(ref filter) = type_filter {
            match extension {
                Some(ext) if ext == filter => {}
                _ => continue,
            }
        }

        let filename_haystack = Utf32Str::new(filename, &mut buf);
        let nucleo_score = pattern.score(filename_haystack, &mut matcher);
        buf.clear();

        let nucleo_score = match nucleo_score {
            Some(s) if s >= min_score => s,
            _ => continue,
        };

        // Classify into tier based on match quality
        let tier_base = classify_filename_match(filename, &search_query)
            .unwrap_or(TIER_FILENAME_FUZZY);

        // Within-tier ranking: nucleo score (normalized) + recency
        let within_tier = (nucleo_score as f64 / 100.0) + recency_bonus(now_ts, *modified_ts);
        let total_score = tier_base + within_tier;

        candidates.insert(
            path.clone(),
            (total_score, filename.clone(), extension.clone(), *modified_ts, *size_bytes, None),
        );
    }

    // Build path -> (modified_ts, size) lookup for content results
    let file_meta: HashMap<&str, (i64, u64)> = all_files
        .iter()
        .map(|(path, _, _, ts, size)| (path.as_str(), (*ts, *size)))
        .collect();

    // === Pass 2: Content search via Tantivy ===
    if let Ok(cidx) = ContentIndex::open_or_create(content_index_path) {
        if let Ok(content_results) = cidx.search(&search_query, limit * 2, type_filter.as_deref()) {
            for cr in content_results.into_iter() {
                // Look up real mtime from SQLite data
                let (mtime, size) = file_meta.get(cr.path.as_str()).copied().unwrap_or((0, 0));

                // Position bonus: match at start of doc (0.0) gets full bonus,
                // match at end (1.0) gets none
                let position_bonus = 100.0 * (1.0 - cr.match_position);

                let content_score = TIER_CONTENT
                    + position_bonus
                    + recency_bonus(now_ts, mtime);

                if let Some(existing) = candidates.get_mut(&cr.path) {
                    // File already found by filename search — boost and add snippet
                    existing.0 += BOTH_MATCH_BOOST + position_bonus;
                    existing.5 = cr.snippet;
                } else {
                    // Content-only match
                    candidates.insert(
                        cr.path.clone(),
                        (content_score, cr.filename, Some(cr.extension), mtime, size, cr.snippet),
                    );
                }
            }
        }
    }

    // === Sort and truncate ===
    let mut sorted: Vec<_> = candidates.into_iter().collect();
    sorted.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap_or(std::cmp::Ordering::Equal));
    sorted.truncate(limit);

    let results: Vec<SearchResult> = sorted
        .into_iter()
        .map(|(path, (score, filename, extension, modified_ts, size, snippet))| {
            let modified = if modified_ts > 0 {
                chrono::DateTime::from_timestamp(modified_ts, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            } else {
                String::new()
            };

            SearchResult {
                path,
                filename,
                score: (score * 100.0).round() / 100.0,
                match_type: "unified".to_string(),
                size_bytes: if size > 0 { Some(size) } else { None },
                modified,
                file_type: extension,
                content_snippet: snippet,
            }
        })
        .collect();

    let total = results.len();
    Ok(SearchResponse {
        query: query.to_string(),
        mode: "unified".to_string(),
        elapsed_ms: start.elapsed().as_millis(),
        total_results: total,
        results,
    })
}
