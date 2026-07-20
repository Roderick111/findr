use findr::db::Database;
use findr::indexer;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn setup() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();
    db.init_schema().unwrap();
    (dir, db)
}

/// Create N files in the given directory with distinct names and content.
fn create_files(dir: &Path, prefix: &str, count: usize) {
    for i in 0..count {
        let name = format!("{prefix}_{i}.txt");
        fs::write(dir.join(&name), format!("content of {name}")).unwrap();
    }
}

// ---------------------------------------------------------------------------
// build_index
// ---------------------------------------------------------------------------

#[test]
fn build_index_indexes_files_in_temp_dir() {
    let (dir, db) = setup();

    // Create a subdirectory with files
    let data_dir = dir.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "file", 10);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    let stats = indexer::build_index(&db, Some(&scan_paths), None).unwrap();

    // 10 files indexed (dirs don't count toward files_indexed in the stats,
    // but they ARE inserted into the DB as is_dir entries)
    assert!(
        stats.files_indexed >= 10,
        "expected >=10, got {}",
        stats.files_indexed
    );
    assert_eq!(stats.errors, 0);

    // Verify DB has the correct file paths
    let all = db.get_all_paths().unwrap();
    let file_paths: Vec<_> = all
        .iter()
        .filter(|r| r.path.contains("file_") && r.path.ends_with(".txt"))
        .collect();
    assert_eq!(file_paths.len(), 10);
}

#[test]
fn build_index_clears_previous_data() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("round1");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "old", 5);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();

    // Build again with different files — old ones should be gone (build_index calls clear)
    let data_dir2 = dir.path().join("round2");
    fs::create_dir(&data_dir2).unwrap();
    create_files(&data_dir2, "new", 3);

    let scan_paths2 = vec![data_dir2.to_string_lossy().to_string()];
    let stats = indexer::build_index(&db, Some(&scan_paths2), None).unwrap();

    // Only round2 files should exist
    let all = db.get_all_paths().unwrap();
    let old_files: Vec<_> = all.iter().filter(|r| r.path.contains("old_")).collect();
    assert_eq!(old_files.len(), 0, "old files should be cleared");

    let new_files: Vec<_> = all
        .iter()
        .filter(|r| r.path.contains("new_") && r.path.ends_with(".txt"))
        .collect();
    assert_eq!(new_files.len(), 3);
    assert!(stats.errors == 0);
}

#[test]
fn build_index_skips_nonexistent_scan_path() {
    let (dir, db) = setup();
    let bogus = dir.path().join("does_not_exist");
    let scan_paths = vec![bogus.to_string_lossy().to_string()];
    let stats = indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    assert_eq!(stats.files_indexed, 0);
}

#[test]
fn build_index_handles_nested_directories() {
    let (dir, db) = setup();
    let root = dir.path().join("root");
    let sub1 = root.join("sub1");
    let sub2 = root.join("sub1").join("sub2");
    fs::create_dir_all(&sub2).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(sub1.join("b.txt"), "b").unwrap();
    fs::write(sub2.join("c.txt"), "c").unwrap();

    let scan_paths = vec![root.to_string_lossy().to_string()];
    let stats = indexer::build_index(&db, Some(&scan_paths), None).unwrap();

    // At least 3 files
    let all = db.get_all_paths().unwrap();
    let txt_files: Vec<_> = all.iter().filter(|r| r.path.ends_with(".txt")).collect();
    assert_eq!(txt_files.len(), 3);
    assert!(stats.dirs_scanned >= 3); // root + sub1 + sub2
}

// ---------------------------------------------------------------------------
// compute_diff
// ---------------------------------------------------------------------------

#[test]
fn compute_diff_detects_new_file() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("diff_data");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "original", 3);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();

    // Store scan paths so compute_diff knows where to walk
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    // Add a new file
    fs::write(data_dir.join("brand_new.txt"), "I am new").unwrap();

    let diff = indexer::compute_diff(&db).unwrap();

    let new_names: Vec<_> = diff
        .new_files
        .iter()
        .filter(|f| !f.is_dir)
        .map(|f| f.filename.as_str())
        .collect();
    assert!(
        new_names.contains(&"brand_new.txt"),
        "new file not detected: {:?}",
        new_names
    );
    assert_eq!(diff.errors, 0);
}

#[test]
fn compute_diff_detects_deleted_file() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("diff_del");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "victim", 3);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    // Delete one file
    let deleted_path = data_dir.join("victim_1.txt");
    fs::remove_file(&deleted_path).unwrap();

    let diff = indexer::compute_diff(&db).unwrap();

    assert!(
        diff.deleted_paths
            .iter()
            .any(|p| p.contains("victim_1.txt")),
        "deleted file not detected: {:?}",
        diff.deleted_paths
    );
}

#[test]
fn compute_diff_detects_modified_file() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("diff_mod");
    fs::create_dir(&data_dir).unwrap();
    let target = data_dir.join("will_change.txt");
    fs::write(&target, "original content").unwrap();

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    // Force a future mtime so the diff picks it up
    let future = filetime::FileTime::from_unix_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 100,
        0,
    );
    filetime::set_file_mtime(&target, future).unwrap();

    let diff = indexer::compute_diff(&db).unwrap();

    let modified_names: Vec<_> = diff
        .modified_files
        .iter()
        .map(|f| f.filename.as_str())
        .collect();
    assert!(
        modified_names.contains(&"will_change.txt"),
        "modified file not detected: {:?}",
        modified_names
    );
}

#[test]
fn compute_diff_no_changes_returns_empty() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("diff_empty");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "stable", 3);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    let diff = indexer::compute_diff(&db).unwrap();

    let new_non_dir: Vec<_> = diff.new_files.iter().filter(|f| !f.is_dir).collect();
    assert_eq!(new_non_dir.len(), 0, "unexpected new files");
    assert_eq!(diff.modified_files.len(), 0, "unexpected modified files");
    assert_eq!(diff.deleted_paths.len(), 0, "unexpected deleted paths");
}

// ---------------------------------------------------------------------------
// quick_sync
// ---------------------------------------------------------------------------

#[test]
fn quick_sync_returns_empty_when_no_index() {
    let (_dir, db) = setup();
    // No files indexed at all — max_modified_ts returns 0
    let result = indexer::quick_sync(&db).unwrap();
    assert!(result.is_empty());
}

#[test]
fn quick_sync_returns_empty_when_no_changes() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("sync_stable");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "existing", 5);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    let result = indexer::quick_sync(&db).unwrap();
    assert!(result.is_empty(), "expected no changes, got {:?}", result);
}

#[test]
fn quick_sync_picks_up_new_file() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("sync_new");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "base", 3);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    // Add a new file with a future mtime so it's after max_modified_ts
    let new_file = data_dir.join("newcomer.txt");
    fs::write(&new_file, "hello").unwrap();
    let future = filetime::FileTime::from_unix_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 200,
        0,
    );
    filetime::set_file_mtime(&new_file, future).unwrap();

    let result = indexer::quick_sync(&db).unwrap();

    let names: Vec<_> = result.added.iter().map(|f| f.filename.as_str()).collect();
    assert!(
        names.contains(&"newcomer.txt"),
        "new file not detected: {:?}",
        names
    );
}

// ---------------------------------------------------------------------------
// index_single_path
// ---------------------------------------------------------------------------

#[test]
fn index_single_path_adds_to_existing_index() {
    let (dir, db) = setup();

    // Build index for dir A
    let dir_a = dir.path().join("dir_a");
    fs::create_dir(&dir_a).unwrap();
    create_files(&dir_a, "a_file", 4);

    let scan_paths = vec![dir_a.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();

    let count_before = db.file_count().unwrap();

    // Now additively index dir B
    let dir_b = dir.path().join("dir_b");
    fs::create_dir(&dir_b).unwrap();
    create_files(&dir_b, "b_file", 6);

    indexer::index_single_path(&db, &dir_b.to_string_lossy(), None).unwrap();

    let count_after = db.file_count().unwrap();
    assert!(
        count_after > count_before,
        "count didn't grow: before={count_before}, after={count_after}"
    );

    // Verify both sets of files exist
    let all = db.get_all_paths().unwrap();
    let a_count = all.iter().filter(|r| r.path.contains("a_file")).count();
    let b_count = all.iter().filter(|r| r.path.contains("b_file")).count();
    assert_eq!(a_count, 4, "dir_a files should still exist");
    assert_eq!(b_count, 6, "dir_b files should be added");
}

#[test]
fn index_single_path_errors_on_nonexistent_path() {
    let (dir, db) = setup();
    let bogus = dir.path().join("nope");
    let result = indexer::index_single_path(&db, &bogus.to_string_lossy(), None);
    assert!(result.is_err(), "expected error for nonexistent path");
}

#[test]
fn index_single_path_returns_correct_stats() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("stats_dir");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "stat", 7);
    // Add a subdirectory
    let sub = data_dir.join("sub");
    fs::create_dir(&sub).unwrap();
    create_files(&sub, "subsf", 2);

    let stats = indexer::index_single_path(&db, &data_dir.to_string_lossy(), None).unwrap();

    // 9 files total (7 + 2)
    assert!(
        stats.files_indexed >= 9,
        "expected >=9 files, got {}",
        stats.files_indexed
    );
    assert!(
        stats.dirs_scanned >= 2,
        "expected >=2 dirs, got {}",
        stats.dirs_scanned
    );
    assert_eq!(stats.errors, 0);
    assert!(stats.elapsed_ms < 10_000, "indexing took too long");
}

// ---------------------------------------------------------------------------
// scan_paths_for_preset
// ---------------------------------------------------------------------------

#[test]
fn scan_paths_for_preset_personal_returns_default_paths() {
    let paths = indexer::scan_paths_for_preset("personal", None);
    assert!(!paths.is_empty(), "personal preset should return paths");
    // Default paths include Documents, Desktop, Downloads (expanded from ~/...)
    let joined = paths.join("|");
    assert!(
        joined.contains("Documents"),
        "should include Documents: {}",
        joined
    );
    assert!(
        joined.contains("Desktop"),
        "should include Desktop: {}",
        joined
    );
    assert!(
        joined.contains("Downloads"),
        "should include Downloads: {}",
        joined
    );
    // Paths should be expanded (no tilde)
    for p in &paths {
        assert!(!p.starts_with("~/"), "path should be expanded, got: {}", p);
    }
}

#[test]
fn scan_paths_for_preset_full_home_returns_home_dir() {
    let paths = indexer::scan_paths_for_preset("full_home", None);
    assert_eq!(paths.len(), 1, "full_home should return exactly 1 path");
    // HOME on Unix, USERPROFILE on Windows
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .expect("HOME/USERPROFILE not set");
    assert_eq!(paths[0], home, "full_home path should be home dir");
}

#[test]
fn scan_paths_for_preset_everything_includes_home_and_volumes() {
    let paths = indexer::scan_paths_for_preset("everything", None);
    assert!(!paths.is_empty(), "everything preset should return paths");
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .expect("HOME/USERPROFILE not set");
    assert!(paths.contains(&home), "everything should include home dir");
}

#[test]
fn scan_paths_for_preset_unknown_falls_back_to_default() {
    let unknown = indexer::scan_paths_for_preset("nonexistent_preset", None);
    let default = indexer::scan_paths_for_preset("personal", None);
    assert_eq!(
        unknown, default,
        "unknown preset should fall back to default (same as personal)"
    );
}

#[test]
fn scan_paths_for_preset_custom_paths_merged_and_deduplicated() {
    let base = indexer::scan_paths_for_preset("personal", None);
    let custom = "/tmp/custom_test_path,/tmp/another_path";
    let merged = indexer::scan_paths_for_preset("personal", Some(custom));

    // Should have base paths + 2 custom
    assert_eq!(merged.len(), base.len() + 2, "should merge custom paths");
    assert!(merged.contains(&"/tmp/custom_test_path".to_string()));
    assert!(merged.contains(&"/tmp/another_path".to_string()));

    // Duplicates should be removed
    let with_dup = format!("{},{}", custom, "/tmp/custom_test_path");
    let deduped = indexer::scan_paths_for_preset("personal", Some(&with_dup));
    assert_eq!(
        deduped.len(),
        base.len() + 2,
        "duplicates should be removed"
    );
}

#[test]
fn scan_paths_for_preset_custom_empty_string_ignored() {
    let base = indexer::scan_paths_for_preset("personal", None);
    let with_empty = indexer::scan_paths_for_preset("personal", Some(""));
    assert_eq!(base, with_empty, "empty custom string should not add paths");
}

#[test]
fn scan_paths_for_preset_custom_whitespace_trimmed() {
    let merged =
        indexer::scan_paths_for_preset("personal", Some("  /tmp/spaced  , /tmp/also_spaced  "));
    assert!(
        merged.contains(&"/tmp/spaced".to_string()),
        "should trim whitespace"
    );
    assert!(
        merged.contains(&"/tmp/also_spaced".to_string()),
        "should trim whitespace"
    );
}

#[test]
fn scan_paths_for_preset_accepts_json_custom_paths_with_commas() {
    let custom = serde_json::json!(["/tmp/reports,2026", "/tmp/extra"]).to_string();
    let merged = indexer::scan_paths_for_preset("personal", Some(&custom));
    assert!(merged.contains(&"/tmp/reports,2026".to_string()));
    assert!(merged.contains(&"/tmp/extra".to_string()));
}

#[test]
fn stored_custom_paths_preserves_json_paths() {
    let (_dir, db) = setup();
    db.set_meta(
        "custom_paths",
        &serde_json::json!(["/tmp/reports,2026", "/tmp/extra"]).to_string(),
    )
    .unwrap();
    assert_eq!(
        indexer::stored_custom_paths(&db),
        vec!["/tmp/reports,2026".to_string(), "/tmp/extra".to_string()]
    );
}

#[test]
fn stored_custom_paths_migrates_legacy_effective_paths() {
    let (_dir, db) = setup();
    let mut effective = indexer::scan_paths_for_preset("personal", None);
    effective.push("/tmp/legacy-extra".to_string());
    db.set_meta("scan_preset", "personal").unwrap();
    db.set_meta("scan_paths", &serde_json::to_string(&effective).unwrap())
        .unwrap();
    assert_eq!(
        indexer::stored_custom_paths(&db),
        vec!["/tmp/legacy-extra".to_string()]
    );
}

#[test]
fn quick_sync_detects_modified_file_outside_hot_folders() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("sync_mod");
    fs::create_dir(&data_dir).unwrap();
    let target = data_dir.join("deep_change.txt");
    fs::write(&target, "original").unwrap();

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    let future = filetime::FileTime::from_unix_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 150,
        0,
    );
    filetime::set_file_mtime(&target, future).unwrap();

    let result = indexer::quick_sync(&db).unwrap();
    let modified_names: Vec<_> = result
        .modified
        .iter()
        .map(|f| f.filename.as_str())
        .collect();
    assert!(
        modified_names.contains(&"deep_change.txt"),
        "modified file outside hot folders not detected: {:?}",
        modified_names
    );
}

#[test]
fn quick_sync_detects_deleted_file() {
    let (dir, db) = setup();

    let data_dir = dir.path().join("sync_del");
    fs::create_dir(&data_dir).unwrap();
    create_files(&data_dir, "gone", 2);

    let scan_paths = vec![data_dir.to_string_lossy().to_string()];
    indexer::build_index(&db, Some(&scan_paths), None).unwrap();
    db.set_meta("scan_paths", &data_dir.to_string_lossy())
        .unwrap();

    fs::remove_file(data_dir.join("gone_0.txt")).unwrap();

    let result = indexer::quick_sync(&db).unwrap();
    assert!(
        result.deleted.iter().any(|p| p.ends_with("gone_0.txt")),
        "deleted file not detected: {:?}",
        result.deleted
    );
}
