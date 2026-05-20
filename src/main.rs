mod content;
mod db;
mod errors;
mod fsevents;
mod indexer;
mod search;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs::File;
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(home).join(".findr");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("Warning: failed to create {}: {}", dir.display(), e);
    }
    // Restrict permissions to owner only (0700) — DB contains file inventory
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// Try to acquire an exclusive lock on ~/.findr/sync.lock.
/// Returns the File handle (holds lock until dropped), or None if already locked.
fn try_acquire_lock() -> Option<File> {
    let lock_path = data_dir().join("sync.lock");
    let file = File::create(&lock_path).ok()?;
    use std::os::unix::io::AsRawFd;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 { Some(file) } else { None }
}

fn db_path() -> PathBuf {
    data_dir().join("index.db")
}

fn content_index_path() -> PathBuf {
    data_dir().join("content_index")
}

#[derive(Parser)]
#[command(name = "findr", version, about = "The fastest local file search for macOS")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Search for files (searches both filenames and content)
    Search {
        /// Search query
        query: String,

        /// Filter by file type (e.g., pdf, png, rs)
        #[arg(long, short = 't')]
        r#type: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Max results
        #[arg(long, default_value = "30")]
        limit: usize,
    },
    /// Manage the file index
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Run diagnostics and output a health report (JSON with --json)
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum IndexAction {
    /// Build index from scratch
    Init {
        /// Specific paths to scan (comma-separated)
        #[arg(long)]
        paths: Option<String>,
    },
    /// Show index status
    Status,
    /// Rebuild entire index (full nuke + rebuild)
    Rebuild {
        /// Specific paths to scan (comma-separated)
        #[arg(long)]
        paths: Option<String>,
    },
    /// Incremental sync (diff-based, only processes changes)
    Sync,
    /// Run OCR indexing on pending images (usually called as background process)
    Ocr,
}

/// Check schema version. If content index was built with old schema (STORED content),
/// delete it so reconciliation or next sync rebuilds it with the new schema.
fn check_schema_version(db: &db::Database) {
    let version = db.get_meta("schema_version").unwrap_or(None).unwrap_or_default();
    if version != "2" {
        // Try to set version first — only delete if we can record the migration
        if db.set_meta("schema_version", "2").is_ok() {
            let cidx_path = content_index_path();
            if cidx_path.exists() {
                let _ = std::fs::remove_dir_all(&cidx_path);
            }
        } else {
            errors::log_error("schema", "Failed to set schema_version — skipping migration");
        }
    }
}

/// Detect SQLite/Tantivy drift and trigger targeted re-index if diverged.
/// Only runs if counts diverge by >10% — avoids false positives from OCR images
/// (which are in SQLite but not always in Tantivy if OCR is still running).
fn reconcile_if_needed(db: &db::Database, content_idx_path: &std::path::Path) {
    let sqlite_count = db.file_count().unwrap_or(0);
    if sqlite_count == 0 { return; }

    let tantivy_count = content::ContentIndex::open_or_create(content_idx_path)
        .and_then(|c| c.doc_count())
        .unwrap_or(0) as usize;

    // OCR images are in SQLite but may not be in Tantivy yet — subtract them
    let (ocr_total, ocr_done) = db.ocr_stats(content::OCR_EXTENSIONS).unwrap_or((0, 0));
    let ocr_pending = ocr_total.saturating_sub(ocr_done);
    let expected_tantivy = sqlite_count.saturating_sub(ocr_pending);

    // Allow 10% tolerance — minor drift is normal during concurrent updates
    if expected_tantivy > 0 && tantivy_count < expected_tantivy * 85 / 100 {
        errors::log_error(
            "reconcile",
            &format!("SQLite has {} files (expect ~{} in Tantivy), Tantivy has {}. Triggering re-index.",
                sqlite_count, expected_tantivy, tantivy_count),
        );
        // Re-index all files from SQLite into Tantivy
        if let Ok(cidx) = content::ContentIndex::open_or_create(content_idx_path) {
            let all_files: Vec<(String, String, Option<String>)> = db
                .get_all_paths()
                .unwrap_or_default()
                .into_iter()
                .map(|(path, filename, ext, _ts)| (path, filename, ext))
                .collect();
            if let Err(e) = cidx.index_files(&all_files) {
                errors::log_error("reconcile:tantivy", &format!("Re-index failed: {}", e));
            }
        }
    }
}

/// Check if index exists and has files
fn index_exists(db: &db::Database) -> bool {
    db.file_count().unwrap_or(0) > 0
}

/// Check if incremental reindex is needed (older than 24 hours)
fn needs_incremental_reindex(db: &db::Database) -> bool {
    let last_full = match db.get_meta("last_full_index_time") {
        Ok(Some(ts)) => ts,
        _ => return true,
    };
    let parsed = match chrono::DateTime::parse_from_rfc3339(&last_full) {
        Ok(dt) => dt,
        Err(_) => return true,
    };
    let age = chrono::Utc::now().signed_duration_since(parsed);
    age.num_hours() >= 24
}

/// Incremental reindex: diff filesystem against index, apply only changes.
fn run_incremental_index(db: &db::Database, verbose: bool) -> Result<()> {
    db.init_schema()?;

    if verbose { eprintln!("Incremental sync: computing diff..."); }
    let diff = indexer::compute_diff(db)?;

    let total_changes = diff.new_files.len() + diff.modified_files.len() + diff.deleted_paths.len();
    if verbose {
        eprintln!(
            "  {} new, {} modified, {} deleted ({}ms, {} dirs, {} errors)",
            diff.new_files.len(), diff.modified_files.len(), diff.deleted_paths.len(),
            diff.elapsed_ms, diff.dirs_scanned, diff.errors,
        );
    }

    if total_changes == 0 {
        if verbose { eprintln!("  No changes. Index is up to date."); }
        db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
        return Ok(());
    }

    // Apply to SQLite (INSERT OR REPLACE handles both new and modified)
    if !diff.new_files.is_empty() {
        db.insert_files_batch(&diff.new_files)?;
    }
    if !diff.modified_files.is_empty() {
        db.insert_files_batch(&diff.modified_files)?;
    }
    if !diff.deleted_paths.is_empty() {
        db.delete_paths_batch(&diff.deleted_paths)?;
    }

    // Apply to Tantivy (delete-by-term + re-add for changed, delete for removed)
    let cidx = content::ContentIndex::open_or_create(&content_index_path())?;

    let changed_files: Vec<(String, String, Option<String>)> = diff.new_files.iter()
        .chain(diff.modified_files.iter())
        .map(|f| (f.path.clone(), f.filename.clone(), f.extension.clone()))
        .collect();

    if !changed_files.is_empty() {
        let count = cidx.update_files(&changed_files)?;
        if verbose { eprintln!("  Content indexed: {} files", count); }
    }
    if !diff.deleted_paths.is_empty() {
        cidx.delete_files(&diff.deleted_paths)?;
        if verbose { eprintln!("  Content deleted: {} files", diff.deleted_paths.len()); }
    }

    // OCR any new/modified images
    let ocr_count = run_ocr_incremental(db, &cidx, verbose)?;
    if verbose && ocr_count > 0 {
        eprintln!("  OCR indexed: {} images", ocr_count);
    }

    db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
    db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339())?;

    if verbose { eprintln!("  Sync complete."); }
    Ok(())
}

/// Run full index (paths + content) using double-buffer for atomicity.
/// Builds into temp files, then swaps atomically on success.
/// Text files indexed in parallel (Phase 2), OCR spawned as background process (Phase 3).
fn run_full_index(_db: &db::Database, scan_paths: Option<&[String]>, verbose: bool) -> Result<()> {
    let temp_db_path = data_dir().join("index.db.new");
    let temp_content_path = data_dir().join("content_index.new");

    // Clean up any leftover temp files from a previous failed run
    let _ = std::fs::remove_file(&temp_db_path);
    let _ = std::fs::remove_dir_all(&temp_content_path);

    // Build into temp locations
    let temp_db = db::Database::open(&temp_db_path)?;
    temp_db.init_schema()?;

    if verbose { eprintln!("Phase 1: Indexing file paths..."); }
    let stats = indexer::build_index(&temp_db, scan_paths)?;
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
        .map(|(path, filename, ext, _ts)| (path, filename, ext))
        .collect();

    let temp_cidx = content::ContentIndex::open_or_create(&temp_content_path)?;
    let content_count = temp_cidx.index_files(&all_files)?;
    if verbose {
        eprintln!("  {} files with content indexed", content_count);
    }

    temp_db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
    let event_id = fsevents::current_event_id();
    temp_db.set_meta("fsevent_last_id", &event_id.to_string())?;

    // Drop handles before rename
    drop(temp_cidx);
    drop(temp_db);

    // Atomic swap — rename is atomic on same filesystem (POSIX guarantee)
    let bak_db = data_dir().join("index.db.bak");
    let bak_content = data_dir().join("content_index.bak");

    // Move old to backup (may not exist on first run)
    let _ = std::fs::rename(db_path(), &bak_db);
    let _ = std::fs::rename(content_index_path(), &bak_content);

    // Move new to active
    if let Err(e) = std::fs::rename(&temp_db_path, db_path()) {
        // Rollback: restore old
        let _ = std::fs::rename(&bak_db, db_path());
        let _ = std::fs::rename(&bak_content, content_index_path());
        return Err(anyhow::anyhow!("Failed to swap index: {}", e));
    }
    if let Err(e) = std::fs::rename(&temp_content_path, content_index_path()) {
        // Full rollback — restore both
        let _ = std::fs::rename(&bak_db, db_path());
        let _ = std::fs::rename(&bak_content, content_index_path());
        return Err(anyhow::anyhow!("Failed to swap content index: {}", e));
    }

    // Cleanup backups
    let _ = std::fs::remove_file(&bak_db);
    let _ = std::fs::remove_dir_all(&bak_content);

    if verbose { eprintln!("\nDone. Ready to search."); }

    // Phase 3: OCR in background (doesn't block search availability)
    // Re-open the new DB for OCR check
    let new_db = db::Database::open(&db_path())?;
    new_db.init_schema()?;
    let has_ocr = content::find_ocr_binary().is_some();
    if has_ocr {
        let pending = new_db.get_pending_ocr_paths(content::OCR_EXTENSIONS)?;
        if !pending.is_empty() {
            if verbose {
                eprintln!("  OCR: {} images queued for background processing...", pending.len());
            }
            spawn_background_ocr();
        }
    }

    Ok(())
}

/// Run OCR on pending images. Uses parallel batch mode.
fn run_ocr_incremental(db: &db::Database, cidx: &content::ContentIndex, verbose: bool) -> Result<usize> {
    if content::find_ocr_binary().is_none() {
        return Ok(0);
    }

    let pending = db.get_pending_ocr_paths(content::OCR_EXTENSIONS)?;
    if pending.is_empty() {
        return Ok(0);
    }

    if verbose {
        eprintln!("  OCR: {} new images to process...", pending.len());
    }

    let paths: Vec<std::path::PathBuf> = pending.iter()
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
            let filename = path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let ext = path.extension()
                .map(|e| e.to_string_lossy().to_string());
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
                crate::errors::log_error("ocr:tantivy", &format!("Failed to write OCR content: {}", e));
                // Don't mark as done — will retry next run
            }
        }
    } else if !ocr_marks.is_empty() {
        // No content to write (all low confidence) — still mark as done to avoid retry
        db.mark_ocr_done_batch(&ocr_marks)?;
    }

    Ok(indexed_count)
}

/// Layer 1: Quick diff — find new/modified files, index them.
fn quick_diff_sync(db: &db::Database, content_idx_path: &std::path::Path) -> usize {
    let new_files = match indexer::quick_diff(db) {
        Ok(f) => f,
        Err(e) => {
            errors::log_error("quick_diff", &format!("{}", e));
            return 0;
        }
    };

    if new_files.is_empty() {
        return 0;
    }

    if let Ok(cidx) = content::ContentIndex::open_or_create(content_idx_path) {
        if let Err(e) = cidx.update_files(&new_files) {
            errors::log_error("quick_diff:tantivy", &format!("{}", e));
        }
    }

    new_files.len()
}

/// Layer 1 (primary): FSEvents-based sync — reads macOS change journal.
/// Falls back to quick_diff if FSEvents unavailable (first run, journal purged).
fn fsevents_sync(db: &db::Database, content_idx_path: &std::path::Path) -> usize {
    let last_id: u64 = db.get_meta("fsevent_last_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let scan_paths = indexer::default_scan_paths();
    let result = match fsevents::get_changes_since(last_id, &scan_paths) {
        Some(r) => r,
        None => {
            // No stored event ID or journal unavailable — fallback
            return quick_diff_sync(db, content_idx_path);
        }
    };

    // Store new event ID even if no changes (advance the cursor)
    let _ = db.set_meta("fsevent_last_id", &result.new_event_id.to_string());

    // If FSEvents replay was incomplete (timeout before HistoryDone),
    // fall back to compute_diff for a comprehensive sync
    if !result.complete && !result.changes.is_empty() {
        errors::log_error("fsevents", "Incomplete replay — falling back to quick_diff");
        return quick_diff_sync(db, content_idx_path);
    }

    if result.changes.is_empty() {
        return 0;
    }

    // Process FSEvents into file entries and deletions
    let (to_update, to_delete) = indexer::process_fsevents(&result);

    let total = to_update.len() + to_delete.len();
    if total == 0 {
        return 0;
    }

    // Apply to SQLite
    if !to_update.is_empty() {
        if let Err(e) = db.insert_files_batch(&to_update) {
            errors::log_error("fsevents:sqlite", &format!("insert_files_batch: {}", e));
        }
    }
    if !to_delete.is_empty() {
        if let Err(e) = db.delete_paths_batch(&to_delete) {
            errors::log_error("fsevents:sqlite", &format!("delete_paths_batch: {}", e));
        }
    }

    // Apply to Tantivy
    if let Ok(cidx) = content::ContentIndex::open_or_create(content_idx_path) {
        if !to_update.is_empty() {
            let update_tuples: Vec<_> = to_update.iter()
                .map(|f| (f.path.clone(), f.filename.clone(), f.extension.clone()))
                .collect();
            if let Err(e) = cidx.update_files(&update_tuples) {
                errors::log_error("fsevents:tantivy", &format!("update_files: {}", e));
            }
        }
        if !to_delete.is_empty() {
            if let Err(e) = cidx.delete_files(&to_delete) {
                errors::log_error("fsevents:tantivy", &format!("delete_files: {}", e));
            }
        }

        // OCR any new images detected via FSEvents
        if let Err(e) = run_ocr_incremental(db, &cidx, false) {
            errors::log_error("fsevents:ocr", &format!("{}", e));
        }
    }

    if let Err(e) = db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339()) {
        errors::log_error("fsevents:meta", &format!("{}", e));
    }
    total
}

/// Spawn a detached background process. Skips if sync.lock is held.
fn spawn_background(args: &[&str]) {
    // Check if another background process is running
    if try_acquire_lock().is_none() {
        return; // Another process holds the lock — skip
    }
    // Lock released here — the spawned process will acquire its own

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    if let Err(e) = std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        errors::log_error("spawn", &format!("Failed to spawn background {:?}: {}", args, e));
    }
}

fn spawn_background_sync() { spawn_background(&["index", "sync"]); }
fn spawn_background_ocr() { spawn_background(&["index", "ocr"]); }
fn spawn_background_rebuild() { spawn_background(&["index", "rebuild"]); }

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search { query, r#type, json, limit } => {
            if query.trim().is_empty() {
                if json {
                    println!("{}", serde_json::json!({"query":"","mode":"unified","elapsed_ms":0,"total_results":0,"results":[]}));
                } else {
                    eprintln!("Query cannot be empty.");
                }
                return Ok(());
            }

            let db = db::Database::open(&db_path())?;
            db.init_schema()?;

            // === Auto-index on first run ===
            if !index_exists(&db) {
                if json {
                    // For Raycast: return special response indicating indexing
                    let response = search::SearchResponse {
                        query: query.clone(),
                        mode: "indexing".to_string(),
                        elapsed_ms: 0,
                        total_results: 0,
                        results: vec![],
                    };
                    println!("{}", serde_json::to_string_pretty(&response)?);
                    // Spawn indexing in background
                    spawn_background_rebuild();
                    return Ok(());
                } else {
                    eprintln!("First run detected. Building index...");
                    run_full_index(&db, None, true)?;
                }
            }

            // Acquire lock for write operations during search (schema check, sync, reconcile)
            // Non-blocking: skip writes if another process holds the lock
            let _search_lock = try_acquire_lock();
            if _search_lock.is_some() {
                check_schema_version(&db);
                reconcile_if_needed(&db, &content_index_path());
            }

            // Layer 1: FSEvents-based sync (falls back to quick_diff)
            let new_count = if _search_lock.is_some() {
                fsevents_sync(&db, &content_index_path())
            } else {
                0 // Skip sync if locked — another process is handling it
            };
            if new_count > 0 && !json {
                eprintln!("[+] {} changes detected via FSEvents", new_count);
            }

            // Unified search: filenames + content, tiered ranking
            let response = search::unified_search(
                &db,
                &content_index_path(),
                &query,
                limit,
                r#type.as_deref(),
            )?;

            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                if response.results.is_empty() {
                    eprintln!("No results for \"{}\"", response.query);
                } else {
                    eprintln!(
                        "Found {} results in {}ms\n",
                        response.total_results, response.elapsed_ms
                    );
                    for (i, result) in response.results.iter().enumerate() {
                        let type_badge = result.file_type.as_deref().unwrap_or("?");
                        println!(
                            "  {}. [{}] {}\n     {}",
                            i + 1, type_badge, result.filename, result.path,
                        );
                        if let Some(ref snippet) = result.content_snippet {
                            println!("     >> {}", snippet);
                        }
                    }
                }
            }

            // Layer 2: Background incremental sync if stale (>24 hours)
            if needs_incremental_reindex(&db) {
                spawn_background_sync();
            }
        }

        Commands::Index { action } => match action {
            IndexAction::Init { paths } | IndexAction::Rebuild { paths } => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Try again later."); return Ok(()); }
                };
                let db = db::Database::open(&db_path())?;

                let scan_paths: Option<Vec<String>> = paths.map(|p| {
                    p.split(',').map(|s| {
                        let s = s.trim().to_string();
                        if s.starts_with("~/") {
                            let home = std::env::var("HOME").unwrap_or_default();
                            s.replacen("~", &home, 1)
                        } else {
                            s
                        }
                    }).collect()
                });

                run_full_index(&db, scan_paths.as_deref(), true)?;
            }
            IndexAction::Sync => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Skipping."); return Ok(()); }
                };
                let db = db::Database::open(&db_path())?;
                run_incremental_index(&db, true)?;
            }
            IndexAction::Ocr => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Skipping."); return Ok(()); }
                };
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;
                let cidx = content::ContentIndex::open_or_create(&content_index_path())?;
                let count = run_ocr_incremental(&db, &cidx, true)?;
                eprintln!("OCR complete: {} images indexed", count);
            }
            IndexAction::Status => {
                let db = db::Database::open(&db_path())?;
                let count = db.file_count().unwrap_or(0);
                let last_index = db.get_meta("last_index_time")?.unwrap_or("never".into());
                let last_full = db.get_meta("last_full_index_time")?.unwrap_or("never".into());

                let content_count = content::ContentIndex::open_or_create(&content_index_path())
                    .and_then(|c| c.doc_count())
                    .unwrap_or(0);

                let (ocr_total, ocr_done) = db.ocr_stats(content::OCR_EXTENSIONS).unwrap_or((0, 0));

                eprintln!("Index status:");
                eprintln!("  Files indexed: {}", count);
                eprintln!("  Content indexed: {} files", content_count);
                if ocr_total > 0 {
                    eprintln!("  OCR indexed: {}/{} images", ocr_done, ocr_total);
                }
                eprintln!("  Last updated: {}", last_index);
                eprintln!("  Last full reindex: {}", last_full);
                eprintln!("  Index location: {}", data_dir().display());
            }
        },

        Commands::Doctor { json } => {
            let report = build_doctor_report();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                eprintln!("{}", format_doctor_report(&report));
            }
        }
    }

    Ok(())
}

fn build_doctor_report() -> serde_json::Value {
    let db_result = db::Database::open(&db_path());
    let db_ok = db_result.is_ok();

    let (file_count, last_index, last_full, ocr_total, ocr_done) = match &db_result {
        Ok(db) => {
            let _ = db.init_schema();
            let fc = db.file_count().unwrap_or(0);
            let li = db.get_meta("last_index_time").unwrap_or(None).unwrap_or_else(|| "never".into());
            let lf = db.get_meta("last_full_index_time").unwrap_or(None).unwrap_or_else(|| "never".into());
            let (ot, od) = db.ocr_stats(content::OCR_EXTENSIONS).unwrap_or((0, 0));
            (fc, li, lf, ot, od)
        }
        Err(_) => (0, "never".into(), "never".into(), 0, 0),
    };

    let content_count = content::ContentIndex::open_or_create(&content_index_path())
        .and_then(|c| c.doc_count())
        .unwrap_or(0);

    let index_dir = data_dir();
    let db_size = std::fs::metadata(db_path()).map(|m| m.len()).unwrap_or(0);
    let content_dir_size = walkdir_size(&content_index_path());
    let recent_errors = errors::read_recent_errors(20);

    let home = std::env::var("HOME").unwrap_or_default();
    let scan_paths = ["~/Documents", "~/Desktop", "~/Downloads", "~/Projects", "~/Pictures"];
    let paths_status: Vec<serde_json::Value> = scan_paths
        .iter()
        .map(|p| {
            let expanded = p.replace("~", &home);
            let exists = std::path::Path::new(&expanded).exists();
            serde_json::json!({ "path": p, "exists": exists })
        })
        .collect();

    let ocr_binary_found = content::find_ocr_binary().is_some();
    let fda = indexer::check_full_disk_access();

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "database": {
            "ok": db_ok,
            "path": db_path().to_string_lossy(),
            "size_bytes": db_size,
            "files_indexed": file_count,
            "content_indexed": content_count,
            "last_updated": last_index,
            "last_full_reindex": last_full,
        },
        "ocr": {
            "binary_found": ocr_binary_found,
            "total_images": ocr_total,
            "ocr_completed": ocr_done,
        },
        "content_index": {
            "path": content_index_path().to_string_lossy(),
            "size_bytes": content_dir_size,
        },
        "index_location": index_dir.to_string_lossy(),
        "scan_paths": paths_status,
        "full_disk_access": {
            "ok": fda.0,
            "inaccessible": fda.1,
        },
        "recent_errors": recent_errors,
        "os": {
            "arch": std::env::consts::ARCH,
            "os": std::env::consts::OS,
        }
    })
}

fn format_doctor_report(report: &serde_json::Value) -> String {
    let mut out = String::new();
    out.push_str(&format!("Findr v{}\n\n", report["version"].as_str().unwrap_or("?")));

    out.push_str("Database:\n");
    out.push_str(&format!("  Status: {}\n", if report["database"]["ok"].as_bool().unwrap_or(false) { "OK" } else { "ERROR" }));
    out.push_str(&format!("  Files indexed: {}\n", report["database"]["files_indexed"]));
    out.push_str(&format!("  Content indexed: {}\n", report["database"]["content_indexed"]));
    out.push_str(&format!("  Last updated: {}\n", report["database"]["last_updated"].as_str().unwrap_or("?")));
    out.push_str(&format!("  Last full reindex: {}\n", report["database"]["last_full_reindex"].as_str().unwrap_or("?")));
    out.push_str(&format!("  DB size: {} KB\n", report["database"]["size_bytes"].as_u64().unwrap_or(0) / 1024));
    out.push_str(&format!("  Content index size: {} KB\n", report["content_index"]["size_bytes"].as_u64().unwrap_or(0) / 1024));

    out.push_str("\nOCR:\n");
    out.push_str(&format!("  Binary found: {}\n", if report["ocr"]["binary_found"].as_bool().unwrap_or(false) { "YES" } else { "NO" }));
    out.push_str(&format!("  Images indexed: {}/{}\n", report["ocr"]["ocr_completed"], report["ocr"]["total_images"]));

    out.push_str("\nScan paths:\n");
    if let Some(paths) = report["scan_paths"].as_array() {
        for p in paths {
            let status = if p["exists"].as_bool().unwrap_or(false) { "OK" } else { "MISSING" };
            out.push_str(&format!("  {} — {}\n", p["path"].as_str().unwrap_or("?"), status));
        }
    }

    out.push_str(&format!("\nRecent errors:\n{}\n", report["recent_errors"].as_str().unwrap_or("(none)")));
    out
}

fn walkdir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_dir() {
                        stack.push(entry.path());
                    } else {
                        total += meta.len();
                    }
                }
            }
        }
    }
    total
}
