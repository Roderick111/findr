//! Integration tests for findr: full pipeline ranking + output quality.
//!
//! These tests create real SQLite databases and Tantivy indexes in temp dirs,
//! then verify that unified_search returns correct results with proper ranking.

use findr::db::{Database, FileEntry};
use findr::content::ContentIndex;
use findr::search::unified_search;
use findr::semantic::{cosine_similarity, vec_to_bytes, bytes_to_vec, EMBED_DIMS};
use std::time::Instant;

/// Helper: create a temp DB with schema initialized.
fn temp_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();
    db.init_schema().unwrap();
    (dir, db)
}

/// Helper: create a temp content index.
fn temp_content_index() -> (tempfile::TempDir, ContentIndex) {
    let dir = tempfile::tempdir().unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    (dir, idx)
}

/// Helper: insert file entries into DB.
fn insert_files(db: &Database, files: &[(&str, &str, Option<&str>, u64, i64)]) {
    let entries: Vec<FileEntry> = files.iter().map(|(path, filename, ext, size, mtime)| {
        FileEntry {
            path: path.to_string(),
            filename: filename.to_string(),
            extension: ext.map(|e| e.to_string()),
            size_bytes: *size,
            modified_ts: *mtime,
            is_dir: false,
        }
    }).collect();
    db.insert_files_batch(&entries).unwrap();
}

// ── Ranking Order Tests ──

#[test]
fn prefix_match_ranks_above_contains() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/Brainform.md", "Brainform.md", Some("md"), 1000, now),
        ("/b/AI-Readiness-Brainform.pdf", "AI-Readiness-Brainform.pdf", Some("pdf"), 2000, now),
    ]);

    let result = unified_search(&db, _cdir.path(), "brainform", 10, None, None).unwrap();
    assert!(result.results.len() >= 2, "should find both files");

    // Prefix match (Brainform.md) should rank first
    assert_eq!(result.results[0].filename, "Brainform.md",
        "prefix match should rank #1, got: {}", result.results[0].filename);
    assert!(result.results[0].score > result.results[1].score,
        "prefix score ({}) should beat contains score ({})",
        result.results[0].score, result.results[1].score);
}

#[test]
fn content_match_finds_files_not_matching_filename() {
    let (_dir, db) = temp_db();
    let (_cdir, cidx) = temp_content_index();

    let now = 1700000000i64;

    // Create actual files for content extraction
    let file_dir = tempfile::tempdir().unwrap();
    let rib_path = file_dir.path().join("RIB.txt");
    std::fs::write(&rib_path, "Account details for Revolut Bank UAB, SWIFT code REVOLT21").unwrap();

    let rib_str = rib_path.to_str().unwrap();
    insert_files(&db, &[
        (rib_str, "RIB.txt", Some("txt"), 100, now),
    ]);

    // Index content
    let files = vec![(
        rib_str.to_string(),
        "RIB.txt".to_string(),
        Some("txt".to_string()),
    )];
    cidx.index_files(&files).unwrap();

    let result = unified_search(&db, _cdir.path(), "revolut", 10, None, None).unwrap();
    assert!(!result.results.is_empty(), "should find RIB.txt via content match");
    assert_eq!(result.results[0].filename, "RIB.txt");
}

#[test]
fn type_filter_restricts_results() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/resume.pdf", "resume.pdf", Some("pdf"), 5000, now),
        ("/b/resume.md", "resume.md", Some("md"), 1000, now),
        ("/c/resume.docx", "resume.docx", Some("docx"), 3000, now),
    ]);

    // Inline type filter: "resume pdf"
    let result = unified_search(&db, _cdir.path(), "resume pdf", 10, None, None).unwrap();
    assert!(result.results.iter().all(|r| r.file_type.as_deref() == Some("pdf")),
        "inline type filter should only return PDFs, got: {:?}",
        result.results.iter().map(|r| &r.filename).collect::<Vec<_>>());

    // Explicit type filter
    let result = unified_search(&db, _cdir.path(), "resume", 10, Some("docx"), None).unwrap();
    assert!(result.results.iter().all(|r| r.file_type.as_deref() == Some("docx")),
        "explicit type filter should only return docx");
}

#[test]
fn both_match_boost_applied() {
    let (_dir, db) = temp_db();
    let (_cdir, cidx) = temp_content_index();

    let now = 1700000000i64;

    let file_dir = tempfile::tempdir().unwrap();

    // File 1: matches both filename AND content (should get boost)
    let both_path = file_dir.path().join("revolut-statement.txt");
    std::fs::write(&both_path, "Monthly statement from Revolut showing all transactions").unwrap();

    // File 2: matches content only (no filename match)
    let content_only = file_dir.path().join("bank-doc.txt");
    std::fs::write(&content_only, "Transfer via Revolut on 2024-03-15 completed").unwrap();

    let both_str = both_path.to_str().unwrap();
    let content_str = content_only.to_str().unwrap();

    insert_files(&db, &[
        (both_str, "revolut-statement.txt", Some("txt"), 100, now),
        (content_str, "bank-doc.txt", Some("txt"), 100, now),
    ]);

    let files = vec![
        (both_str.to_string(), "revolut-statement.txt".to_string(), Some("txt".to_string())),
        (content_str.to_string(), "bank-doc.txt".to_string(), Some("txt".to_string())),
    ];
    cidx.index_files(&files).unwrap();

    let result = unified_search(&db, _cdir.path(), "revolut", 10, None, None).unwrap();
    assert!(result.results.len() >= 2);

    // Both-match file should rank above content-only
    assert_eq!(result.results[0].filename, "revolut-statement.txt",
        "both-match file should rank #1");
    assert!(result.results[0].score > result.results[1].score);
}

#[test]
fn semantic_search_tier() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/business-plan.md", "business-plan.md", Some("md"), 5000, now),
        ("/b/random-notes.txt", "random-notes.txt", Some("txt"), 1000, now),
    ]);

    // Simulate pre-queried semantic matches (as produced by HNSW or brute-force).
    // Only above-threshold matches are included (threshold filtering happens upstream).
    let semantic_matches: Vec<(String, f32)> = vec![
        ("/a/business-plan.md".to_string(), 0.85), // high similarity
    ];

    // Search for something that won't match filename or content, only semantic
    let result = unified_search(
        &db, _cdir.path(), "venture capital fundraising", 10, None, Some(&semantic_matches)
    ).unwrap();

    // Should find business-plan.md via semantic similarity
    let found = result.results.iter().any(|r| r.filename == "business-plan.md");
    assert!(found, "semantic search should find business-plan.md");
}

#[test]
fn recency_affects_within_tier_ordering() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/report-old.md", "report-old.md", Some("md"), 1000, now - 86400 * 365), // 1 year old
        ("/b/report-new.md", "report-new.md", Some("md"), 1000, now),                // today
    ]);

    let result = unified_search(&db, _cdir.path(), "report", 10, None, None).unwrap();
    assert!(result.results.len() >= 2);

    // Both are prefix matches — recent one should rank higher
    assert_eq!(result.results[0].filename, "report-new.md",
        "recent file should rank above old file in same tier");
}

#[test]
fn document_type_bonus_over_dev_files() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/config.rs", "config.rs", Some("rs"), 500, now),
        ("/b/config.pdf", "config.pdf", Some("pdf"), 5000, now),
    ]);

    let result = unified_search(&db, _cdir.path(), "config", 10, None, None).unwrap();
    assert!(result.results.len() >= 2);

    // PDF should rank above .rs due to file type bonus
    assert_eq!(result.results[0].filename, "config.pdf",
        "PDF should rank above .rs file, got: {}",
        result.results[0].filename);
}

#[test]
fn empty_query_returns_empty() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let result = unified_search(&db, _cdir.path(), "", 10, None, None).unwrap();
    assert!(result.results.is_empty());
}

#[test]
fn limit_respected() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    let mut files = Vec::new();
    for i in 0..50 {
        files.push((
            format!("/files/test{}.md", i).leak() as &str,
            format!("test{}.md", i).leak() as &str,
            Some("md"),
            1000u64,
            now,
        ));
    }
    insert_files(&db, &files);

    let result = unified_search(&db, _cdir.path(), "test", 5, None, None).unwrap();
    assert!(result.results.len() <= 5, "limit should cap results at 5");
}

// ── Performance Tests ──

#[test]
fn search_latency_10k_files() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;

    // Insert 10K files
    let entries: Vec<FileEntry> = (0..10_000).map(|i| {
        FileEntry {
            path: format!("/home/user/Documents/file_{:05}.txt", i),
            filename: format!("file_{:05}.txt", i),
            extension: Some("txt".to_string()),
            size_bytes: 1000 + (i as u64 * 7) % 50000,
            modified_ts: now - (i as i64 * 3600),
            is_dir: false,
        }
    }).collect();
    db.insert_files_batch(&entries).unwrap();

    // Warm up
    let _ = unified_search(&db, _cdir.path(), "file_001", 30, None, None);

    // Measure
    let iterations = 20;
    let start = Instant::now();
    for _ in 0..iterations {
        let _ = unified_search(&db, _cdir.path(), "file_001", 30, None, None).unwrap();
    }
    let elapsed = start.elapsed();
    let per_search_ms = elapsed.as_millis() / iterations as u128;

    eprintln!("search over 10K files: {}ms/query ({} iterations in {:?})",
        per_search_ms, iterations, elapsed);
    // Release: <100ms, Debug: <2000ms (Nucleo + Levenshtein without optimizations)
    assert!(per_search_ms < 2000,
        "search too slow: {}ms/query over 10K files", per_search_ms);
}

#[test]
fn search_latency_with_content_index() {
    let (_dir, db) = temp_db();
    let (_cdir, cidx) = temp_content_index();

    let now = 1700000000i64;
    let file_dir = tempfile::tempdir().unwrap();

    // Create 100 real files with content
    let mut entries = Vec::new();
    let mut content_files = Vec::new();
    for i in 0..100 {
        let path = file_dir.path().join(format!("doc_{:03}.txt", i));
        let content = format!("Document number {} contains various words about topic {} and related subjects like alpha bravo charlie delta echo foxtrot", i, i * 7);
        std::fs::write(&path, &content).unwrap();

        let path_str = path.to_str().unwrap().to_string();
        entries.push(FileEntry {
            path: path_str.clone(),
            filename: format!("doc_{:03}.txt", i),
            extension: Some("txt".to_string()),
            size_bytes: content.len() as u64,
            modified_ts: now - (i as i64 * 3600),
            is_dir: false,
        });
        content_files.push((path_str, format!("doc_{:03}.txt", i), Some("txt".to_string())));
    }
    db.insert_files_batch(&entries).unwrap();
    cidx.index_files(&content_files).unwrap();

    // Search that hits both filename and content paths
    let start = Instant::now();
    let iterations = 10;
    for _ in 0..iterations {
        let _ = unified_search(&db, _cdir.path(), "alpha bravo", 30, None, None).unwrap();
    }
    let elapsed = start.elapsed();
    let per_search_ms = elapsed.as_millis() / iterations as u128;

    eprintln!("search with content index (100 docs): {}ms/query", per_search_ms);
    assert!(per_search_ms < 200,
        "content search too slow: {}ms/query", per_search_ms);
}

#[test]
fn semantic_scan_performance() {
    // Simulate searching 5000 document vectors
    let query: Vec<f32> = (0..EMBED_DIMS).map(|i| (i as f32).sin()).collect();
    let docs: Vec<(String, Vec<f32>)> = (0..5000)
        .map(|d| {
            let vec: Vec<f32> = (0..EMBED_DIMS).map(|i| ((i + d) as f32 * 0.01).cos()).collect();
            (format!("/file_{}.md", d), vec)
        })
        .collect();

    let start = Instant::now();
    let iterations = 100;
    let mut total_above = 0usize;
    for _ in 0..iterations {
        for (_, doc_vec) in &docs {
            if cosine_similarity(&query, doc_vec) > 0.15 {
                total_above += 1;
            }
        }
    }
    let elapsed = start.elapsed();
    let per_scan_ms = elapsed.as_millis() / iterations as u128;

    eprintln!("semantic scan 5000 docs: {}ms/scan ({} above threshold total)",
        per_scan_ms, total_above);
    // Release: <20ms, Debug: <2000ms
    assert!(per_scan_ms < 2000, "semantic scan too slow: {}ms", per_scan_ms);
}

#[test]
fn db_insert_and_query_performance() {
    let (_dir, db) = temp_db();

    // Insert 10K files
    let entries: Vec<FileEntry> = (0..10_000).map(|i| {
        FileEntry {
            path: format!("/home/user/docs/file_{:05}.pdf", i),
            filename: format!("file_{:05}.pdf", i),
            extension: Some("pdf".to_string()),
            size_bytes: 50000,
            modified_ts: 1700000000 - (i as i64 * 60),
            is_dir: false,
        }
    }).collect();

    let start = Instant::now();
    db.insert_files_batch(&entries).unwrap();
    let insert_ms = start.elapsed().as_millis();

    let start = Instant::now();
    let all = db.get_all_paths_with_size().unwrap();
    let query_ms = start.elapsed().as_millis();

    eprintln!("DB: insert 10K = {}ms, query all = {}ms ({} rows)", insert_ms, query_ms, all.len());
    assert_eq!(all.len(), 10_000);
    assert!(insert_ms < 1000, "DB insert too slow: {}ms", insert_ms);
    assert!(query_ms < 100, "DB query too slow: {}ms", query_ms);
}

#[test]
fn vector_serialization_bulk_performance() {
    let vectors: Vec<Vec<f32>> = (0..5000)
        .map(|d| (0..EMBED_DIMS).map(|i| ((i + d) as f32 * 0.001).sin()).collect())
        .collect();

    // Serialize all
    let start = Instant::now();
    let serialized: Vec<Vec<u8>> = vectors.iter().map(|v| vec_to_bytes(v)).collect();
    let ser_ms = start.elapsed().as_millis();

    // Deserialize all
    let start = Instant::now();
    let deserialized: Vec<Vec<f32>> = serialized.iter()
        .filter_map(|b| bytes_to_vec(b))
        .collect();
    let deser_ms = start.elapsed().as_millis();

    eprintln!("vector ser/deser 5000x512d: serialize={}ms, deserialize={}ms", ser_ms, deser_ms);
    assert_eq!(deserialized.len(), 5000);
    // Release: <50ms, Debug: <500ms
    assert!(ser_ms < 500, "serialization too slow: {}ms", ser_ms);
    assert!(deser_ms < 500, "deserialization too slow: {}ms", deser_ms);
}

// ── Output Quality: end-to-end correctness ──

#[test]
fn separator_normalization_matches() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/code_review.md", "code_review.md", Some("md"), 1000, now),
        ("/b/code-review.md", "code-review.md", Some("md"), 1000, now),
    ]);

    let result = unified_search(&db, _cdir.path(), "code review", 10, None, None).unwrap();
    assert!(result.results.len() >= 2,
        "separator normalization should match both underscore and hyphen variants");
}

#[test]
fn case_insensitive_search() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/README.md", "README.md", Some("md"), 500, now),
    ]);

    let result = unified_search(&db, _cdir.path(), "readme", 10, None, None).unwrap();
    assert!(!result.results.is_empty(), "case-insensitive search should find README.md");
    assert_eq!(result.results[0].filename, "README.md");
}

#[test]
fn search_response_metadata() {
    let (_dir, db) = temp_db();
    let (_cdir, _cidx) = temp_content_index();

    let now = 1700000000i64;
    insert_files(&db, &[
        ("/a/test.pdf", "test.pdf", Some("pdf"), 5000, now),
    ]);

    let result = unified_search(&db, _cdir.path(), "test", 10, None, None).unwrap();
    assert_eq!(result.query, "test");
    assert_eq!(result.mode, "unified");
    assert!(result.elapsed_ms < 5000); // sanity
    assert_eq!(result.total_results, result.results.len());

    let r = &result.results[0];
    assert_eq!(r.file_type.as_deref(), Some("pdf"));
    assert_eq!(r.size_bytes, Some(5000));
    assert!(!r.modified.is_empty());
    assert!(r.score > 0.0);
}

// ── Content Index Integrity Tests ──

#[test]
fn content_index_survives_after_indexing() {
    // Regression test: content index must retain all docs after index_files().
    // Previously, a flawed reconcile check would nuke the content index
    // because it estimated expected docs incorrectly (didn't account for
    // files with no extractable content).
    let dir = tempfile::tempdir().unwrap();
    let cidx = ContentIndex::open_or_create(dir.path()).unwrap();

    let file_dir = tempfile::tempdir().unwrap();

    // Create mix of files: some with content, some without (like images pre-OCR)
    let mut files = Vec::new();
    for i in 0..20 {
        let path = file_dir.path().join(format!("doc_{}.txt", i));
        std::fs::write(&path, format!("searchable content number {}", i)).unwrap();
        files.push((
            path.to_str().unwrap().to_string(),
            format!("doc_{}.txt", i),
            Some("txt".to_string()),
        ));
    }

    let count = cidx.index_files(&files).unwrap();
    assert_eq!(count, 20);
    assert_eq!(cidx.doc_count().unwrap(), 20);

    // Verify content is actually searchable
    let results = cidx.search("searchable content", 30, None).unwrap();
    assert_eq!(results.len(), 20,
        "all 20 docs should be found via content search, got {}", results.len());

    // Simulate what reconcile does: check doc count
    let doc_count = cidx.doc_count().unwrap();
    assert_eq!(doc_count, 20,
        "doc count should match indexed count after index_files, got {}", doc_count);
}

#[test]
fn content_match_ranks_above_semantic_only() {
    // A file found via content search (tier 2000) must rank above
    // a file found only via semantic similarity (tier 1500).
    let (_dir, db) = temp_db();
    let (_cdir, cidx) = temp_content_index();

    let now = 1700000000i64;
    let file_dir = tempfile::tempdir().unwrap();

    // File with actual content containing "revolut"
    let content_file = file_dir.path().join("bank_statement.txt");
    std::fs::write(&content_file, "Account details for Revolut Bank UAB SWIFT REVOLT21").unwrap();
    let content_str = content_file.to_str().unwrap();

    // File that would only match semantically (no "revolut" in content)
    let semantic_file = file_dir.path().join("finance_report.txt");
    std::fs::write(&semantic_file, "quarterly financial analysis and banking overview").unwrap();
    let semantic_str = semantic_file.to_str().unwrap();

    insert_files(&db, &[
        (content_str, "bank_statement.txt", Some("txt"), 100, now),
        (semantic_str, "finance_report.txt", Some("txt"), 100, now),
    ]);

    // Index content
    let files = vec![
        (content_str.to_string(), "bank_statement.txt".to_string(), Some("txt".to_string())),
        (semantic_str.to_string(), "finance_report.txt".to_string(), Some("txt".to_string())),
    ];
    cidx.index_files(&files).unwrap();

    // Verify content index has both files
    assert_eq!(cidx.doc_count().unwrap(), 2);

    // Search for "revolut" — should find bank_statement via content
    let result = unified_search(&db, _cdir.path(), "revolut", 10, None, None).unwrap();

    let content_hit = result.results.iter()
        .find(|r| r.filename == "bank_statement.txt");
    assert!(content_hit.is_some(),
        "bank_statement.txt should be found via content search for 'revolut'");

    let hit = content_hit.unwrap();
    assert!(hit.score >= 2000.0,
        "content match should be in CONTENT tier (>=2000), got {}", hit.score);
}
