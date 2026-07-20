use findr::content;
use findr::db;
use findr::errors;
use findr::indexer;
use findr::pipeline;
use findr::platform;
use findr::search;
use findr::semantic;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            let dir = platform::data_dir();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("Warning: failed to create {}: {}", dir.display(), e);
            }
            platform::secure_directory(&dir);
            dir
        })
        .clone()
}

/// Try to acquire an exclusive lock on data_dir/sync.lock.
/// Returns the File handle (holds lock until dropped), or None if already locked.
fn try_acquire_lock() -> Option<File> {
    let lock_path = data_dir().join("sync.lock");
    let file = File::create(&lock_path).ok()?;
    if platform::try_lock_exclusive(&file) {
        Some(file)
    } else {
        None
    }
}

/// Separate lock for embedding — allows parallel execution with OCR/sync.
fn try_acquire_embed_lock() -> Option<File> {
    let lock_path = data_dir().join("embed.lock");
    let file = File::create(&lock_path).ok()?;
    if platform::try_lock_exclusive(&file) {
        Some(file)
    } else {
        None
    }
}

fn db_path() -> PathBuf {
    data_dir().join("index.db")
}

fn content_index_path() -> PathBuf {
    data_dir().join("content_index")
}

/// Emit a standardized JSON search error and return Ok (exit 0).
/// `--json` search must never exit(1) — Raycast treats non-zero as a crash.
fn emit_json_error(query: &str, error: &str, hint: Option<&str>) -> Result<()> {
    let mut value = serde_json::json!({
        "query": query,
        "mode": "error",
        "elapsed_ms": 0,
        "total_results": 0,
        "results": [],
        "error": platform::redact_home_in_str(error),
    });
    if let Some(h) = hint {
        value["hint"] = serde_json::json!(platform::redact_home_in_str(h));
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn emit_search_json(response: &search::SearchResponse, sync_skipped: bool) -> Result<()> {
    let mut value = serde_json::to_value(response)?;
    if sync_skipped {
        value["sync_skipped"] = serde_json::json!(true);
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[derive(Parser)]
#[command(
    name = "findr",
    version,
    about = "The fastest local file search",
    after_help = "EXAMPLES:\n  findr search invoice\n  findr search \"resume pdf\"          # inline type filter\n  findr search main.rs --type rs      # explicit type filter\n  findr search \"projects /\"           # folder filter (trailing /)\n  findr search \"/brainform\"           # folder filter (leading /)\n  findr search \"dharma in:daily\"      # scope to folders named 'daily'\n  findr search \"report in:downloads\"  # scope to Downloads\n  findr search \"in:obsidian\"          # recent files in scope\n  findr search revolut --path ~/Docs  # explicit path filter\n  findr search revolut --snippet-length 500  # longer snippets\n  findr index status\n  findr index embed --status\n  findr doctor --json\n\nINLINE FILTERS:\n  Type: last word matching a known extension (pdf, png, docx, etc.)\n  Folder: trailing '/' or 'folder'/'dir' keyword\n  Scope: 'in:<name>' searches inside matching folders\n\nSEMANTIC SEARCH:\n  Set OPENROUTER_API_KEY env or create openrouter_key in data dir\n  Then run: findr index embed\n  Get a key at: https://openrouter.ai"
)]
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
        #[arg(long, value_parser = ["open", "finder", "copy", "preview", "trash"])]
        action: String,
    },
    /// Manage local configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
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
    Status {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
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
    /// Remove a user-added path and rebuild the effective scope
    RemovePath {
        /// User-added directory to remove
        path: String,
    },
    /// Run semantic embedding on pending files (requires OpenRouter API key)
    Embed {
        /// Show embedding status instead of running
        #[arg(long)]
        status: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Store OpenRouter API key in the protected findr data directory.
    SetKey { key: Option<String> },
    /// Report whether an OpenRouter API key is configured.
    GetKey,
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
        errors::log_error(
            "spawn",
            &format!("Failed to spawn background {:?}: {}", args, e),
        );
    }
}

fn spawn_background_sync() {
    spawn_background(&["index", "sync"]);
}
/// macOS: spawn background findr-ocr process. Linux/Windows: run ocrs inline (no subprocess).
fn run_ocr(_db: &db::Database) {
    #[cfg(target_os = "macos")]
    {
        let _ = _db;
        spawn_background(&["index", "ocr"]);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = _db;
        // Reopen DB — run_full_index swaps in a new file, old handle is stale
        let db = match db::Database::open(&db_path()) {
            Ok(d) => d,
            Err(_) => return,
        };
        let cidx = match content::ContentIndex::open_or_create(&content_index_path()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = pipeline::run_ocr_incremental(&db, &cidx, true);
    }
}
fn spawn_background_rebuild() {
    spawn_background(&["index", "rebuild"]);
}

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
struct ScanConfig {
    preset: String,
    custom_paths: Vec<String>,
    paths: Vec<String>,
}

fn resolve_scan_config(
    custom_paths: Option<&str>,
    preset: Option<&str>,
    db: &db::Database,
) -> ScanConfig {
    let effective_preset = preset
        .map(|s| s.to_string())
        .or_else(|| db.get_meta("scan_preset").ok().flatten())
        .unwrap_or_else(|| "personal".to_string());
    let effective_custom = custom_paths
        .map(indexer::parse_path_list)
        .unwrap_or_else(|| indexer::stored_custom_paths(db));
    let paths = indexer::scan_paths_for_preset_paths(&effective_preset, &effective_custom);

    ScanConfig {
        preset: effective_preset,
        custom_paths: effective_custom,
        paths,
    }
}

/// Store the scan configuration in DB metadata for future syncs.
fn store_scan_config(db: &db::Database, config: &ScanConfig) {
    if let Ok(encoded) = serde_json::to_string(&config.paths) {
        let _ = db.set_meta("scan_paths", &encoded);
    }
    let _ = db.set_meta("scan_preset", &config.preset);
    if let Ok(encoded) = serde_json::to_string(&config.custom_paths) {
        let _ = db.set_meta("custom_paths", &encoded);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search {
            query,
            r#type,
            json,
            limit,
            path,
            snippet_length,
            no_semantic,
            no_sync,
        } => {
            // Keep automation input bounded before it reaches collectors or
            // allocations. Preserve legacy limit=0 behavior (search code
            // normalizes it to one result).
            let limit = limit.min(1_000);
            let snippet_length = snippet_length.min(10_000);
            if query.trim().is_empty() && !json {
                eprintln!("Query cannot be empty.");
                std::process::exit(1);
            }

            // Guard against single-char queries — Nucleo matches nearly everything
            // with min_score=12, causing long search times.
            if query.trim().len() < 2 && !query.trim().is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "query": query,
                            "mode": "too_short",
                            "elapsed_ms": 0,
                            "total_results": 0,
                            "results": [],
                            "hint": "Type at least 2 characters"
                        })
                    );
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
                        return emit_json_error(
                            &query,
                            &format!("Index corrupt: {e}"),
                            Some("Run: findr index rebuild"),
                        );
                    }
                    eprintln!("Index corrupt ({}). Run: findr index rebuild", e);
                    std::process::exit(1);
                }
            };
            if let Err(e) = db.init_schema() {
                if json {
                    return emit_json_error(
                        &query,
                        &format!("Schema init failed: {e}"),
                        Some("Run: findr index rebuild"),
                    );
                }
                eprintln!("Schema init failed ({}). Run: findr index rebuild", e);
                std::process::exit(1);
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
                    let result = pipeline::run_full_index(
                        &db,
                        None,
                        &data_dir(),
                        &db_path(),
                        &content_index_path(),
                        true,
                    )?;
                    if result.spawn_ocr {
                        run_ocr(&db);
                    }
                    if result.spawn_embed {
                        spawn_background_embed();
                    }
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
                    let canonical = pb
                        .canonicalize()
                        .unwrap_or(pb)
                        .to_string_lossy()
                        .to_string();
                    vec![if canonical.ends_with('/') || canonical.ends_with('\\') {
                        canonical
                    } else {
                        format!("{}{}", canonical, std::path::MAIN_SEPARATOR)
                    }]
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
                let scope_paths: Vec<String> = scope_response
                    .results
                    .into_iter()
                    .map(|r| {
                        if r.path.ends_with('/') || r.path.ends_with('\\') {
                            r.path
                        } else {
                            format!("{}{}", r.path, std::path::MAIN_SEPARATOR)
                        }
                    })
                    .collect();
                if !scope_paths.is_empty() {
                    path_filter = scope_paths;
                }
                // If 0 matching folders → keep existing path_filter (fail open)
            }

            // Sync BEFORE returning results (recent files or search)
            // --no-sync: skip sync entirely (return cached results instantly)
            let _search_lock = if !no_sync { try_acquire_lock() } else { None };
            let schema_rebuild = if _search_lock.is_some() {
                let requested = pipeline::check_schema_version(&db, &content_index_path());
                pipeline::reconcile_if_needed(&db, &content_index_path());
                requested
            } else {
                false
            };

            // Incremental sync (FSEvents on macOS, quick_sync elsewhere)
            let sync_skipped = !no_sync && _search_lock.is_none();
            let new_count = if _search_lock.is_some() {
                #[cfg(target_os = "macos")]
                {
                    pipeline::fsevents_sync(&db, &content_index_path())
                }
                #[cfg(not(target_os = "macos"))]
                {
                    pipeline::incremental_sync(&db, &content_index_path())
                }
            } else {
                0
            };

            // Child processes must acquire the same OS lock. Release this
            // process's handle before spawning migration work.
            drop(_search_lock);
            if schema_rebuild {
                spawn_background_rebuild();
            }

            // Empty query (after scope extraction) → return recent files (now freshly synced)
            if clean_query.trim().is_empty() && json {
                let response = search::recent_files(&db, limit, &path_filter)?;
                return emit_search_json(&response, sync_skipped);
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
                    let qvec = pipeline::get_query_vector(&db, &_api_key, &clean_query)?;
                    let hnsw_dir = data_dir();

                    // Step 2: HNSW lookup (local, fast)
                    let hnsw_stale = if semantic::hnsw_index_exists(&hnsw_dir) {
                        let stored = db
                            .get_meta("hnsw_vector_count")
                            .ok()
                            .flatten()
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let (_, current) = db
                            .semantic_stats(semantic::EMBEDDABLE_EXTENSIONS)
                            .unwrap_or((0, 0));
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
                    let matches: Vec<(String, f32)> = raw_vecs
                        .into_iter()
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
                    if matches.is_empty() {
                        return None;
                    }
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
                return emit_search_json(&response, sync_skipped);
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
                            i + 1,
                            type_badge,
                            result.filename,
                            result.path,
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
                    eprintln!(
                        "Index already exists ({} files). Use 'findr index rebuild' to recreate.",
                        db.file_count().unwrap_or(0)
                    );
                    return Ok(());
                }
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => {
                        eprintln!("Another findr process is running. Try again later.");
                        std::process::exit(2);
                    }
                };

                let config = resolve_scan_config(paths.as_deref(), preset.as_deref(), &db);
                store_scan_config(&db, &config);
                let result = pipeline::run_full_index(
                    &db,
                    Some(&config.paths),
                    &data_dir(),
                    &db_path(),
                    &content_index_path(),
                    true,
                )?;
                if result.spawn_ocr {
                    run_ocr(&db);
                }
                if result.spawn_embed {
                    spawn_background_embed();
                }
            }
            IndexAction::Rebuild { paths, preset } => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => {
                        eprintln!("Another findr process is running. Try again later.");
                        std::process::exit(2);
                    }
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

                let config = resolve_scan_config(paths.as_deref(), preset.as_deref(), &db);
                store_scan_config(&db, &config);
                let result = pipeline::run_full_index(
                    &db,
                    Some(&config.paths),
                    &data_dir(),
                    &db_path(),
                    &content_index_path(),
                    true,
                )?;
                if result.spawn_ocr {
                    run_ocr(&db);
                }
                if result.spawn_embed {
                    spawn_background_embed();
                }
            }
            IndexAction::AddPath { path } => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => {
                        eprintln!("Another findr process is running. Skipping.");
                        std::process::exit(2);
                    }
                };
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;

                let expanded = indexer::expand_tilde(&path);
                let preset = db.get_meta("scan_preset").ok().flatten();

                eprintln!("Indexing {}...", expanded);
                let stats = indexer::index_single_path(&db, &expanded, preset.as_deref())?;
                eprintln!(
                    "  {} files, {} dirs in {}ms",
                    stats.files_indexed, stats.dirs_scanned, stats.elapsed_ms
                );

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

                // User paths extend the active preset and persist independently.
                let mut custom_paths = indexer::stored_custom_paths(&db);
                if !custom_paths.contains(&expanded) {
                    custom_paths.push(expanded);
                    let encoded = serde_json::to_string(&custom_paths)?;
                    let config = resolve_scan_config(Some(&encoded), None, &db);
                    store_scan_config(&db, &config);
                }
            }
            IndexAction::RemovePath { path } => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => {
                        eprintln!("Another findr process is running. Skipping.");
                        std::process::exit(2);
                    }
                };
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;
                let expanded = indexer::expand_tilde(&path);
                let mut custom_paths = indexer::stored_custom_paths(&db);
                let old_len = custom_paths.len();
                custom_paths.retain(|candidate| candidate != &expanded);
                if custom_paths.len() == old_len {
                    anyhow::bail!("path is controlled by the active preset or is not configured");
                }
                let encoded = serde_json::to_string(&custom_paths)?;
                let config = resolve_scan_config(Some(&encoded), None, &db);
                store_scan_config(&db, &config);
                pipeline::run_full_index(
                    &db,
                    Some(&config.paths),
                    &data_dir(),
                    &db_path(),
                    &content_index_path(),
                    true,
                )?;
            }
            IndexAction::Sync => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => {
                        eprintln!("Another findr process is running. Skipping.");
                        std::process::exit(2);
                    }
                };
                let db = db::Database::open(&db_path())?;
                pipeline::run_incremental_index(&db, &content_index_path(), true)?;
            }
            IndexAction::Ocr => {
                let _lock = match try_acquire_lock() {
                    Some(f) => f,
                    None => {
                        eprintln!("Another findr process is running. Skipping.");
                        std::process::exit(2);
                    }
                };
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;
                let cidx = content::ContentIndex::open_or_create(&content_index_path())?;
                let count = pipeline::run_ocr_incremental(&db, &cidx, true)?;
                eprintln!("OCR complete: {} images indexed", count);
            }
            IndexAction::Embed { status } => {
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;

                if status {
                    let (total, done) = db
                        .semantic_stats(semantic::EMBEDDABLE_EXTENSIONS)
                        .unwrap_or((0, 0));
                    let has_key = semantic::get_api_key().is_some();
                    println!("Semantic embedding status:");
                    println!(
                        "  API key: {}",
                        if has_key {
                            "configured"
                        } else {
                            "not configured"
                        }
                    );
                    println!("  Files embedded: {}/{}", done, total);
                    return Ok(());
                }

                let api_key = match semantic::get_api_key() {
                    Some(k) => k,
                    None => {
                        eprintln!(
                            "No API key. Set OPENROUTER_API_KEY or create {}/openrouter_key",
                            data_dir().display()
                        );
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

                let count = pipeline::run_embed_batch(&db, &api_key, true)?;
                eprintln!("Embedding complete: {} files embedded", count);

                // Rebuild HNSW index from all vectors
                if count > 0 {
                    pipeline::rebuild_hnsw_index(&db, &data_dir(), true);
                }
            }
            IndexAction::Status { json } => {
                let db = db::Database::open(&db_path())?;
                let count = db.file_count().unwrap_or(0);
                let last_index = db.get_meta("last_index_time")?.unwrap_or("never".into());
                let last_full = db
                    .get_meta("last_full_index_time")?
                    .unwrap_or("never".into());

                let content_count = content::ContentIndex::open_or_create(&content_index_path())
                    .and_then(|c| c.doc_count())
                    .unwrap_or(0);

                let (ocr_total, ocr_done) = db.ocr_stats(content::OCR_EXTENSIONS).unwrap_or((0, 0));
                let (embed_total, embed_done) = db
                    .semantic_stats(semantic::EMBEDDABLE_EXTENSIONS)
                    .unwrap_or((0, 0));
                let has_api_key = semantic::get_api_key().is_some();

                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "files_indexed": count,
                            "content_indexed": content_count,
                            "last_sync": last_index,
                            "last_full_index": last_full,
                        })
                    );
                    return Ok(());
                }

                println!("Index status:");
                println!("  Files indexed: {}", count);
                println!("  Content indexed: {} files", content_count);
                if ocr_total > 0 {
                    println!("  OCR indexed: {}/{} images", ocr_done, ocr_total);
                }
                if has_api_key {
                    println!("  Semantic: {}/{} files embedded", embed_done, embed_total);
                    let hnsw_exists = semantic::hnsw_index_exists(&data_dir());
                    let hnsw_vecs = db
                        .get_meta("hnsw_vector_count")
                        .unwrap_or(None)
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
        Commands::Config { action } => match action {
            ConfigAction::SetKey { key } => {
                let key = key
                    .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                    .unwrap_or_default();
                let key = key.trim();
                if key.is_empty() {
                    anyhow::bail!("API key cannot be empty");
                }
                let path = data_dir().join("openrouter_key");
                let mut options = std::fs::OpenOptions::new();
                options.create(true).truncate(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&path)?;
                file.write_all(key.as_bytes())?;
                file.write_all(b"\n")?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                }
                println!("API key configured");
            }
            ConfigAction::GetKey => {
                println!(
                    "{}",
                    if semantic::get_api_key().is_some() {
                        "configured"
                    } else {
                        "not configured"
                    }
                );
            }
        },
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
            let li = db
                .get_meta("last_index_time")
                .unwrap_or(None)
                .unwrap_or_else(|| "never".into());
            let lf = db
                .get_meta("last_full_index_time")
                .unwrap_or(None)
                .unwrap_or_else(|| "never".into());
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
    let scan_paths: Vec<String> = db_result
        .as_ref()
        .ok()
        .map(indexer::stored_or_default_paths)
        .unwrap_or_else(indexer::default_scan_paths);
    let custom_paths = db_result
        .as_ref()
        .ok()
        .map(indexer::stored_custom_paths)
        .unwrap_or_default();
    let paths_status: Vec<serde_json::Value> = scan_paths
        .iter()
        .map(|p| {
            let exists = std::path::Path::new(p).exists();
            let custom = custom_paths.contains(p);
            serde_json::json!({ "path": p, "exists": exists, "custom": custom })
        })
        .collect();

    let ocr_binary_found = content::ocr_available();
    let fda = indexer::check_full_disk_access();

    let hnsw_exists = semantic::hnsw_index_exists(&index_dir);
    let hnsw_vector_count = db_result
        .as_ref()
        .ok()
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
    out.push_str(&format!(
        "Findr v{}\n\n",
        report["version"].as_str().unwrap_or("?")
    ));

    out.push_str("Database:\n");
    out.push_str(&format!(
        "  Status: {}\n",
        if report["database"]["ok"].as_bool().unwrap_or(false) {
            "OK"
        } else {
            "ERROR"
        }
    ));
    out.push_str(&format!(
        "  Files indexed: {}\n",
        report["database"]["files_indexed"]
    ));
    out.push_str(&format!(
        "  Content indexed: {}\n",
        report["database"]["content_indexed"]
    ));
    out.push_str(&format!(
        "  Last updated: {}\n",
        report["database"]["last_updated"].as_str().unwrap_or("?")
    ));
    out.push_str(&format!(
        "  Last full reindex: {}\n",
        report["database"]["last_full_reindex"]
            .as_str()
            .unwrap_or("?")
    ));
    out.push_str(&format!(
        "  DB size: {} KB\n",
        report["database"]["size_bytes"].as_u64().unwrap_or(0) / 1024
    ));
    out.push_str(&format!(
        "  Content index size: {} KB\n",
        report["content_index"]["size_bytes"].as_u64().unwrap_or(0) / 1024
    ));

    out.push_str("\nOCR:\n");
    out.push_str(&format!(
        "  Binary found: {}\n",
        if report["ocr"]["binary_found"].as_bool().unwrap_or(false) {
            "YES"
        } else {
            "NO"
        }
    ));
    out.push_str(&format!(
        "  Images indexed: {}/{}\n",
        report["ocr"]["ocr_completed"], report["ocr"]["total_images"]
    ));

    out.push_str("\nHNSW:\n");
    if report["hnsw"]["index_exists"].as_bool().unwrap_or(false) {
        out.push_str(&format!(
            "  Status: built ({} vectors)\n",
            report["hnsw"]["vector_count"]
        ));
    } else {
        out.push_str("  Status: not built\n");
    }

    out.push_str("\nScan paths:\n");
    if let Some(paths) = report["scan_paths"].as_array() {
        for p in paths {
            let status = if p["exists"].as_bool().unwrap_or(false) {
                "OK"
            } else {
                "MISSING"
            };
            out.push_str(&format!(
                "  {} — {}\n",
                p["path"].as_str().unwrap_or("?"),
                status
            ));
        }
    }

    out.push_str(&format!(
        "\nRecent errors:\n{}\n",
        report["recent_errors"].as_str().unwrap_or("(none)")
    ));
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
