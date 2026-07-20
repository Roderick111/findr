use findr::content::ContentIndex;
use findr::db::Database;
use findr::pipeline;
use findr::semantic::{self, EMBED_DIMS};
use std::fs;
use tempfile::tempdir;

/// Helper: create a temp directory with some text files for indexing.
fn create_test_files(dir: &std::path::Path, files: &[(&str, &str)]) {
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }
}

/// Helper: set up isolated data_dir, db_path, content_index_path.
struct TestPaths {
    data_dir: tempfile::TempDir,
    source_dir: tempfile::TempDir,
}

impl TestPaths {
    fn new() -> Self {
        Self {
            data_dir: tempdir().unwrap(),
            source_dir: tempdir().unwrap(),
        }
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.data_dir.path().join("index.db")
    }

    fn content_index_path(&self) -> std::path::PathBuf {
        self.data_dir.path().join("content_index")
    }

    fn source(&self) -> &std::path::Path {
        self.source_dir.path()
    }

    fn data(&self) -> &std::path::Path {
        self.data_dir.path()
    }
}

// ---------------------------------------------------------------------------
// 1. run_full_index
// ---------------------------------------------------------------------------

#[test]
fn test_run_full_index_creates_db_and_content_index() {
    let tp = TestPaths::new();
    create_test_files(
        tp.source(),
        &[
            ("hello.txt", "Hello world"),
            ("notes.md", "# Notes\nSome markdown content"),
            ("data.csv", "a,b,c\n1,2,3"),
        ],
    );

    // Parent DB (used to read scan_preset metadata)
    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];
    let scan_refs: Vec<String> = scan_paths.clone();

    let result = pipeline::run_full_index(
        &parent_db,
        Some(&scan_refs),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    // DB file exists at final path
    assert!(tp.db_path().exists(), "DB should exist at final path");
    // Content index dir exists at final path
    assert!(
        tp.content_index_path().exists(),
        "Content index should exist at final path"
    );

    // No temp files left
    assert!(
        !tp.data().join("index.db.new").exists(),
        "Temp DB should be cleaned up"
    );
    assert!(
        !tp.data().join("content_index.new").exists(),
        "Temp content index should be cleaned up"
    );
    // No backup files left
    assert!(
        !tp.data().join("index.db.bak").exists(),
        "Backup DB should be cleaned up"
    );
    assert!(
        !tp.data().join("content_index.bak").exists(),
        "Backup content index should be cleaned up"
    );

    // Verify DB has files
    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();
    let count = db.file_count().unwrap();
    assert!(count >= 3, "DB should have at least 3 files, got {}", count);

    // Verify content index has docs
    let cidx = ContentIndex::open_or_create(&tp.content_index_path()).unwrap();
    let doc_count = cidx.doc_count().unwrap();
    assert!(
        doc_count >= 3,
        "Content index should have at least 3 docs, got {}",
        doc_count
    );

    // Verify content_indexed_count metadata was set
    let stored_count = db.get_meta("content_indexed_count").unwrap();
    assert!(
        stored_count.is_some(),
        "content_indexed_count metadata should be set"
    );
    let stored_count_num: usize = stored_count.unwrap().parse().unwrap();
    assert!(
        stored_count_num >= 3,
        "content_indexed_count should be >= 3, got {}",
        stored_count_num
    );
    let stored_paths: Vec<String> =
        serde_json::from_str(&db.get_meta("scan_paths").unwrap().unwrap()).unwrap();
    assert_eq!(stored_paths, scan_paths);

    // FullIndexResult fields: OCR and embed depend on external binaries/keys,
    // so just verify the struct is returned without error
    let _ = result.spawn_ocr;
    let _ = result.spawn_embed;
}

#[test]
fn test_run_full_index_atomic_swap_replaces_previous() {
    let tp = TestPaths::new();
    create_test_files(tp.source(), &[("a.txt", "first run")]);

    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];

    // First full index
    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    let db1 = Database::open(&tp.db_path()).unwrap();
    db1.init_schema().unwrap();
    let count1 = db1.file_count().unwrap();
    drop(db1);

    // Add more files
    create_test_files(
        tp.source(),
        &[("b.txt", "second file"), ("c.txt", "third file")],
    );

    // Second full index (atomic swap over existing)
    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    let db2 = Database::open(&tp.db_path()).unwrap();
    db2.init_schema().unwrap();
    let count2 = db2.file_count().unwrap();

    assert!(
        count2 > count1,
        "Second index should have more files: {} vs {}",
        count2,
        count1
    );

    // No leftover temp/backup files
    assert!(!tp.data().join("index.db.new").exists());
    assert!(!tp.data().join("content_index.new").exists());
    assert!(!tp.data().join("index.db.bak").exists());
    assert!(!tp.data().join("content_index.bak").exists());
}

// ---------------------------------------------------------------------------
// 2. run_incremental_index
// ---------------------------------------------------------------------------

#[test]
fn test_run_incremental_index_detects_new_files() {
    let tp = TestPaths::new();
    create_test_files(tp.source(), &[("orig.txt", "original content")]);

    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];

    // Build full index first
    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    // Open the new DB and store scan_paths so compute_diff can find them
    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();
    db.set_meta("scan_paths", &scan_paths[0]).unwrap();

    let count_before = db.file_count().unwrap();

    // Add new files
    create_test_files(
        tp.source(),
        &[("new1.txt", "new file one"), ("new2.txt", "new file two")],
    );

    // Run incremental
    pipeline::run_incremental_index(&db, &tp.content_index_path(), false).unwrap();

    let count_after = db.file_count().unwrap();
    assert!(
        count_after > count_before,
        "Incremental should add new files: before={}, after={}",
        count_before,
        count_after
    );

    // Content index should have docs (may be fewer than DB count since
    // directories and binary files don't get content-indexed)
    let cidx = ContentIndex::open_or_create(&tp.content_index_path()).unwrap();
    let doc_count = cidx.doc_count().unwrap();
    assert!(
        doc_count > 0,
        "Content index should have some docs after incremental, got {}",
        doc_count
    );
}

#[test]
fn test_run_incremental_index_detects_deleted_files() {
    let tp = TestPaths::new();
    create_test_files(
        tp.source(),
        &[("keep.txt", "keep this"), ("delete_me.txt", "delete this")],
    );

    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];

    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();
    db.set_meta("scan_paths", &scan_paths[0]).unwrap();

    let count_before = db.file_count().unwrap();

    // Delete a file
    fs::remove_file(tp.source().join("delete_me.txt")).unwrap();

    // Run incremental
    pipeline::run_incremental_index(&db, &tp.content_index_path(), false).unwrap();

    let count_after = db.file_count().unwrap();
    assert!(
        count_after < count_before,
        "Incremental should remove deleted files: before={}, after={}",
        count_before,
        count_after
    );
}

#[test]
fn test_run_incremental_index_no_changes() {
    let tp = TestPaths::new();
    create_test_files(tp.source(), &[("stable.txt", "no changes here")]);

    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];

    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();
    db.set_meta("scan_paths", &scan_paths[0]).unwrap();

    let count_before = db.file_count().unwrap();

    // Run incremental with no filesystem changes
    pipeline::run_incremental_index(&db, &tp.content_index_path(), false).unwrap();

    let count_after = db.file_count().unwrap();
    assert_eq!(
        count_before, count_after,
        "No changes should mean same file count"
    );
}

// ---------------------------------------------------------------------------
// 3. reconcile_if_needed
// ---------------------------------------------------------------------------

#[test]
fn test_reconcile_detects_drift_and_rebuilds() {
    let tp = TestPaths::new();
    create_test_files(
        tp.source(),
        &[
            ("one.txt", "file one"),
            ("two.txt", "file two"),
            ("three.txt", "file three"),
            ("four.txt", "file four"),
            ("five.txt", "file five"),
        ],
    );

    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];

    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();

    // Verify content_indexed_count was set during full index
    let stored = db.get_meta("content_indexed_count").unwrap();
    assert!(
        stored.is_some(),
        "content_indexed_count should exist after full index"
    );
    let expected_count: usize = stored.unwrap().parse().unwrap();
    assert!(expected_count >= 5, "Should have indexed at least 5 files");

    // Simulate drift: set content_indexed_count very high so Tantivy looks degraded
    // (tantivy_count < expected * 85 / 100 triggers rebuild)
    db.set_meta("content_indexed_count", "1000").unwrap();

    // Clear last_reconcile_check so it runs immediately
    // (set to a time more than 60 minutes ago)
    let old_time = chrono::Utc::now() - chrono::Duration::hours(2);
    db.set_meta("last_reconcile_check", &old_time.to_rfc3339())
        .unwrap();

    // Run reconcile - should detect drift and rebuild
    pipeline::reconcile_if_needed(&db, &tp.content_index_path());

    // After reconcile, content_indexed_count should be updated to actual count
    let new_stored = db.get_meta("content_indexed_count").unwrap().unwrap();
    let new_count: usize = new_stored.parse().unwrap();
    // It should no longer be 1000 - it should reflect actual file count
    assert!(
        new_count < 1000,
        "content_indexed_count should be corrected from 1000 to actual: {}",
        new_count
    );
    assert!(
        new_count >= 5,
        "Should still have at least 5 indexed files, got {}",
        new_count
    );
}

#[test]
fn test_reconcile_skips_when_recent() {
    let tp = TestPaths::new();
    create_test_files(tp.source(), &[("test.txt", "content")]);

    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];

    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();

    // Set last_reconcile_check to now (should skip)
    db.set_meta("last_reconcile_check", &chrono::Utc::now().to_rfc3339())
        .unwrap();

    // Artificially set a high count to trigger drift IF it runs
    db.set_meta("content_indexed_count", "9999").unwrap();

    // Run reconcile - should skip because last check was recent
    pipeline::reconcile_if_needed(&db, &tp.content_index_path());

    // Count should still be 9999 (reconcile didn't run)
    let stored = db.get_meta("content_indexed_count").unwrap().unwrap();
    assert_eq!(
        stored, "9999",
        "Reconcile should have skipped - count unchanged"
    );
}

// ---------------------------------------------------------------------------
// 4. run_ocr_incremental
// ---------------------------------------------------------------------------

#[test]
fn test_run_ocr_incremental_no_eligible_files() {
    let tp = TestPaths::new();
    // Only .txt files - not OCR eligible
    create_test_files(
        tp.source(),
        &[("readme.txt", "just text"), ("notes.md", "markdown notes")],
    );

    let parent_db_path = tp.data().join("parent.db");
    let parent_db = Database::open(&parent_db_path).unwrap();
    parent_db.init_schema().unwrap();

    let scan_paths = vec![tp.source().to_string_lossy().to_string()];

    pipeline::run_full_index(
        &parent_db,
        Some(&scan_paths),
        tp.data(),
        &tp.db_path(),
        &tp.content_index_path(),
        false,
    )
    .unwrap();

    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();

    let cidx = ContentIndex::open_or_create(&tp.content_index_path()).unwrap();

    // run_ocr_incremental should return 0 — either no OCR binary or no pending images
    let count = pipeline::run_ocr_incremental(&db, &cidx, false).unwrap();
    assert_eq!(count, 0, "Should return 0 for non-image files");
}

#[test]
fn test_run_ocr_incremental_returns_zero_without_ocr_binary() {
    let tp = TestPaths::new();

    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();

    let cidx = ContentIndex::open_or_create(&tp.content_index_path()).unwrap();

    // Without findr-ocr binary in PATH, should return 0 immediately
    let count = pipeline::run_ocr_incremental(&db, &cidx, false).unwrap();
    assert_eq!(count, 0, "Should return 0 when no OCR binary available");
}

// ---------------------------------------------------------------------------
// 5. rebuild_hnsw_index
// ---------------------------------------------------------------------------

#[test]
fn test_rebuild_hnsw_creates_index_files() {
    let tp = TestPaths::new();
    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();

    // Insert fake semantic vectors into DB
    let mut entries = Vec::new();
    for i in 0..10 {
        let vec: Vec<f32> = (0..EMBED_DIMS)
            .map(|d| ((d + i) as f32 * 0.01).sin())
            .collect();
        let bytes = semantic::vec_to_bytes(&vec);
        entries.push((
            format!("/fake/file_{}.txt", i),
            bytes,
            1700000000i64 + i as i64,
            "test_hash".to_string(),
        ));
    }
    db.upsert_semantic_vectors(&entries).unwrap();

    // Rebuild HNSW
    pipeline::rebuild_hnsw_index(&db, tp.data(), false);

    // Verify HNSW files exist on disk
    assert!(
        semantic::hnsw_index_exists(tp.data()),
        "HNSW index files should exist after rebuild"
    );

    // Verify hnsw_vector_count metadata was set
    let count = db.get_meta("hnsw_vector_count").unwrap();
    assert_eq!(
        count,
        Some("10".to_string()),
        "hnsw_vector_count should be 10"
    );
}

#[test]
fn test_rebuild_hnsw_noop_with_empty_vectors() {
    let tp = TestPaths::new();
    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();

    // No vectors in DB
    pipeline::rebuild_hnsw_index(&db, tp.data(), false);

    // HNSW files should NOT exist
    assert!(
        !semantic::hnsw_index_exists(tp.data()),
        "HNSW index should not be created when no vectors exist"
    );

    // hnsw_vector_count should not be set
    let count = db.get_meta("hnsw_vector_count").unwrap();
    assert_eq!(
        count, None,
        "hnsw_vector_count should not be set for empty vectors"
    );
}

#[test]
fn test_rebuild_hnsw_replaces_existing_index() {
    let tp = TestPaths::new();
    let db = Database::open(&tp.db_path()).unwrap();
    db.init_schema().unwrap();

    // Build with 5 vectors
    let mut entries = Vec::new();
    for i in 0..5 {
        let vec: Vec<f32> = (0..EMBED_DIMS)
            .map(|d| ((d + i) as f32 * 0.01).sin())
            .collect();
        let bytes = semantic::vec_to_bytes(&vec);
        entries.push((
            format!("/fake/file_{}.txt", i),
            bytes,
            1700000000i64 + i as i64,
            "hash_v1".to_string(),
        ));
    }
    db.upsert_semantic_vectors(&entries).unwrap();
    pipeline::rebuild_hnsw_index(&db, tp.data(), false);

    let count1 = db.get_meta("hnsw_vector_count").unwrap();
    assert_eq!(count1, Some("5".to_string()));

    // Add 5 more vectors and rebuild
    let mut more_entries = Vec::new();
    for i in 5..10 {
        let vec: Vec<f32> = (0..EMBED_DIMS)
            .map(|d| ((d + i) as f32 * 0.02).cos())
            .collect();
        let bytes = semantic::vec_to_bytes(&vec);
        more_entries.push((
            format!("/fake/file_{}.txt", i),
            bytes,
            1700000000i64 + i as i64,
            "hash_v2".to_string(),
        ));
    }
    db.upsert_semantic_vectors(&more_entries).unwrap();
    pipeline::rebuild_hnsw_index(&db, tp.data(), false);

    let count2 = db.get_meta("hnsw_vector_count").unwrap();
    assert_eq!(
        count2,
        Some("10".to_string()),
        "should reflect all 10 vectors after rebuild"
    );
    assert!(semantic::hnsw_index_exists(tp.data()));
}
