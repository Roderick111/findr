//! Indexing pipeline functions extracted from main.rs.
//!
//! These are the core business-logic functions for indexing, OCR, embedding,
//! and sync. They were extracted to enable unit testing and reduce main.rs complexity.

use crate::content;
use crate::db;
use crate::errors;
#[cfg(target_os = "macos")]
use crate::fsevents;
use crate::indexer;
use crate::semantic;

use anyhow::Result;
use std::path::Path;

/// Run OCR on pending images. Uses parallel batch mode.
pub fn run_ocr_incremental(
    db: &db::Database,
    cidx: &content::ContentIndex,
    verbose: bool,
) -> Result<usize> {
    if !content::ocr_available() {
        return Ok(0);
    }

    let pending = db.get_pending_ocr_paths(content::OCR_EXTENSIONS)?;
    if pending.is_empty() {
        return Ok(0);
    }

    if verbose {
        eprintln!("  OCR: {} new images to process...", pending.len());
    }

    let paths: Vec<std::path::PathBuf> = pending
        .iter()
        .map(|(p, _)| std::path::PathBuf::from(p))
        .collect();
    let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();

    let results = content::extract_ocr_batch(&path_refs);

    let mtime_map: std::collections::HashMap<String, i64> = pending.into_iter().collect();
    let mut ocr_marks: Vec<(String, i64, f64)> = Vec::new();
    let mut indexed_count = 0;

    // Collect Tantivy updates with pre-extracted content (avoid re-running OCR)
    let mut tantivy_updates: Vec<(String, String, Option<String>, String)> = Vec::new();

    for (path, text, confidence) in &results {
        let path_str = path.to_string_lossy().to_string();
        let mtime = mtime_map.get(&path_str).copied().unwrap_or(0);

        if !text.is_empty() {
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let ext = path.extension().map(|e| e.to_string_lossy().to_string());
            tantivy_updates.push((path_str.clone(), filename, ext, text.clone()));
            indexed_count += 1;
        }

        ocr_marks.push((path_str, mtime, *confidence));
    }

    // Only mark as done AFTER successful Tantivy write
    if !tantivy_updates.is_empty() {
        match cidx.update_files_with_content(&tantivy_updates) {
            Ok(_) => {
                if !ocr_marks.is_empty() {
                    db.mark_ocr_done_batch(&ocr_marks)?;
                }
            }
            Err(e) => {
                errors::log_error(
                    "ocr:tantivy",
                    &format!("Failed to write OCR content: {}", e),
                );
                // Don't mark as done — will retry next run
            }
        }
    } else if !ocr_marks.is_empty() {
        // No content to write (all low confidence) — still mark as done to avoid retry
        db.mark_ocr_done_batch(&ocr_marks)?;
    }

    Ok(indexed_count)
}

/// Get query embedding vector: cache first (instant), then API with 3s timeout.
/// Returns None if both cache miss and API fails/times out.
pub fn get_query_vector(db: &db::Database, api_key: &str, query: &str) -> Option<Vec<f32>> {
    // Check cache first (instant)
    if let Some(cached_bytes) = db.get_cached_query_vector(query) {
        return semantic::bytes_to_vec(&cached_bytes);
    }

    // API call with 3s timeout (no retries)
    match semantic::embed_query(api_key, query) {
        Ok(vec) => {
            let bytes = semantic::vec_to_bytes(&vec);
            db.cache_query_vector(query, &bytes);
            Some(vec)
        }
        Err(_) => None,
    }
}

/// Embed pending files in batches. Mirrors run_ocr_incremental pattern.
pub fn run_embed_batch(db: &db::Database, api_key: &str, verbose: bool) -> Result<usize> {
    let pending = db.get_pending_embed_paths(semantic::EMBEDDABLE_EXTENSIONS)?;
    if pending.is_empty() {
        return Ok(0);
    }

    let total = pending.len();
    let total_batches = total.div_ceil(semantic::API_BATCH_SIZE);
    if verbose {
        eprintln!(
            "  Semantic: {} files to embed ({} batches)...",
            total, total_batches
        );
    }

    let mut embedded = 0;
    let mut batch_num = 0;

    for chunk in pending.chunks(semantic::API_BATCH_SIZE) {
        batch_num += 1;
        let mut texts: Vec<String> = Vec::new();
        let mut meta: Vec<(String, i64, String)> = Vec::new(); // (path, mtime, hash)

        for f in chunk {
            let ext_str = f.extension.as_deref().unwrap_or("");

            // Read file content
            let content = match semantic::read_file_for_embed(&f.path, ext_str) {
                Some(c) => c,
                None => continue,
            };

            // Build embed text (format-specific)
            let embed_text = match semantic::build_embed_text(&f.filename, &content, ext_str) {
                Some(t) => t,
                None => continue,
            };

            // Check hash — skip if content unchanged
            let hash = semantic::embed_hash(&embed_text);
            if let Ok(Some(old_hash)) = db.get_embed_hash(&f.path) {
                if old_hash == hash {
                    // Content unchanged — update mtime so it's no longer "pending"
                    let _ = db.update_semantic_mtime(&f.path, f.modified_ts);
                    continue;
                }
            }

            texts.push(embed_text);
            meta.push((f.path.clone(), f.modified_ts, hash));
        }

        if texts.is_empty() {
            continue;
        }

        match semantic::embed_texts(api_key, &texts) {
            Ok(vectors) => {
                let entries: Vec<(String, Vec<u8>, i64, String)> = vectors
                    .iter()
                    .zip(meta.iter())
                    .map(|(vec, (path, mtime, hash))| {
                        (
                            path.clone(),
                            semantic::vec_to_bytes(vec),
                            *mtime,
                            hash.clone(),
                        )
                    })
                    .collect();
                if let Err(e) = db.upsert_semantic_vectors(&entries) {
                    errors::log_error("embed:db", &format!("{}", e));
                } else {
                    embedded += entries.len();
                    if verbose {
                        eprintln!(
                            "    Batch {}/{}: {} / {} embedded",
                            batch_num, total_batches, embedded, total
                        );
                    }
                }
            }
            Err(e) => {
                errors::log_error("embed:api", &format!("{}", e));
                // Continue with next batch
            }
        }
    }

    Ok(embedded)
}

/// Rebuild HNSW index from all vectors in SQLite. Called after embedding completes.
/// build_and_save_hnsw uses atomic temp-dir swap, so no pre-delete needed.
pub fn rebuild_hnsw_index(db: &db::Database, data_dir: &Path, verbose: bool) {
    let raw_vecs = match db.load_all_vectors() {
        Ok(v) => v,
        Err(e) => {
            errors::log_error("hnsw:load_vectors", &format!("{}", e));
            if verbose {
                eprintln!("  Warning: could not load vectors for HNSW: {}", e);
            }
            return;
        }
    };
    if raw_vecs.is_empty() {
        return;
    }

    let vectors: Vec<(String, Vec<f32>)> = raw_vecs
        .into_iter()
        .filter_map(|(path, bytes)| semantic::bytes_to_vec(&bytes).map(|v| (path, v)))
        .collect();

    if vectors.is_empty() {
        return;
    }

    match semantic::build_and_save_hnsw(&vectors, data_dir) {
        Ok(()) => {
            // Store vector count for staleness detection
            let _ = db.set_meta("hnsw_vector_count", &vectors.len().to_string());
            if verbose {
                eprintln!("  HNSW index rebuilt: {} vectors", vectors.len());
            }
        }
        Err(e) => {
            errors::log_error("hnsw:build", &format!("{}", e));
            if verbose {
                eprintln!(
                    "  Warning: HNSW index build failed: {}. Brute-force fallback active.",
                    e
                );
            }
        }
    }
}

/// Apply filesystem changes to Tantivy first, then SQLite.
/// Returns (change_count, dual_store_ok).
fn apply_dual_store_changes(
    db: &db::Database,
    content_idx_path: &Path,
    added: &[db::FileEntry],
    modified: &[db::FileEntry],
    deleted: &[String],
) -> (usize, bool) {
    let total = added.len() + modified.len() + deleted.len();
    if total == 0 {
        return (0, true);
    }

    let cidx = match content::ContentIndex::open_or_create(content_idx_path) {
        Ok(c) => c,
        Err(e) => {
            errors::log_error("sync:tantivy:open", &format!("{}", e));
            return (0, false);
        }
    };

    let changed = added
        .iter()
        .chain(modified.iter())
        .map(|f| (f.path.clone(), f.filename.clone(), f.extension.clone()))
        .collect::<Vec<_>>();

    if !changed.is_empty() {
        if let Err(e) = cidx.update_files(&changed) {
            errors::log_error("sync:tantivy:update", &format!("{}", e));
            return (0, false);
        }
    }
    if !deleted.is_empty() {
        if let Err(e) = cidx.delete_files(deleted) {
            errors::log_error("sync:tantivy:delete", &format!("{}", e));
            return (0, false);
        }
    }

    let mut sqlite_ok = true;
    if !added.is_empty() {
        if let Err(e) = db.insert_files_batch(added) {
            errors::log_error("sync:sqlite:insert", &format!("{}", e));
            sqlite_ok = false;
        }
    }
    if !modified.is_empty() {
        if let Err(e) = db.insert_files_batch(modified) {
            errors::log_error("sync:sqlite:update", &format!("{}", e));
            sqlite_ok = false;
        }
    }
    if !deleted.is_empty() {
        if let Err(e) = db.delete_paths_batch(deleted) {
            errors::log_error("sync:sqlite:delete", &format!("{}", e));
            sqlite_ok = false;
        }
    }

    if !sqlite_ok {
        errors::log_error(
            "sync:compensate",
            "SQLite apply failed after Tantivy success — forcing reconcile",
        );
        force_reconcile(db, content_idx_path);
    }

    if let Ok(actual_count) = cidx.doc_count() {
        let _ = db.set_meta("content_indexed_count", &actual_count.to_string());
    }
    let _ = db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339());

    (total, sqlite_ok)
}

fn force_reconcile(db: &db::Database, content_idx_path: &Path) {
    let old_time = chrono::Utc::now() - chrono::Duration::hours(2);
    let _ = db.set_meta("last_reconcile_check", &old_time.to_rfc3339());
    reconcile_if_needed(db, content_idx_path);
}

/// Layer 1: Quick diff — find new/modified/deleted files, apply dual-store sync.
pub fn incremental_sync(db: &db::Database, content_idx_path: &Path) -> usize {
    let sync_result = match indexer::quick_sync(db) {
        Ok(r) => r,
        Err(e) => {
            errors::log_error("quick_sync", &format!("{}", e));
            return 0;
        }
    };

    if sync_result.is_empty() {
        return 0;
    }

    let (count, _) = apply_dual_store_changes(
        db,
        content_idx_path,
        &sync_result.added,
        &sync_result.modified,
        &sync_result.deleted,
    );
    count
}

/// Detect SQLite/Tantivy drift and trigger targeted re-index if diverged.
/// Compares Tantivy doc count against the actual count stored after last indexing,
/// NOT a computed estimate (which was wrong — it didn't account for files with
/// no extractable content like binaries, empty files, failed extractions).
/// Runs at most once per hour to avoid redundant Tantivy opens on every search.
pub fn reconcile_if_needed(db: &db::Database, content_idx_path: &Path) {
    // Only check every hour — avoid opening Tantivy on every search
    if let Ok(Some(last_check)) = db.get_meta("last_reconcile_check") {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&last_check) {
            let age = chrono::Utc::now().signed_duration_since(dt);
            if age.num_minutes() < 60 {
                return;
            }
        }
    }
    let _ = db.set_meta("last_reconcile_check", &chrono::Utc::now().to_rfc3339());

    // Use actual stored content count from last indexing (set by run_full_index).
    // Falls back to old heuristic only if metadata is missing (pre-upgrade DBs).
    let expected_tantivy = match db.get_meta("content_indexed_count") {
        Ok(Some(s)) => s.parse::<usize>().unwrap_or(0),
        _ => {
            // Fallback for DBs without the new metadata key
            let sqlite_count = db.file_count().unwrap_or(0);
            let (ocr_total, ocr_done) = db.ocr_stats(content::OCR_EXTENSIONS).unwrap_or((0, 0));
            let ocr_pending = ocr_total.saturating_sub(ocr_done);
            sqlite_count.saturating_sub(ocr_pending)
        }
    };

    if expected_tantivy == 0 {
        return;
    }

    let tantivy_count = content::ContentIndex::open_or_create(content_idx_path)
        .and_then(|c| c.doc_count())
        .unwrap_or(0) as usize;

    if tantivy_count < expected_tantivy * 85 / 100 {
        eprintln!(
            "Warning: content index degraded ({} docs, expected ~{}). Rebuilding...",
            tantivy_count, expected_tantivy
        );
        errors::log_error(
            "reconcile",
            &format!(
                "Content index drift: expected ~{}, found {}. Triggering re-index.",
                expected_tantivy, tantivy_count
            ),
        );
        if let Ok(cidx) = content::ContentIndex::open_or_create(content_idx_path) {
            let all_files: Vec<(String, String, Option<String>)> = db
                .get_all_paths()
                .unwrap_or_default()
                .into_iter()
                .map(|f| (f.path, f.filename, f.extension))
                .collect();
            match cidx.index_files(&all_files) {
                Ok(count) => {
                    let _ = db.set_meta("content_indexed_count", &count.to_string());
                    eprintln!("  Rebuilt content index: {} files", count);
                }
                Err(e) => {
                    eprintln!("Error: content index rebuild failed: {}", e);
                    errors::log_error("reconcile:tantivy", &format!("Re-index failed: {}", e));
                }
            }
        }
    }
}

/// Check schema version. If outdated, trigger a background full rebuild
/// so new features (like directory indexing) get picked up.
/// Returns true if a rebuild was triggered (caller should spawn background rebuild).
pub fn check_schema_version(db: &db::Database, content_index_path: &Path) -> bool {
    let version = db
        .get_meta("schema_version")
        .unwrap_or(None)
        .unwrap_or_default();
    if version != "3" {
        if db.set_meta("schema_version", "3").is_ok() {
            // Delete content index (may have stale schema)
            if content_index_path.exists() {
                let _ = std::fs::remove_dir_all(content_index_path);
            }
            eprintln!("Schema upgraded to v3. Rebuilding index in background...");
            return true; // Caller should spawn background rebuild
        } else {
            errors::log_error(
                "schema",
                "Failed to set schema_version — skipping migration",
            );
        }
    }
    false
}

/// Incremental reindex: diff filesystem against index, apply only changes.
pub fn run_incremental_index(
    db: &db::Database,
    content_index_path: &Path,
    verbose: bool,
) -> Result<()> {
    db.init_schema()?;

    if verbose {
        eprintln!("Incremental sync: computing diff...");
    }
    let diff = indexer::compute_diff(db)?;

    let total_changes = diff.new_files.len() + diff.modified_files.len() + diff.deleted_paths.len();
    if verbose {
        eprintln!(
            "  {} new, {} modified, {} deleted ({}ms, {} dirs, {} errors)",
            diff.new_files.len(),
            diff.modified_files.len(),
            diff.deleted_paths.len(),
            diff.elapsed_ms,
            diff.dirs_scanned,
            diff.errors,
        );
    }

    if total_changes == 0 {
        if verbose {
            eprintln!("  No changes. Index is up to date.");
        }
        db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
        return Ok(());
    }

    // Apply to Tantivy first (delete-by-term + re-add for changed, delete for removed).
    let cidx = content::ContentIndex::open_or_create(content_index_path)?;

    let changed_files: Vec<(String, String, Option<String>)> = diff
        .new_files
        .iter()
        .chain(diff.modified_files.iter())
        .map(|f| (f.path.clone(), f.filename.clone(), f.extension.clone()))
        .collect();

    if !changed_files.is_empty() {
        let count = cidx.update_files(&changed_files)?;
        if verbose {
            eprintln!("  Content indexed: {} files", count);
        }
    }
    if !diff.deleted_paths.is_empty() {
        cidx.delete_files(&diff.deleted_paths)?;
        if verbose {
            eprintln!("  Content deleted: {} files", diff.deleted_paths.len());
        }
    }

    // Apply to SQLite after Tantivy succeeds
    if !diff.new_files.is_empty() {
        db.insert_files_batch(&diff.new_files)?;
    }
    if !diff.modified_files.is_empty() {
        db.insert_files_batch(&diff.modified_files)?;
    }
    if !diff.deleted_paths.is_empty() {
        db.delete_paths_batch(&diff.deleted_paths)?;
    }

    // OCR any new/modified images
    let ocr_count = run_ocr_incremental(db, &cidx, verbose)?;
    if verbose && ocr_count > 0 {
        eprintln!("  OCR indexed: {} images", ocr_count);
    }

    // Update stored content count so reconcile_if_needed doesn't trigger
    // unnecessary full re-indexes due to stale metadata.
    let actual_count = cidx.doc_count().unwrap_or(0);
    db.set_meta("content_indexed_count", &actual_count.to_string())?;

    db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
    db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339())?;

    if verbose {
        eprintln!("  Sync complete.");
    }
    Ok(())
}

/// Result from run_full_index indicating what background tasks should be spawned.
pub struct FullIndexResult {
    /// Whether OCR background process should be spawned
    pub spawn_ocr: bool,
    /// Whether embedding background process should be spawned
    pub spawn_embed: bool,
}

/// Run full index (paths + content) using double-buffer for atomicity.
/// Builds into temp files, then swaps atomically on success.
/// Text files indexed in parallel (Phase 2). Returns signals for background tasks
/// (OCR, embedding) — caller is responsible for spawning them.
pub fn run_full_index(
    parent_db: &db::Database,
    scan_paths: Option<&[String]>,
    data_dir: &Path,
    db_path: &Path,
    content_index_path: &Path,
    verbose: bool,
) -> Result<FullIndexResult> {
    let temp_db_path = data_dir.join("index.db.new");
    let temp_content_path = data_dir.join("content_index.new");

    // Clean up any leftover temp files from a previous failed run
    let _ = std::fs::remove_file(&temp_db_path);
    let _ = std::fs::remove_dir_all(&temp_content_path);

    // Build into temp locations
    let temp_db = db::Database::open(&temp_db_path)?;
    temp_db.init_schema()?;

    if verbose {
        eprintln!("Phase 1: Indexing file paths...");
    }
    // Read scan preset from the main DB (stored by resolve_scan_paths before this call)
    let preset = parent_db.get_meta("scan_preset").ok().flatten();
    let stats = indexer::build_index(&temp_db, scan_paths, preset.as_deref())?;
    if verbose {
        eprintln!(
            "  {} files indexed, {} dirs scanned, {} errors in {}ms",
            stats.files_indexed, stats.dirs_scanned, stats.errors, stats.elapsed_ms,
        );
        eprintln!("\nPhase 2: Indexing file contents (parallel, PDF warnings are harmless)...");
    }

    let all_files: Vec<(String, String, Option<String>)> = temp_db
        .get_all_paths()?
        .into_iter()
        .map(|f| (f.path, f.filename, f.extension))
        .collect();

    let temp_cidx = content::ContentIndex::open_or_create(&temp_content_path)?;
    let content_count = temp_cidx.index_files(&all_files)?;
    if verbose {
        eprintln!("  {} files with content indexed", content_count);
    }

    // Store actual content count — used by reconcile to detect real drift
    temp_db.set_meta("content_indexed_count", &content_count.to_string())?;

    temp_db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
    temp_db.set_meta("schema_version", "3")?;
    // A full rebuild creates a new SQLite generation with no semantic vectors.
    // Never let the previous generation's HNSW graph survive beside it.
    temp_db.set_meta("hnsw_vector_count", "0")?;

    // Carry scan config into new DB so future syncs use the same paths.
    // scan_paths passed as argument is authoritative; also copy preset/custom from parent.
    if let Some(paths) = scan_paths {
        temp_db.set_meta("scan_paths", &serde_json::to_string(paths)?)?;
    }
    for key in &["scan_preset", "custom_paths"] {
        if let Ok(Some(val)) = parent_db.get_meta(key) {
            let _ = temp_db.set_meta(key, &val);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let event_id = fsevents::current_event_id();
        temp_db.set_meta("fsevent_last_id", &event_id.to_string())?;
    }

    // Drop handles before rename
    drop(temp_cidx);
    drop(temp_db);

    // Invalidate semantic files before exposing the new DB/content pair. This
    // is conservative on rebuild failure, but prevents stale paths from being
    // returned against the new generation.
    semantic::delete_hnsw_index(data_dir);

    // Atomic swap — rename is atomic on same filesystem (POSIX guarantee)
    let generation = format!(
        "{}.{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let bak_db = data_dir.join(format!("index.db.bak.{generation}"));
    let bak_content = data_dir.join(format!("content_index.bak.{generation}"));

    // Move old to backup (may not exist on first run — that's fine)
    let had_db = db_path.exists();
    let had_content = content_index_path.exists();
    if had_db {
        std::fs::rename(db_path, &bak_db)
            .map_err(|e| anyhow::anyhow!("Failed to backup database: {e}"))?;
    }
    if had_content {
        if let Err(e) = std::fs::rename(content_index_path, &bak_content) {
            if had_db {
                let _ = std::fs::rename(&bak_db, db_path);
            }
            return Err(anyhow::anyhow!("Failed to backup content index: {e}"));
        }
    }

    // Move new to active
    if let Err(e) = std::fs::rename(&temp_db_path, db_path) {
        // Rollback: restore old (only if backup existed)
        if had_db {
            if let Err(re) = std::fs::rename(&bak_db, db_path) {
                errors::log_error(
                    "rollback",
                    &format!("CRITICAL: failed to restore DB backup: {}", re),
                );
            }
        }
        if had_content {
            if let Err(re) = std::fs::rename(&bak_content, content_index_path) {
                errors::log_error(
                    "rollback",
                    &format!("CRITICAL: failed to restore content backup: {}", re),
                );
            }
        }
        return Err(anyhow::anyhow!("Failed to swap index: {}", e));
    }
    if let Err(e) = std::fs::rename(&temp_content_path, content_index_path) {
        // Full rollback — restore both (only if backups existed)
        if had_db {
            let _ = std::fs::remove_file(db_path);
            if let Err(re) = std::fs::rename(&bak_db, db_path) {
                errors::log_error(
                    "rollback",
                    &format!("CRITICAL: failed to restore DB backup: {}", re),
                );
            }
        }
        if had_content {
            let _ = std::fs::remove_dir_all(content_index_path);
            if let Err(re) = std::fs::rename(&bak_content, content_index_path) {
                errors::log_error(
                    "rollback",
                    &format!("CRITICAL: failed to restore content backup: {}", re),
                );
            }
        }
        return Err(anyhow::anyhow!("Failed to swap content index: {}", e));
    }

    // Cleanup backups
    let _ = std::fs::remove_file(&bak_db);
    let _ = std::fs::remove_dir_all(&bak_content);

    if verbose {
        eprintln!("\nDone. Ready to search.");
    }

    // Phase 3 & 4: Check what background tasks are needed
    let new_db = db::Database::open(db_path)?;
    new_db.init_schema()?;

    let spawn_ocr = if content::ocr_available() {
        let pending = new_db.get_pending_ocr_paths(content::OCR_EXTENSIONS)?;
        if !pending.is_empty() {
            if verbose {
                eprintln!(
                    "  OCR: {} images queued for background processing...",
                    pending.len()
                );
            }
            true
        } else {
            false
        }
    } else {
        false
    };

    let spawn_embed = if semantic::get_api_key().is_some() {
        let embed_pending = new_db.get_pending_embed_paths(semantic::EMBEDDABLE_EXTENSIONS)?;
        if !embed_pending.is_empty() {
            if verbose {
                eprintln!(
                    "  Semantic: {} files queued for background embedding...",
                    embed_pending.len()
                );
            }
            true
        } else {
            false
        }
    } else {
        false
    };

    Ok(FullIndexResult {
        spawn_ocr,
        spawn_embed,
    })
}

#[cfg(target_os = "macos")]
pub fn fsevents_sync(db: &db::Database, content_idx_path: &Path) -> usize {
    let last_id: u64 = db
        .get_meta("fsevent_last_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let scan_paths = indexer::stored_or_default_paths(db);
    let result = match fsevents::get_changes_since(last_id, &scan_paths) {
        Some(r) => r,
        None => {
            // No stored event ID or journal unavailable — fallback
            return incremental_sync(db, content_idx_path);
        }
    };

    // If FSEvents replay was incomplete (timeout before HistoryDone),
    // fall back to a comprehensive sync. Never checkpoint an incomplete
    // replay: its cursor is not a durable statement of filesystem coverage.
    if !result.complete {
        errors::log_error("fsevents", "Incomplete replay — falling back to quick_sync");
        return incremental_sync(db, content_idx_path);
    }

    if result.changes.is_empty() {
        // No writes were needed, so acknowledging the fully replayed cursor is safe.
        let _ = db.set_meta("fsevent_last_id", &result.new_event_id.to_string());
        return 0;
    }

    // Process FSEvents into file entries and deletions
    let preset = db.get_meta("scan_preset").ok().flatten();
    let preset_ref = preset.as_deref();
    let (to_update, to_delete) = indexer::process_fsevents(&result, preset_ref);

    let total = to_update.len() + to_delete.len();
    if total == 0 {
        return 0;
    }

    // Apply to Tantivy first — extra docs on crash are harmless (reconcile cleans up).
    let mut apply_ok = true;
    if let Ok(cidx) = content::ContentIndex::open_or_create(content_idx_path) {
        if !to_update.is_empty() {
            let update_tuples: Vec<_> = to_update
                .iter()
                .map(|f| (f.path.clone(), f.filename.clone(), f.extension.clone()))
                .collect();
            if let Err(e) = cidx.update_files(&update_tuples) {
                errors::log_error("fsevents:tantivy", &format!("update_files: {}", e));
                apply_ok = false;
            }
        }
        if !to_delete.is_empty() {
            if let Err(e) = cidx.delete_files(&to_delete) {
                errors::log_error("fsevents:tantivy", &format!("delete_files: {}", e));
                apply_ok = false;
            }
        }

        // OCR any new images detected via FSEvents
        if let Err(e) = run_ocr_incremental(db, &cidx, false) {
            errors::log_error("fsevents:ocr", &format!("{}", e));
            apply_ok = false;
        }
    } else {
        apply_ok = false;
    }

    // Apply to SQLite after Tantivy
    if !to_update.is_empty() {
        if let Err(e) = db.insert_files_batch(&to_update) {
            errors::log_error("fsevents:sqlite", &format!("insert_files_batch: {}", e));
            apply_ok = false;
        }
    }
    if !to_delete.is_empty() {
        if let Err(e) = db.delete_paths_batch(&to_delete) {
            errors::log_error("fsevents:sqlite", &format!("delete_paths_batch: {}", e));
            apply_ok = false;
        }
    }

    if !apply_ok {
        // Keep the old cursor so the next run replays these events or falls
        // back to a complete diff. A failed apply must never look complete.
        return 0;
    }

    // Invalidate semantic vectors for changed files (background will re-embed)
    if semantic::get_api_key().is_some() && !to_update.is_empty() {
        let update_paths: Vec<String> = to_update.iter().map(|f| f.path.clone()).collect();
        let _ = db.delete_semantic_paths(&update_paths);
    }

    if let Err(e) = db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339()) {
        errors::log_error("fsevents:meta", &format!("{}", e));
        return 0;
    }
    let _ = db.set_meta("fsevent_last_id", &result.new_event_id.to_string());
    total
}
