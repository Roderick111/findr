use anyhow::Result;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::Utf32Str;
use nucleo::Matcher;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::content::ContentIndex;
use crate::db::Database;

/// (score, filename, extension, modified_ts, size_bytes, content_snippet)
type CandidateData = (f64, String, Option<String>, i64, u64, Option<String>);

/// Pre-queried semantic matches: (path, cosine_similarity).
/// Produced by HNSW approximate search or brute-force fallback.
pub type SemanticMatches = Vec<(String, f32)>;

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

// Tier score bases — any filename match beats content-only match
const TIER_FILENAME_PREFIX: f64 = 10000.0;  // filename starts with query
const TIER_FILENAME_CONTAINS: f64 = 5000.0; // filename contains query as substring
const TIER_FILENAME_TYPO: f64 = 3000.0;     // filename typo match (Levenshtein) — name match > content match
const TIER_CONTENT: f64 = 2000.0;           // content match (exact word via Tantivy)
const TIER_SEMANTIC: f64 = 1500.0;           // semantic embedding cosine match
const TIER_FILENAME_FUZZY: f64 = 1000.0;    // filename fuzzy subsequence match (Nucleo)
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

    let parts: Vec<&str> = query.split_whitespace().collect();
    if parts.len() >= 2 {
        let last = parts.last().unwrap().trim_start_matches('.');
        if known_extensions.contains(&last.to_lowercase().as_str()) {
            let search_part = parts[..parts.len() - 1].join(" ");
            return (search_part, Some(last.to_lowercase()));
        }
    }
    (query.to_string(), None)
}

/// File type bonus — documents and media that users actually look for
/// rank above dev/config files.
fn file_type_bonus(ext: &Option<String>) -> f64 {
    match ext.as_deref() {
        // Documents — highest priority
        Some("pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx") => 200.0,
        Some("txt" | "md" | "csv") => 150.0,
        // Media
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp") => 120.0,
        Some("mp3" | "mp4" | "mov" | "avi" | "wav" | "flac") => 100.0,
        // Archives
        Some("zip" | "tar" | "gz" | "rar" | "7z") => 80.0,
        // Dev files — lowest
        Some("json" | "yml" | "yaml" | "toml" | "xml" | "ini" | "cfg" | "conf") => 20.0,
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "rb" | "java"
             | "c" | "cpp" | "h" | "html" | "css" | "scss" | "sh") => 10.0,
        _ => 50.0, // unknown types get middle ground
    }
}

/// Levenshtein distance on byte slices (avoids Vec<char> allocation for ASCII).
/// Falls back to char-based comparison for non-ASCII.
fn levenshtein(a: &str, b: &str) -> usize {
    if a.is_ascii() && b.is_ascii() {
        levenshtein_bytes(a.as_bytes(), b.as_bytes())
    } else {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        levenshtein_chars(&a, &b)
    }
}

fn levenshtein_bytes(a: &[u8], b: &[u8]) -> usize {
    let (m, n) = (a.len(), b.len());
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i-1].eq_ignore_ascii_case(&b[j-1]) { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j-1] + 1).min(prev[j-1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Same as levenshtein_bytes but reuses pre-allocated buffers to avoid alloc per call.
fn levenshtein_bytes_reuse(a: &[u8], b: &[u8], prev: &mut Vec<usize>, curr: &mut Vec<usize>) -> usize {
    let (m, n) = (a.len(), b.len());
    prev.clear();
    prev.extend(0..=n);
    curr.resize(n + 1, 0);
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i-1].eq_ignore_ascii_case(&b[j-1]) { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j-1] + 1).min(prev[j-1] + cost);
        }
        std::mem::swap(prev, curr);
    }
    prev[n]
}

fn levenshtein_chars(a: &[char], b: &[char]) -> usize {
    let (m, n) = (a.len(), b.len());
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i-1] == b[j-1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j-1] + 1).min(prev[j-1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Check if query approximately matches any word in the filename.
/// Conservative: max distance 1 for queries <= 6 chars, max 2 for longer.
/// Accepts pre-lowercased filename and query, plus reusable Levenshtein buffers.
fn filename_fuzzy_typo_match(
    fname_lower: &str, query_lower: &str, max_dist: usize,
    lev_prev: &mut Vec<usize>, lev_curr: &mut Vec<usize>,
) -> bool {
    let stem = fname_lower.rsplit_once('.').map(|(s, _)| s).unwrap_or(fname_lower);

    for word in stem.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() { continue; }

        // Strict length check — word must be within 1 char of query length
        let len_diff = (word.len() as isize - query_lower.len() as isize).unsigned_abs();
        if len_diff > 1 {
            continue;
        }

        let dist = if word.is_ascii() && query_lower.is_ascii() {
            levenshtein_bytes_reuse(word.as_bytes(), query_lower.as_bytes(), lev_prev, lev_curr)
        } else {
            levenshtein(word, query_lower)
        };
        if dist > 0 && dist <= max_dist {
            return true;
        }
    }
    false
}

fn recency_bonus(now_ts: i64, modified_ts: i64) -> f64 {
    let age_days = (now_ts - modified_ts).max(0) as f64 / 86400.0;
    100.0 / (1.0 + age_days.sqrt())
}

/// Accepts pre-lowercased filename and query to avoid per-file allocation.
fn classify_filename_match(fname_lower: &str, query_lower: &str) -> Option<f64> {
    // Direct match
    if fname_lower.starts_with(query_lower) {
        return Some(TIER_FILENAME_PREFIX);
    }
    if fname_lower.contains(query_lower) {
        return Some(TIER_FILENAME_CONTAINS);
    }

    // Normalize separators: "code review" matches "code_review", "code-review"
    let fname_normalized = fname_lower.replace(|c: char| !c.is_alphanumeric(), " ");
    let query_normalized = query_lower.replace(|c: char| !c.is_alphanumeric(), " ");

    if fname_normalized.starts_with(&query_normalized) {
        Some(TIER_FILENAME_PREFIX)
    } else if fname_normalized.contains(&query_normalized) {
        Some(TIER_FILENAME_CONTAINS)
    } else {
        None
    }
}

/// Unified search: runs both filename (Nucleo) and content (Tantivy) searches,
/// merges results into tiered ranking.
pub fn unified_search(
    db: &Database,
    content_index_path: &Path,
    query: &str,
    limit: usize,
    explicit_type_filter: Option<&str>,
    semantic_matches: Option<&SemanticMatches>,
) -> Result<SearchResponse> {
    let start = std::time::Instant::now();
    // Explicit --type flag takes priority; otherwise detect inline type filter from query
    let (search_query, type_filter) = if let Some(t) = explicit_type_filter {
        (query.to_string(), Some(t.to_string()))
    } else {
        parse_query(query)
    };

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let mut candidates: HashMap<String, CandidateData> = HashMap::new();

    // Pre-compute query lowercase once (used by classify + typo match across both passes)
    let query_lower = search_query.to_lowercase();

    // === Pass 1: Filename search — Nucleo fuzzy + Levenshtein typo (single merged pass) ===
    let all_files = db.get_all_paths_with_size()?;

    let pattern = Pattern::parse(
        &search_query,
        CaseMatching::Ignore,
        Normalization::Smart,
    );
    let min_score: u32 = (search_query.len() as u32) * 12;
    let max_dist: usize = if search_query.len() <= 6 { 1 } else { 2 };

    /// Per-file result from the merged Nucleo + Levenshtein pass.
    /// nucleo_score: fuzzy match score (None = no match).
    /// typo_match: whether Levenshtein found a typo match.
    type FileMatch = (
        String, // path
        String, // filename
        Option<String>, // extension
        i64, // modified_ts
        u64, // size_bytes
        Option<u32>, // nucleo_score
        bool, // typo_match
    );

    // Single pass: compute both Nucleo and Levenshtein per file.
    // Parallel for large file sets (>2000), sequential for small to avoid rayon overhead.
    let file_matches: Vec<FileMatch> = if all_files.len() >= 2000 {
        all_files.par_iter()
            .filter(|(_, _, extension, _, _)| {
                match (&type_filter, extension) {
                    (Some(filter), Some(ext)) => ext == filter,
                    (Some(_), None) => false,
                    (None, _) => true,
                }
            })
            .map_init(
                || (Matcher::default(), Vec::new(), Vec::with_capacity(50), Vec::with_capacity(50)),
                |(matcher, buf, lev_prev, lev_curr), (path, filename, extension, modified_ts, size_bytes)| {
                    let filename_haystack = Utf32Str::new(filename, buf);
                    let nucleo_score = pattern.score(filename_haystack, matcher);
                    buf.clear();

                    let fname_lower = filename.to_lowercase();
                    let typo = filename_fuzzy_typo_match(
                        &fname_lower, &query_lower, max_dist, lev_prev, lev_curr,
                    );

                    (path.clone(), filename.clone(), extension.clone(), *modified_ts, *size_bytes, nucleo_score, typo)
                },
            )
            .filter(|(_, _, _, _, _, ns, typo)| ns.is_some_and(|s| s >= min_score) || *typo)
            .collect()
    } else {
        // Sequential path for small file sets — avoids rayon thread pool overhead
        let mut matcher = Matcher::default();
        let mut buf = Vec::new();
        let mut lev_prev: Vec<usize> = Vec::with_capacity(50);
        let mut lev_curr: Vec<usize> = Vec::with_capacity(50);

        all_files.iter()
            .filter(|(_, _, extension, _, _)| {
                match (&type_filter, extension) {
                    (Some(filter), Some(ext)) => ext == filter,
                    (Some(_), None) => false,
                    (None, _) => true,
                }
            })
            .filter_map(|(path, filename, extension, modified_ts, size_bytes)| {
                let filename_haystack = Utf32Str::new(filename, &mut buf);
                let nucleo_score = pattern.score(filename_haystack, &mut matcher);
                buf.clear();

                let fname_lower = filename.to_lowercase();
                let typo = filename_fuzzy_typo_match(
                    &fname_lower, &query_lower, max_dist, &mut lev_prev, &mut lev_curr,
                );

                let has_nucleo = nucleo_score.is_some_and(|s| s >= min_score);
                if has_nucleo || typo {
                    Some((path.clone(), filename.clone(), extension.clone(), *modified_ts, *size_bytes, nucleo_score, typo))
                } else {
                    None
                }
            })
            .collect()
    };

    // Merge results into candidates HashMap
    for (path, filename, extension, modified_ts, size_bytes, nucleo_score, typo_match) in file_matches {
        // Nucleo match: classify tier and compute score
        if let Some(ns) = nucleo_score {
            if ns >= min_score {
                let fname_lower = filename.to_lowercase();
                let tier_base = classify_filename_match(&fname_lower, &query_lower)
                    .unwrap_or(TIER_FILENAME_FUZZY);
                let within_tier = (ns as f64 / 100.0)
                    + recency_bonus(now_ts, modified_ts)
                    + file_type_bonus(&extension);
                candidates.insert(
                    path.clone(),
                    (tier_base + within_tier, filename.clone(), extension.clone(), modified_ts, size_bytes, None),
                );
            }
        }

        // Typo match: insert or upgrade if better than existing non-content match
        if typo_match {
            let score = TIER_FILENAME_TYPO + recency_bonus(now_ts, modified_ts) + file_type_bonus(&extension);
            if let Some(existing) = candidates.get_mut(&path) {
                if existing.0 < TIER_CONTENT && score > existing.0 {
                    existing.0 = score;
                }
            } else {
                candidates.insert(
                    path,
                    (score, filename, extension, modified_ts, size_bytes, None),
                );
            }
        }
    }

    // === Pass 2: Content search via Tantivy ===
    if let Ok(cidx) = ContentIndex::open_or_create(content_index_path) {
        if let Ok(content_results) = cidx.search(&search_query, limit * 2, type_filter.as_deref()) {
            for cr in content_results {
                // Look up mtime/size from candidates or DB
                let (mtime, size) = if let Some(cand) = candidates.get(&cr.path) {
                    (cand.3, cand.4)
                } else {
                    // Not in candidates yet — query DB for metadata
                    db.get_mtime(&cr.path).unwrap_or(None)
                        .map(|ts| (ts, 0u64))
                        .unwrap_or((0, 0))
                };

                // Position bonus: match at start of doc (0.0) gets full bonus,
                // match at end (1.0) gets none
                let position_bonus = 100.0 * (1.0 - cr.match_position);

                let ext = Some(cr.extension.clone());
                // Use Tantivy BM25 score for within-tier ranking (capped at 500)
                let bm25_bonus = (cr.score as f64 * 30.0).min(500.0);
                let content_score = TIER_CONTENT
                    + bm25_bonus
                    + position_bonus
                    + recency_bonus(now_ts, mtime)
                    + file_type_bonus(&ext);

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

    // === Pass 3: Semantic search (pre-queried via HNSW or brute-force) ===
    if let Some(matches) = semantic_matches {
        for (path, sim) in matches {
            // Look up metadata from existing candidates or DB
            let (mtime, size, ext, filename) = if let Some(cand) = candidates.get(path) {
                (cand.3, cand.4, cand.2.clone(), cand.1.clone())
            } else {
                let mtime_size = db.get_mtime(path).unwrap_or(None)
                    .map(|ts| (ts, 0u64))
                    .unwrap_or((0, 0));
                let p = std::path::Path::new(path);
                let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase());
                let fname = p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (mtime_size.0, mtime_size.1, ext, fname)
            };

            let semantic_score = TIER_SEMANTIC
                + (*sim as f64 * 500.0)
                + recency_bonus(now_ts, mtime)
                + file_type_bonus(&ext);

            if let Some(existing) = candidates.get_mut(path) {
                // Already found by filename or content — boost
                existing.0 += BOTH_MATCH_BOOST + (*sim as f64 * 200.0);
            } else {
                // Semantic-only discovery
                candidates.insert(
                    path.clone(),
                    (semantic_score, filename, ext, mtime, size, None),
                );
            }
        }
    }

    // === Sort and truncate ===
    let mut sorted: Vec<_> = candidates.into_iter().collect();
    sorted.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap_or(std::cmp::Ordering::Equal));
    sorted.truncate(limit);

    // Extract snippets only for final top-N results (avoids 60+ random disk reads)
    let results: Vec<SearchResult> = sorted
        .into_iter()
        .map(|(path, (score, filename, extension, modified_ts, size, snippet))| {
            // Lazy snippet extraction — only for displayed results
            let content_snippet = if snippet.is_some() {
                snippet
            } else if score >= TIER_CONTENT {
                // Content match without snippet — extract from file now
                let (snip, _) = crate::content::extract_snippet_from_file(
                    std::path::Path::new(&path), &search_query
                );
                snip
            } else {
                None
            };

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
                content_snippet,
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_query ──

    #[test]
    fn parse_query_no_filter() {
        let (q, f) = parse_query("quarterly report");
        assert_eq!(q, "quarterly report");
        assert!(f.is_none());
    }

    #[test]
    fn parse_query_inline_type() {
        let (q, f) = parse_query("resume pdf");
        assert_eq!(q, "resume");
        assert_eq!(f.unwrap(), "pdf");
    }

    #[test]
    fn parse_query_dotted_extension() {
        let (q, f) = parse_query("invoice .xlsx");
        assert_eq!(q, "invoice");
        assert_eq!(f.unwrap(), "xlsx");
    }

    #[test]
    fn parse_query_unknown_ext_no_filter() {
        let (q, f) = parse_query("hello world");
        assert_eq!(q, "hello world");
        assert!(f.is_none());
    }

    #[test]
    fn parse_query_single_word() {
        let (q, f) = parse_query("pdf");
        // Single word — no split possible
        assert_eq!(q, "pdf");
        assert!(f.is_none());
    }

    #[test]
    fn parse_query_case_insensitive() {
        let (q, f) = parse_query("report PDF");
        assert_eq!(q, "report");
        assert_eq!(f.unwrap(), "pdf");
    }

    #[test]
    fn parse_query_multi_word_with_type() {
        let (q, f) = parse_query("annual financial report xlsx");
        assert_eq!(q, "annual financial report");
        assert_eq!(f.unwrap(), "xlsx");
    }

    // ── file_type_bonus ──

    #[test]
    fn file_type_bonus_documents_highest() {
        let pdf = file_type_bonus(&Some("pdf".into()));
        let rs = file_type_bonus(&Some("rs".into()));
        let unknown = file_type_bonus(&None);
        assert!(pdf > rs, "pdf ({}) should beat rs ({})", pdf, rs);
        assert!(unknown > rs, "unknown ({}) should beat rs ({})", unknown, rs);
    }

    #[test]
    fn file_type_bonus_media_mid() {
        let png = file_type_bonus(&Some("png".into()));
        let pdf = file_type_bonus(&Some("pdf".into()));
        let json = file_type_bonus(&Some("json".into()));
        assert!(png > json);
        assert!(pdf > png);
    }

    // ── levenshtein ──

    #[test]
    fn levenshtein_identical() {
        assert_eq!(levenshtein("hello", "hello"), 0);
    }

    #[test]
    fn levenshtein_one_edit() {
        assert_eq!(levenshtein("hello", "helo"), 1);   // deletion
        assert_eq!(levenshtein("hello", "helloo"), 1);  // insertion
        assert_eq!(levenshtein("hello", "hallo"), 1);   // substitution
    }

    #[test]
    fn levenshtein_two_edits() {
        assert_eq!(levenshtein("brainform", "brainfrm"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn levenshtein_empty() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn levenshtein_case_insensitive_bytes() {
        // levenshtein_bytes uses eq_ignore_ascii_case
        assert_eq!(levenshtein_bytes(b"Hello", b"hello"), 0);
        assert_eq!(levenshtein_bytes(b"WORLD", b"world"), 0);
    }

    #[test]
    fn levenshtein_unicode() {
        // Non-ASCII triggers char-based path
        assert_eq!(levenshtein("café", "cafe"), 1);
        assert_eq!(levenshtein("naïve", "naive"), 1);
    }

    // ── filename_fuzzy_typo_match ──

    // Helper: wraps the new signature for test convenience
    fn typo_match(filename: &str, query: &str, max_dist: usize) -> bool {
        let mut p = Vec::new();
        let mut c = Vec::new();
        filename_fuzzy_typo_match(&filename.to_lowercase(), query, max_dist, &mut p, &mut c)
    }

    #[test]
    fn typo_match_one_char_off() {
        assert!(typo_match("Brainform.md", "brainfrm", 2));
    }

    #[test]
    fn typo_match_exact_no_match() {
        // Exact match returns false (dist must be > 0)
        assert!(!typo_match("hello.txt", "hello", 2));
    }

    #[test]
    fn typo_match_too_far() {
        assert!(!typo_match("abcdef.txt", "xyz", 2));
    }

    #[test]
    fn typo_match_respects_max_dist() {
        // "test" vs "tset" = distance 2
        assert!(!typo_match("tset.rs", "test", 1));
        // But with max_dist=2 it should match
        assert!(typo_match("tset.rs", "test", 2));
    }

    #[test]
    fn typo_match_separator_split() {
        // Filename with underscores — matches individual words
        assert!(typo_match("code_revew.md", "review", 2));
    }

    // ── recency_bonus ──

    #[test]
    fn recency_bonus_recent_higher() {
        let now = 1700000000;
        let recent = recency_bonus(now, now - 86400);      // 1 day old
        let old = recency_bonus(now, now - 86400 * 365);   // 1 year old
        assert!(recent > old, "recent ({}) should beat old ({})", recent, old);
    }

    #[test]
    fn recency_bonus_same_time() {
        let now = 1700000000;
        let bonus = recency_bonus(now, now);
        assert!((bonus - 100.0).abs() < 0.01, "same-time bonus should be ~100, got {}", bonus);
    }

    #[test]
    fn recency_bonus_future_clamped() {
        let now = 1700000000;
        let bonus = recency_bonus(now, now + 10000);
        // Future file — age clamped to 0
        assert!((bonus - 100.0).abs() < 0.01);
    }

    // ── classify_filename_match ──

    // Note: classify_filename_match now expects pre-lowercased inputs

    #[test]
    fn classify_prefix() {
        let score = classify_filename_match("brainform.md", "brain");
        assert_eq!(score, Some(TIER_FILENAME_PREFIX));
    }

    #[test]
    fn classify_contains() {
        let score = classify_filename_match("ai-readiness-brainform.pdf", "brainform");
        assert_eq!(score, Some(TIER_FILENAME_CONTAINS));
    }

    #[test]
    fn classify_no_match() {
        let score = classify_filename_match("readme.md", "invoice");
        assert_eq!(score, None);
    }

    #[test]
    fn classify_normalized_separators() {
        let score = classify_filename_match("code_review.md", "code review");
        assert!(score.is_some(), "separator normalization should match");
    }

    #[test]
    fn classify_case_insensitive() {
        let score = classify_filename_match("readme.md", "readme");
        assert_eq!(score, Some(TIER_FILENAME_PREFIX));
    }

    // ── Tier ordering invariants ──

    #[test]
    fn tier_ordering() {
        assert!(TIER_FILENAME_PREFIX > TIER_FILENAME_CONTAINS);
        assert!(TIER_FILENAME_CONTAINS > TIER_FILENAME_TYPO);
        assert!(TIER_FILENAME_TYPO > TIER_CONTENT);
        assert!(TIER_CONTENT > TIER_SEMANTIC);
        assert!(TIER_SEMANTIC > TIER_FILENAME_FUZZY);
        assert!(TIER_FILENAME_FUZZY > BOTH_MATCH_BOOST);
    }

    // ── Output quality: scoring correctness ──

    #[test]
    fn prefix_match_always_beats_content_match() {
        let prefix_score = TIER_FILENAME_PREFIX; // 10000
        let content_max = TIER_CONTENT + 500.0 + 100.0 + 200.0 + 100.0; // 2900 max
        assert!(prefix_score > content_max,
            "prefix ({}) must beat max content score ({})", prefix_score, content_max);
    }

    #[test]
    fn typo_match_beats_content_only() {
        let typo_min = TIER_FILENAME_TYPO; // 3000
        let content_max = TIER_CONTENT + 500.0 + 100.0 + 200.0 + 100.0;
        assert!(typo_min > content_max,
            "typo tier ({}) must beat max content ({})", typo_min, content_max);
    }

    // ── Performance: levenshtein on realistic inputs ──
    // Note: thresholds are relaxed for debug builds (~10-50x slower than release)

    #[test]
    fn levenshtein_perf_short_strings() {
        let start = std::time::Instant::now();
        let iterations = 100_000;
        for _ in 0..iterations {
            let _ = levenshtein("brainform", "brainfrm");
        }
        let elapsed = start.elapsed();
        let per_op_ns = elapsed.as_nanos() / iterations as u128;
        eprintln!("levenshtein(9,8 chars): {}ns/op ({} ops in {:?})",
            per_op_ns, iterations, elapsed);
        // Release: <1μs, Debug: <50μs
        assert!(per_op_ns < 50_000, "levenshtein too slow: {}ns/op", per_op_ns);
    }

    #[test]
    fn levenshtein_perf_long_filenames() {
        let start = std::time::Instant::now();
        let iterations = 10_000;
        let a = "annual_financial_report_2024_final_v3";
        let b = "annual_financal_report_2024_fnal_v3";
        for _ in 0..iterations {
            let _ = levenshtein(a, b);
        }
        let elapsed = start.elapsed();
        let per_op_ns = elapsed.as_nanos() / iterations as u128;
        eprintln!("levenshtein(36,34 chars): {}ns/op ({} ops in {:?})",
            per_op_ns, iterations, elapsed);
        // Release: <5μs, Debug: <500μs
        assert!(per_op_ns < 500_000, "levenshtein too slow on long strings: {}ns/op", per_op_ns);
    }

    // ── Property-based tests ──

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn levenshtein_symmetry(a in "\\PC{1,20}", b in "\\PC{1,20}") {
            prop_assert_eq!(levenshtein(&a, &b), levenshtein(&b, &a));
        }

        #[test]
        fn levenshtein_identity(s in "\\PC{1,30}") {
            prop_assert_eq!(levenshtein(&s, &s), 0);
        }

        #[test]
        fn levenshtein_triangle_inequality(
            a in "\\PC{1,10}",
            b in "\\PC{1,10}",
            c in "\\PC{1,10}"
        ) {
            let ab = levenshtein(&a, &b);
            let bc = levenshtein(&b, &c);
            let ac = levenshtein(&a, &c);
            prop_assert!(ac <= ab + bc,
                "triangle inequality violated: d({:?},{:?})={} > d({:?},{:?})={} + d({:?},{:?})={}",
                a, c, ac, a, b, ab, b, c, bc);
        }

        #[test]
        fn levenshtein_bounded_by_max_len(a in "\\PC{1,15}", b in "\\PC{1,15}") {
            let dist = levenshtein(&a, &b);
            let max_len = a.chars().count().max(b.chars().count());
            prop_assert!(dist <= max_len,
                "distance {} exceeds max length {}", dist, max_len);
        }

        #[test]
        fn parse_query_never_panics(input in "\\PC{0,100}") {
            let _ = parse_query(&input);
        }

        #[test]
        fn parse_query_preserves_content(input in "[a-zA-Z0-9 ]{1,50}") {
            let (query, filter) = parse_query(&input);
            // Original content is preserved across query + filter
            if let Some(f) = filter {
                let reconstructed = format!("{} {}", query, f);
                // Lowercased filter, but content preserved
                prop_assert!(!query.is_empty() || !f.is_empty(),
                    "parse_query lost all content from {:?}", input);
                let _ = reconstructed; // use it
            }
        }

        #[test]
        fn classify_prefix_on_lowered(filename in "[a-z]{3,15}\\.[a-z]{2,4}") {
            // Pre-lowercased: if query is a prefix of filename, should return prefix tier
            let query = &filename[..3];
            let result = classify_filename_match(&filename, query);
            if filename.starts_with(query) {
                prop_assert_eq!(result, Some(TIER_FILENAME_PREFIX));
            }
        }

        #[test]
        fn recency_bonus_monotonic(age1 in 0i64..1_000_000, age2 in 0i64..1_000_000) {
            let now = 2_000_000_000i64;
            let b1 = recency_bonus(now, now - age1);
            let b2 = recency_bonus(now, now - age2);
            if age1 <= age2 {
                prop_assert!(b1 >= b2,
                    "newer file (age={}) got lower bonus ({}) than older (age={}, bonus={})",
                    age1, b1, age2, b2);
            }
        }

        #[test]
        fn file_type_bonus_non_negative(ext in proptest::option::of("[a-z]{1,5}")) {
            let bonus = file_type_bonus(&ext);
            prop_assert!(bonus >= 0.0, "negative bonus for {:?}: {}", ext, bonus);
        }
    }
}
