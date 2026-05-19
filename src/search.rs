use anyhow::Result;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::Utf32Str;
use nucleo::Matcher;
use serde::Serialize;

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

/// Parse query for inline type filter.
/// "revolut pdf" -> ("revolut", Some("pdf"))
/// "revolut .pdf" -> ("revolut", Some("pdf"))
/// "revolut" -> ("revolut", None)
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

pub fn fuzzy_search(
    db: &Database,
    query: &str,
    limit: usize,
) -> Result<SearchResponse> {
    let start = std::time::Instant::now();
    let (search_query, type_filter) = parse_query(query);

    let all_files = db.get_all_paths_with_size()?;

    let pattern = Pattern::parse(
        &search_query,
        CaseMatching::Ignore,
        Normalization::Smart,
    );

    // Minimum score threshold to prevent garbage subsequence matches
    let min_score: u32 = (search_query.len() as u32) * 12;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let mut scored: Vec<(f64, &str, &str, &Option<String>, i64, u64)> = Vec::new();
    let mut buf = Vec::new();
    let mut matcher = Matcher::default();

    for (path, filename, extension, modified_ts, size_bytes) in &all_files {
        // Type filter
        if let Some(ref filter) = type_filter {
            match extension {
                Some(ext) if ext == filter => {}
                _ => continue,
            }
        }

        // Score against filename (primary) — this is what users expect
        let filename_haystack = Utf32Str::new(filename, &mut buf);
        let filename_score = pattern.score(filename_haystack, &mut matcher);

        // Only match on filename. Path matching creates too much noise
        // (e.g., "revolut" matching "evaluation/validators/report-formatter.ts")
        let base_score = match filename_score {
            Some(fs) if fs >= min_score => fs as f64 * 2.0,
            _ => continue,
        };

        // Recency bonus: files modified recently score higher
        let age_days = (now_ts - modified_ts).max(0) as f64 / 86400.0;
        let recency_bonus = 100.0 / (1.0 + age_days.sqrt());

        let final_score = base_score + recency_bonus;

        scored.push((final_score, path, filename, extension, *modified_ts, *size_bytes));
        buf.clear();
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    let results: Vec<SearchResult> = scored
        .into_iter()
        .map(|(score, path, filename, extension, modified_ts, size)| {
            let modified = chrono::DateTime::from_timestamp(modified_ts, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();

            SearchResult {
                path: path.to_string(),
                filename: filename.to_string(),
                score: (score * 100.0).round() / 100.0,
                match_type: "filename".to_string(),
                size_bytes: Some(size),
                modified,
                file_type: extension.clone(),
                content_snippet: None,
            }
        })
        .collect();

    let total = results.len();
    Ok(SearchResponse {
        query: query.to_string(),
        mode: "filename".to_string(),
        elapsed_ms: start.elapsed().as_millis(),
        total_results: total,
        results,
    })
}
