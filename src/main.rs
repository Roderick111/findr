mod content;
mod db;
mod errors;
mod indexer;
mod search;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    let dir = PathBuf::from(home).join(".findr");
    std::fs::create_dir_all(&dir).expect("Failed to create ~/.findr");
    dir
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
    /// Rebuild entire index
    Rebuild {
        /// Specific paths to scan (comma-separated)
        #[arg(long)]
        paths: Option<String>,
    },
}

/// Check if index exists and has files
fn index_exists(db: &db::Database) -> bool {
    db.file_count().unwrap_or(0) > 0
}

/// Check if full reindex is needed (older than 7 days)
fn needs_full_reindex(db: &db::Database) -> bool {
    let last_full = match db.get_meta("last_full_index_time") {
        Ok(Some(ts)) => ts,
        _ => return true,
    };
    let parsed = match chrono::DateTime::parse_from_rfc3339(&last_full) {
        Ok(dt) => dt,
        Err(_) => return true,
    };
    let age = chrono::Utc::now().signed_duration_since(parsed);
    age.num_days() >= 7
}

/// Run full index (paths + content). Used for first-run auto-index and manual init.
fn run_full_index(db: &db::Database, scan_paths: Option<&[String]>, verbose: bool) -> Result<()> {
    db.init_schema()?;

    if verbose { eprintln!("Phase 1: Indexing file paths..."); }
    let stats = indexer::build_index(db, scan_paths)?;
    if verbose {
        eprintln!(
            "  {} files indexed, {} dirs scanned, {} errors in {}ms",
            stats.files_indexed, stats.dirs_scanned, stats.errors, stats.elapsed_ms,
        );
        eprintln!("\nPhase 2: Indexing file contents...");
    }

    let all_files: Vec<(String, String, Option<String>)> = db
        .get_all_paths()?
        .into_iter()
        .map(|(path, filename, ext, _ts)| (path, filename, ext))
        .collect();

    let cidx = content::ContentIndex::open_or_create(&content_index_path())?;
    let content_count = cidx.index_files(&all_files)?;
    if verbose {
        eprintln!("  {} files with content indexed", content_count);
    }

    db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
    if verbose { eprintln!("\nDone. Ready to search."); }
    Ok(())
}

/// Layer 1: Quick diff — find new/modified files, index them.
fn quick_diff_sync(db: &db::Database, content_idx_path: &std::path::Path) -> usize {
    let new_files = match indexer::quick_diff(db) {
        Ok(f) => f,
        Err(_) => return 0,
    };

    if new_files.is_empty() {
        return 0;
    }

    if let Ok(cidx) = content::ContentIndex::open_or_create(content_idx_path) {
        let _ = cidx.index_new_files(&new_files);
    }

    new_files.len()
}

/// Layer 2: Full reindex as a separate process (survives parent exit).
fn spawn_background_reindex() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    // Spawn `findr index rebuild` as detached process
    let _ = std::process::Command::new(exe)
        .args(["index", "rebuild"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search { query, r#type, json, limit } => {
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
                    spawn_background_reindex();
                    return Ok(());
                } else {
                    eprintln!("First run detected. Building index...");
                    run_full_index(&db, None, true)?;
                }
            }

            // Layer 1: Quick diff before search
            let new_count = quick_diff_sync(&db, &content_index_path());
            if new_count > 0 && !json {
                eprintln!("[+] {} new files indexed from recent activity", new_count);
            }

            // Prepend type flag to query for the inline parser
            let effective_query = if let Some(ref t) = r#type {
                format!("{} {}", query, t)
            } else {
                query
            };

            // Unified search: filenames + content, tiered ranking
            let response = search::unified_search(
                &db,
                &content_index_path(),
                &effective_query,
                limit,
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

            // Layer 2: Background full reindex if stale (>7 days)
            if needs_full_reindex(&db) {
                spawn_background_reindex();
            }
        }

        Commands::Index { action } => match action {
            IndexAction::Init { paths } | IndexAction::Rebuild { paths } => {
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
            IndexAction::Status => {
                let db = db::Database::open(&db_path())?;
                let count = db.file_count().unwrap_or(0);
                let last_index = db.get_meta("last_index_time")?.unwrap_or("never".into());
                let last_full = db.get_meta("last_full_index_time")?.unwrap_or("never".into());

                let content_count = content::ContentIndex::open_or_create(&content_index_path())
                    .and_then(|c| c.doc_count())
                    .unwrap_or(0);

                eprintln!("Index status:");
                eprintln!("  Files indexed: {}", count);
                eprintln!("  Content indexed: {} files", content_count);
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
    let db_ok = db::Database::open(&db_path()).is_ok();
    let file_count = db::Database::open(&db_path())
        .and_then(|db| db.file_count())
        .unwrap_or(0);
    let content_count = content::ContentIndex::open_or_create(&content_index_path())
        .and_then(|c| c.doc_count())
        .unwrap_or(0);
    let last_index = db::Database::open(&db_path())
        .and_then(|db| db.get_meta("last_index_time"))
        .unwrap_or(None)
        .unwrap_or_else(|| "never".into());
    let last_full = db::Database::open(&db_path())
        .and_then(|db| db.get_meta("last_full_index_time"))
        .unwrap_or(None)
        .unwrap_or_else(|| "never".into());

    let index_dir = data_dir();
    let db_size = std::fs::metadata(db_path()).map(|m| m.len()).unwrap_or(0);
    let content_dir_size = walkdir_size(&content_index_path());

    let recent_errors = errors::read_recent_errors(20);

    // Check scan paths exist
    let scan_paths = ["~/Documents", "~/Desktop", "~/Downloads", "~/Projects", "~/Pictures"];
    let home = std::env::var("HOME").unwrap_or_default();
    let paths_status: Vec<serde_json::Value> = scan_paths
        .iter()
        .map(|p| {
            let expanded = p.replace("~", &home);
            let exists = std::path::Path::new(&expanded).exists();
            serde_json::json!({ "path": p, "exists": exists })
        })
        .collect();

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
        "content_index": {
            "path": content_index_path().to_string_lossy(),
            "size_bytes": content_dir_size,
        },
        "index_location": index_dir.to_string_lossy(),
        "scan_paths": paths_status,
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
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}
