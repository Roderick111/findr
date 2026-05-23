use findr::content;
use findr::db;
use findr::errors;
#[cfg(target_os = "macos")]
use findr::fsevents;
use findr::indexer;
use findr::platform;
use findr::search;
use findr::semantic;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs::File;
use std::path::PathBuf;
use std::sync::OnceLock;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

fn data_dir() -> PathBuf {
    DATA_DIR.get_or_init(|| {
        let dir = platform::data_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("Warning: failed to create {}: {}", dir.display(), e);
        }
        platform::secure_directory(&dir);
        dir
    }).clone()
}

/// Try to acquire an exclusive lock on data_dir/sync.lock.
/// Returns the File handle (holds lock until dropped), or None if already locked.
fn try_acquire_lock() -> Option<File> {
    let lock_path = data_dir().join("sync.lock");
    let file = File::create(&lock_path).ok()?;
    if platform::try_lock_exclusive(&file) { Some(file) } else { None }
}

/// Separate lock for embedding — allows parallel execution with OCR/sync.
fn try_acquire_embed_lock() -> Option<File> {
    let lock_path = data_dir().join("embed.lock");
    let file = File::create(&lock_path).ok()?;
    if platform::try_lock_exclusive(&file) { Some(file) } else { None }
}

fn db_path() -> PathBuf {
    data_dir().join("index.db")
}

fn content_index_path() -> PathBuf {
    data_dir().join("content_index")
}

#[derive(Parser)]
#[command(name = "findr", version, about = "The fastest local file search",
    after_help = "EXAMPLES:\n  findr search invoice\n  findr search \"resume pdf\"          # inline type filter\n  findr search main.rs --type rs      # explicit type filter\n  findr search \"projects /\"           # folder filter (trailing /)\n  findr search \"/brainform\"           # folder filter (leading /)\n  findr search \"dharma in:daily\"      # scope to folders named 'daily'\n  findr search \"report in:downloads\"  # scope to Downloads\n  findr search \"in:obsidian\"          # recent files in scope\n  findr search revolut --path ~/Docs  # explicit path filter\n  findr search revolut --snippet-length 500  # longer snippets\n  findr index status\n  findr index embed --status\n  findr doctor --json\n\nINLINE FILTERS:\n  Type: last word matching a known extension (pdf, png, docx, etc.)\n  Folder: trailing '/' or 'folder'/'dir' keyword\n  Scope: 'in:<name>' searches inside matching folders\n\nSEMANTIC SEARCH:\n  Set OPENROUTER_API_KEY env or create openrouter_key in data dir\n  Then run: findr index embed\n  Get a key at: https://openrouter.ai")]
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

        /// Filter results to files under this path
        #[arg(long)]
        path: Option<String>,

        /// Max length of content snippets (default: 200)
        #[arg(long, default_value = "200")]
        snippet_length: usize,

        /// Skip semantic search (faster, no API call)
        #[arg(long)]
        no_semantic: bool,

        /// Skip index sync (return cached results instantly)
        #[arg(long)]
        no_sync: bool,
    },
    /// Manage the file index
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Record a file interaction for frequency-based ranking
    Track {
        /// File path that was interacted with
        path: String,

        /// Action type
        #[arg(long, value_parser = ["open", "finder", "copy", "preview"])]
        action: String,
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
        /// Scan scope preset
        #[arg(long, value_parser = ["personal", "full_home", "everything"])]
        preset: Option<String>,
    },
    /// Show index status
    Status,
    /// Rebuild entire index (full nuke + rebuild)
    Rebuild {
        /// Specific paths to scan (comma-separated)
        #[arg(long)]
        paths: Option<String>,
        /// Scan scope preset
        #[arg(long, value_parser = ["personal", "full_home", "everything"])]
        preset: Option<String>,
    },
    /// Incremental sync (diff-based, only processes changes)
    Sync,
    /// Run OCR indexing on pending images (usually called as background process)
    Ocr,
    /// Add a single path to the index without rebuilding
    AddPath {
        /// Directory to index
        path: String,
    },
    /// Run semantic embedding on pending files (requires OpenRouter API key)
    Embed {
        /// Show embedding status instead of running
        #[arg(long)]
        status: bool,
    },
}

/// Check schema version. If outdated, trigger a background full rebuild
/// so new features (like directory indexing) get picked up.
fn check_schema_version(db: &db::Database) {
    let version = db.get_meta("schema_version").unwrap_or(None).unwrap_or_default();
    if version != "3" {
        if db.set_meta("schema_version", "3").is_ok() {
            // Delete content index (may have stale schema)
            let cidx_path = content_index_path();
            if cidx_path.exists() {
                let _ = std::fs::remove_dir_all(&cidx_path);
            }
            // Trigger full rebuild in background to pick up new features (dir indexing)
            eprintln!("Schema upgraded to v3. Rebuilding index in background...");
            spawn_background_rebuild();
        } else {
            errors::log_error("schema", "Failed to set schema_version — skipping migration");
        }
    }
}

/// Detect SQLite/Tantivy drift and trigger targeted re-index if diverged.
/// Compares Tantivy doc count against the actual count stored after last indexing,
/// NOT a computed estimate (which was wrong — it didn't account for files with
/// no extractable content like binaries, empty files, failed extractions).
/// Runs at most once per hour to avoid redundant Tantivy opens on every search.
fn reconcile_if_needed(db: &db::Database, content_idx_path: &std::path::Path) {
    // Only check every hour — avoid opening Tantivy on every search
    if let Ok(Some(last_check)) = db.get_meta("last_reconcile_check") {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&last_check) {
            let age = chrono::Utc::now().signed_duration_since(dt);
            if age.num_minutes() < 60 { return; }
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

    if expected_tantivy == 0 { return; }

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
            &format!("Content index drift: expected ~{}, found {}. Triggering re-index.",
                expected_tantivy, tantivy_count),
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

    // Apply to Tantivy first (delete-by-term + re-add for changed, delete for removed).
    // If crash after Tantivy but before SQLite, Tantivy has extra docs (harmless,
    // cleaned on next reconcile). Reverse order would leave SQLite with files
    // Tantivy doesn't have, causing content matches to silently go missing.
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

    // Apply to SQLite after Tantivy succeeds (INSERT OR REPLACE handles both new and modified)
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

    if verbose { eprintln!("  Sync complete."); }
    Ok(())
}

/// Run full index (paths + content) using double-buffer for atomicity.
/// Builds into temp files, then swaps atomically on success.
/// Text files indexed in parallel (Phase 2), OCR spawned as background process (Phase 3).
fn run_full_index(parent_db: &db::Database, scan_paths: Option<&[String]>, verbose: bool) -> Result<()> {
    let temp_db_path = data_dir().join("index.db.new");
    let temp_content_path = data_dir().join("content_index.new");

    // Clean up any leftover temp files from a previous failed run
    let _ = std::fs::remove_file(&temp_db_path);
    let _ = std::fs::remove_dir_all(&temp_content_path);

    // Build into temp locations
    let temp_db = db::Database::open(&temp_db_path)?;
    temp_db.init_schema()?;

    if verbose { eprintln!("Phase 1: Indexing file paths..."); }
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
    // (not a computed estimate that ignores files with no extractable content)
    temp_db.set_meta("content_indexed_count", &content_count.to_string())?;

    temp_db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
    #[cfg(target_os = "macos")]
    {
        let event_id = fsevents::current_event_id();
        temp_db.set_meta("fsevent_last_id", &event_id.to_string())?;
    }

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
        if let Err(re) = std::fs::rename(&bak_db, db_path()) {
            errors::log_error("rollback", &format!("CRITICAL: failed to restore DB backup: {}", re));
        }
        if let Err(re) = std::fs::rename(&bak_content, content_index_path()) {
            errors::log_error("rollback", &format!("CRITICAL: failed to restore content backup: {}", re));
        }
        return Err(anyhow::anyhow!("Failed to swap index: {}", e));
    }
    if let Err(e) = std::fs::rename(&temp_content_path, content_index_path()) {
        // Full rollback — restore both
        if let Err(re) = std::fs::rename(&bak_db, db_path()) {
            errors::log_error("rollback", &format!("CRITICAL: failed to restore DB backup: {}", re));
        }
        if let Err(re) = std::fs::rename(&bak_content, content_index_path()) {
            errors::log_error("rollback", &format!("CRITICAL: failed to restore content backup: {}", re));
        }
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

    // Phase 4: Semantic embedding in background (network-bound, parallel to OCR)
    if semantic::get_api_key().is_some() {
        let embed_pending = new_db.get_pending_embed_paths(semantic::EMBEDDABLE_EXTENSIONS)?;
        if !embed_pending.is_empty() {
            if verbose {
                eprintln!("  Semantic: {} files queued for background embedding...", embed_pending.len());
            }
            spawn_background_embed();
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
                errors::log_error("ocr:tantivy", &format!("Failed to write OCR content: {}", e));
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
fn get_query_vector(db: &db::Database, api_key: &str, query: &str) -> Option<Vec<f32>> {
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

/// Layer 1: Quick diff — find new/modified files, index them.
fn incremental_sync(db: &db::Database, content_idx_path: &std::path::Path) -> usize {
    let new_files = match indexer::quick_sync(db) {
        Ok(f) => f,
        Err(e) => {
            errors::log_error("quick_sync", &format!("{}", e));
            return 0;
        }
    };

    if new_files.is_empty() {
        return 0;
    }

    if let Ok(cidx) = content::ContentIndex::open_or_create(content_idx_path) {
        if let Err(e) = cidx.update_files(&new_files) {
            errors::log_error("quick_sync:tantivy", &format!("{}", e));
        }
    }

    new_files.len()
}

/// Embed pending files in batches. Mirrors run_ocr_incremental pattern.
fn run_embed_batch(db: &db::Database, api_key: &str, verbose: bool) -> Result<usize> {
    let pending = db.get_pending_embed_paths(semantic::EMBEDDABLE_EXTENSIONS)?;
    if pending.is_empty() {
        return Ok(0);
    }

    let total = pending.len();
    let total_batches = total.div_ceil(semantic::API_BATCH_SIZE);
    if verbose {
        eprintln!("  Semantic: {} files to embed ({} batches)...", total, total_batches);
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
                let entries: Vec<(String, Vec<u8>, i64, String)> = vectors.iter()
                    .zip(meta.iter())
                    .map(|(vec, (path, mtime, hash))| {
                        (path.clone(), semantic::vec_to_bytes(vec), *mtime, hash.clone())
                    })
                    .collect();
                if let Err(e) = db.upsert_semantic_vectors(&entries) {
                    errors::log_error("embed:db", &format!("{}", e));
                } else {
                    embedded += entries.len();
                    if verbose {
                        eprintln!("    Batch {}/{}: {} / {} embedded", batch_num, total_batches, embedded, total);
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
fn rebuild_hnsw_index(db: &db::Database, verbose: bool) {
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
        .filter_map(|(path, bytes)| {
            semantic::bytes_to_vec(&bytes).map(|v| (path, v))
        })
        .collect();

    if vectors.is_empty() {
        return;
    }

    let dir = data_dir();
    match semantic::build_and_save_hnsw(&vectors, &dir) {
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
                eprintln!("  Warning: HNSW index build failed: {}. Brute-force fallback active.", e);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn fsevents_sync(db: &db::Database, content_idx_path: &std::path::Path) -> usize {
    let last_id: u64 = db.get_meta("fsevent_last_id")
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

    // Store new event ID even if no changes (advance the cursor)
    let _ = db.set_meta("fsevent_last_id", &result.new_event_id.to_string());

    // If FSEvents replay was incomplete (timeout before HistoryDone),
    // fall back to compute_diff for a comprehensive sync
    if !result.complete && !result.changes.is_empty() {
        errors::log_error("fsevents", "Incomplete replay — falling back to quick_sync");
        return incremental_sync(db, content_idx_path);
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

    // Apply to Tantivy first — extra docs on crash are harmless (reconcile cleans up).
    // SQLite-first would leave content matches silently missing on crash.
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

    // Apply to SQLite after Tantivy
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

    // Invalidate semantic vectors for changed files (background will re-embed)
    if semantic::get_api_key().is_some() && !to_update.is_empty() {
        let update_paths: Vec<String> = to_update.iter().map(|f| f.path.clone()).collect();
        let _ = db.delete_semantic_paths(&update_paths);
    }

    if let Err(e) = db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339()) {
        errors::log_error("fsevents:meta", &format!("{}", e));
    }
    total
}

/// Spawn a detached background process. Skips if sync.lock is held.
fn spawn_background(args: &[&str]) {
    // No pre-check for lock — child process acquires its own lock on startup.
    // Avoids TOCTOU race where lock is released between check and child spawn.
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

/// Spawn embedding as a separate detached process (uses embed.lock, not sync.lock).
/// No pre-check for lock — child process acquires its own lock on startup.
fn spawn_background_embed() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(["index", "embed"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Resolve scan paths: preset selects base scope, --paths adds extra paths on top.
/// Falls back to stored preset in DB, then hardcoded defaults.
fn resolve_scan_paths(custom_paths: Option<&str>, preset: Option<&str>, db: &db::Database) -> Vec<String> {
    let effective_preset = preset
        .map(|s| s.to_string())
        .or_else(|| db.get_meta("scan_preset").ok().flatten())
        .unwrap_or_else(|| "personal".to_string());

    indexer::scan_paths_for_preset(&effective_preset, custom_paths)
}

/// Store the scan configuration in DB metadata for future syncs.
fn store_scan_config(db: &db::Database, paths: &[String], preset: Option<&str>, custom_paths: Option<&str>) {
    let _ = db.set_meta("scan_paths", &paths.join(","));
    if let Some(p) = preset {
        let _ = db.set_meta("scan_preset", p);
    }
    if let Some(c) = custom_paths {
        let _ = db.set_meta("custom_paths", c);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search { query, r#type, json, limit, path, snippet_length, no_semantic, no_sync } => {
            if query.trim().is_empty() && !json {
                eprintln!("Query cannot be empty.");
                std::process::exit(1);
            }

            // Guard against single-char queries — Nucleo matches nearly everything
            // with min_score=12, causing long search times.
            if query.trim().len() < 2 && !query.trim().is_empty() {
                if json {
                    println!("{}", serde_json::json!({
                        "query": query,
                        "mode": "too_short",
                        "elapsed_ms": 0,
                        "total_results": 0,
                        "results": [],
                        "hint": "Type at least 2 characters"
                    }));
                    return Ok(()); // exit 0 — valid JSON response for Raycast
                } else {
                    eprintln!("Query too short. Type at least 2 characters.");
                    std::process::exit(1);
                }
            }

            let db = match db::Database::open(&db_path()) {
                Ok(db) => db,
                Err(e) => {
                    if json {
                        println!("{}", serde_json::json!({"error": format!("Index corrupt: {}. Run: findr index rebuild", e)}));
                        return Ok(()); // exit 0 — valid JSON for Raycast
                    } else {
                        eprintln!("Index corrupt ({}). Run: findr index rebuild", e);
                        std::process::exit(1);
                    }
                }
            };
            if let Err(e) = db.init_schema() {
                if json {
                    println!("{}", serde_json::json!({"error": format!("Schema init failed: {}. Run: findr index rebuild", e)}));
                    return Ok(());
                } else {
                    eprintln!("Schema init failed ({}). Run: findr index rebuild", e);
                    std::process::exit(1);
                }
            }

            // Prune interactions older than 1 year (lightweight, runs on search startup)
            let _ = db.prune_old_interactions();

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

            // Canonicalize --path filter (expand ~, ensure trailing /)
            let mut path_filter: Vec<String> = match path {
                Some(p) => {
                    let expanded = if p.starts_with('~') {
                        p.replacen('~', &platform::home_dir().unwrap_or_default(), 1)
                    } else {
                        p
                    };
                    let pb = std::path::PathBuf::from(&expanded);
                    let canonical = pb.canonicalize().unwrap_or(pb).to_string_lossy().to_string();
                    vec![if canonical.ends_with('/') || canonical.ends_with('\\') { canonical } else { format!("{}{}", canonical, std::path::MAIN_SEPARATOR) }]
                }
                None => vec![],
            };

            // Pre-parse query: extract in:scope and inline type filter
            let (clean_query, inline_type, scope) = search::parse_query(&query);

            // Resolve in:scope → folder paths via folder discovery search
            if let Some(ref scope_name) = scope {
                let scope_response = search::unified_search(
                    &db,
                    &content_index_path(),
                    scope_name,
                    &search::SearchOptions {
                        limit: 20,
                        type_filter: Some("__dir__"),
                        snippet_length: 0,
                        path_filter: &path_filter,
                        ..Default::default()
                    },
                )?;
                let scope_paths: Vec<String> = scope_response.results
                    .into_iter()
                    .map(|r| if r.path.ends_with('/') || r.path.ends_with('\\') { r.path } else { format!("{}{}", r.path, std::path::MAIN_SEPARATOR) })
                    .collect();
                if !scope_paths.is_empty() {
                    path_filter = scope_paths;
                }
                // If 0 matching folders → keep existing path_filter (fail open)
            }

            // Sync BEFORE returning results (recent files or search)
            // --no-sync: skip sync entirely (return cached results instantly)
            let _search_lock = if !no_sync { try_acquire_lock() } else { None };
            if _search_lock.is_some() {
                check_schema_version(&db);
                reconcile_if_needed(&db, &content_index_path());
            }

            // Incremental sync (FSEvents on macOS, quick_sync elsewhere)
            let new_count = if _search_lock.is_some() {
                #[cfg(target_os = "macos")]
                { fsevents_sync(&db, &content_index_path()) }
                #[cfg(not(target_os = "macos"))]
                { incremental_sync(&db, &content_index_path()) }
            } else {
                0
            };

            // Empty query (after scope extraction) → return recent files (now freshly synced)
            if clean_query.trim().is_empty() && json {
                let response = search::recent_files(&db, limit, &path_filter)?;
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            if clean_query.trim().is_empty() && !json {
                if scope.is_some() {
                    let response = search::recent_files(&db, limit, &path_filter)?;
                    for r in &response.results {
                        eprintln!("  {}", r.filename);
                    }
                } else {
                    eprintln!("Query cannot be empty.");
                }
                return Ok(());
            }
            if new_count > 0 && !json {
                eprintln!("[+] {} changes synced", new_count);
            }

            // Semantic search: skipped with --no-semantic flag.
            // Uses cached query vectors when available (instant), API with 3s timeout otherwise.
            let semantic_matches: Option<search::SemanticMatches> = if no_semantic {
                None
            } else {
                semantic::get_api_key().and_then(|_api_key| {
                    // Step 1: Get query vector (cache → API with 1s deadline)
                    let qvec = get_query_vector(&db, &_api_key, &clean_query)?;
                    let hnsw_dir = data_dir();

                    // Step 2: HNSW lookup (local, fast)
                    let hnsw_stale = if semantic::hnsw_index_exists(&hnsw_dir) {
                        let stored = db.get_meta("hnsw_vector_count").ok().flatten()
                            .and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                        let (_, current) = db.semantic_stats(semantic::EMBEDDABLE_EXTENSIONS).unwrap_or((0, 0));
                        stored > 0 && current > 0 && (current as f64 / stored as f64) < 0.85
                    } else {
                        false
                    };

                    if semantic::hnsw_index_exists(&hnsw_dir) && !hnsw_stale {
                        match semantic::query_hnsw(&qvec, &hnsw_dir, limit) {
                            Ok(matches) if !matches.is_empty() => return Some(matches),
                            Ok(_) => {}
                            Err(e) => {
                                errors::log_error("hnsw:query", &format!("{}", e));
                            }
                        }
                    }

                    // Brute-force fallback (capped at 2000 most recent to bound latency)
                    let raw_vecs = db.load_recent_vectors(2000).unwrap_or_default();
                    if raw_vecs.is_empty() {
                        return None;
                    }
                    let matches: Vec<(String, f32)> = raw_vecs.into_iter()
                        .filter_map(|(path, bytes)| {
                            let vec = semantic::bytes_to_vec(&bytes)?;
                            let sim = semantic::cosine_similarity(&qvec, &vec);
                            if sim >= semantic::COSINE_THRESHOLD {
                                Some((path, sim))
                            } else {
                                None
                            }
                        })
                        .collect();
                    if matches.is_empty() { return None; }
                    Some(matches)
                })
            };

            // Unified search: filenames + content + semantic, tiered ranking
            let effective_type = r#type.as_deref().or(inline_type.as_deref());
            let response = search::unified_search(
                &db,
                &content_index_path(),
                &clean_query,
                &search::SearchOptions {
                    limit,
                    type_filter: effective_type,
                    semantic_matches: semantic_matches.as_ref(),
                    snippet_length,
                    path_filter: &path_filter,
                },
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
            IndexAction::Init { paths, preset } => {
                let db = db::Database::open(&db_path())?;
                if index_exists(&db) {
                    eprintln!("Index already exists ({} files). Use 'findr index rebuild' to recreate.", db.file_count().unwrap_or(0));
                    return Ok(());
                }
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Try again later."); std::process::exit(2); }
                };

                let scan_paths = resolve_scan_paths(paths.as_deref(), preset.as_deref(), &db);
                store_scan_config(&db, &scan_paths, preset.as_deref(), paths.as_deref());
                run_full_index(&db, Some(&scan_paths), true)?;
            }
            IndexAction::Rebuild { paths, preset } => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Try again later."); std::process::exit(2); }
                };

                // Rebuild creates a fresh temp DB, so a corrupt existing DB is fine.
                // Try to open it (for scan path hints), but recover if it's corrupt.
                let db = match db::Database::open(&db_path()) {
                    Ok(db) => db,
                    Err(e) => {
                        eprintln!("Warning: existing index is corrupt ({}). Cleaning up...", e);
                        let _ = std::fs::remove_file(db_path());
                        let _ = std::fs::remove_file(db_path().with_extension("db-wal"));
                        let _ = std::fs::remove_file(db_path().with_extension("db-shm"));
                        let _ = std::fs::remove_dir_all(content_index_path());
                        // Create a fresh DB — run_full_index builds into temp anyway
                        db::Database::open(&db_path())?
                    }
                };

                let scan_paths = resolve_scan_paths(paths.as_deref(), preset.as_deref(), &db);
                store_scan_config(&db, &scan_paths, preset.as_deref(), paths.as_deref());
                run_full_index(&db, Some(&scan_paths), true)?;
            }
            IndexAction::AddPath { path } => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Skipping."); std::process::exit(2); }
                };
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;

                let expanded = indexer::expand_tilde(&path);
                let preset = db.get_meta("scan_preset").ok().flatten();

                eprintln!("Indexing {}...", expanded);
                let stats = indexer::index_single_path(&db, &expanded, preset.as_deref())?;
                eprintln!("  {} files, {} dirs in {}ms", stats.files_indexed, stats.dirs_scanned, stats.elapsed_ms);

                // Index content for new files
                let cidx = content::ContentIndex::open_or_create(&content_index_path())?;
                let new_files: Vec<(String, String, Option<String>)> = db
                    .get_all_paths()?
                    .into_iter()
                    .filter(|f| f.path.starts_with(&expanded))
                    .map(|f| (f.path, f.filename, f.extension))
                    .collect();
                if !new_files.is_empty() {
                    let count = cidx.update_files(&new_files)?;
                    eprintln!("  {} files content indexed", count);
                }

                // Update stored scan paths to include new path
                let mut current_paths = indexer::stored_or_default_paths(&db);
                if !current_paths.contains(&expanded) {
                    current_paths.push(expanded);
                    store_scan_config(&db, &current_paths, None, None);
                }
            }
            IndexAction::Sync => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Skipping."); std::process::exit(2); }
                };
                let db = db::Database::open(&db_path())?;
                run_incremental_index(&db, true)?;
            }
            IndexAction::Ocr => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => { eprintln!("Another findr process is running. Skipping."); std::process::exit(2); }
                };
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;
                let cidx = content::ContentIndex::open_or_create(&content_index_path())?;
                let count = run_ocr_incremental(&db, &cidx, true)?;
                eprintln!("OCR complete: {} images indexed", count);
            }
            IndexAction::Embed { status } => {
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;

                if status {
                    let (total, done) = db.semantic_stats(semantic::EMBEDDABLE_EXTENSIONS).unwrap_or((0, 0));
                    let has_key = semantic::get_api_key().is_some();
                    println!("Semantic embedding status:");
                    println!("  API key: {}", if has_key { "configured" } else { "not configured" });
                    println!("  Files embedded: {}/{}", done, total);
                    return Ok(());
                }

                let api_key = match semantic::get_api_key() {
                    Some(k) => k,
                    None => {
                        eprintln!("No API key. Set OPENROUTER_API_KEY or create {}/openrouter_key", data_dir().display());
                        return Ok(());
                    }
                };

                let _lock = match try_acquire_embed_lock() {
                    Some(f) => f,
                    None => {
                        eprintln!("Another embedding process is running. Skipping.");
                        std::process::exit(2);
                    }
                };

                let count = run_embed_batch(&db, &api_key, true)?;
                eprintln!("Embedding complete: {} files embedded", count);

                // Rebuild HNSW index from all vectors
                if count > 0 {
                    rebuild_hnsw_index(&db, true);
                }
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
                let (embed_total, embed_done) = db.semantic_stats(semantic::EMBEDDABLE_EXTENSIONS).unwrap_or((0, 0));
                let has_api_key = semantic::get_api_key().is_some();

                println!("Index status:");
                println!("  Files indexed: {}", count);
                println!("  Content indexed: {} files", content_count);
                if ocr_total > 0 {
                    println!("  OCR indexed: {}/{} images", ocr_done, ocr_total);
                }
                if has_api_key {
                    println!("  Semantic: {}/{} files embedded", embed_done, embed_total);
                    let hnsw_exists = semantic::hnsw_index_exists(&data_dir());
                    let hnsw_vecs = db.get_meta("hnsw_vector_count").unwrap_or(None)
                        .unwrap_or_else(|| "0".into());
                    if hnsw_exists {
                        println!("  HNSW index: {} vectors (built)", hnsw_vecs);
                    } else {
                        println!("  HNSW index: not built");
                    }
                } else {
                    println!("  Semantic: disabled (no API key)");
                }
                println!("  Last updated: {}", last_index);
                println!("  Last full reindex: {}", last_full);
                println!("  Index location: {}", data_dir().display());
            }
        },

        Commands::Track { path, action } => {
            if !std::path::Path::new(&path).exists() {
                eprintln!("Warning: path does not exist: {}", path);
            }
            let db = db::Database::open(&db_path())?;
            db.init_schema()?;
            db.record_interaction(&path, &action)?;
        }
        Commands::Doctor { json } => {
            let report = build_doctor_report();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", format_doctor_report(&report));
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

    // Read stored scan paths from DB, fall back to defaults
    let scan_paths: Vec<String> = db_result.as_ref().ok()
        .map(indexer::stored_or_default_paths)
        .unwrap_or_else(indexer::default_scan_paths);
    let paths_status: Vec<serde_json::Value> = scan_paths
        .iter()
        .map(|p| {
            let exists = std::path::Path::new(p).exists();
            serde_json::json!({ "path": p, "exists": exists })
        })
        .collect();

    let ocr_binary_found = content::find_ocr_binary().is_some();
    let fda = indexer::check_full_disk_access();

    let hnsw_exists = semantic::hnsw_index_exists(&index_dir);
    let hnsw_vector_count = db_result.as_ref().ok()
        .and_then(|db| db.get_meta("hnsw_vector_count").ok().flatten())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

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
        "hnsw": {
            "index_exists": hnsw_exists,
            "vector_count": hnsw_vector_count,
        },
        "content_index": {
            "path": content_index_path().to_string_lossy(),
            "size_bytes": content_dir_size,
        },
        "index_location": index_dir.to_string_lossy(),
        "scan_paths": paths_status,
        "permissions": {
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

    out.push_str("\nHNSW:\n");
    if report["hnsw"]["index_exists"].as_bool().unwrap_or(false) {
        out.push_str(&format!("  Status: built ({} vectors)\n", report["hnsw"]["vector_count"]));
    } else {
        out.push_str("  Status: not built\n");
    }

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
