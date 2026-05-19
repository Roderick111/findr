mod content;
mod db;
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
#[command(name = "findr", about = "Fast local file search for macOS")]
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
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Manage the file index
    Index {
        #[command(subcommand)]
        action: IndexAction,
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

/// Layer 1: Quick diff — find new/modified files, index them.
fn quick_diff_sync(db: &db::Database, content_idx_path: &PathBuf) -> usize {
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

/// Layer 2: Full reindex in background if older than 7 days
fn background_full_reindex(db_path: PathBuf, content_idx_path: PathBuf) {
    std::thread::spawn(move || {
        let db = match db::Database::open(&db_path) {
            Ok(d) => d,
            Err(_) => return,
        };

        eprintln!("[bg] Layer 2: full reindex starting (index older than 7 days)");

        if let Err(e) = db.init_schema() {
            eprintln!("[bg] Schema init error: {}", e);
            return;
        }

        match indexer::build_index(&db, None) {
            Ok(stats) => {
                let _ = db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339());
                eprintln!("[bg] Layer 2: reindexed {} files in {}ms", stats.files_indexed, stats.elapsed_ms);
            }
            Err(e) => eprintln!("[bg] Reindex error: {}", e),
        }

        if let Ok(all_files) = db.get_all_paths() {
            let files: Vec<(String, String, Option<String>)> = all_files
                .into_iter()
                .map(|(path, filename, ext, _ts)| (path, filename, ext))
                .collect();

            if let Ok(cidx) = content::ContentIndex::open_or_create(&content_idx_path) {
                match cidx.index_files(&files) {
                    Ok(n) => eprintln!("[bg] Layer 2: content indexed {} files", n),
                    Err(e) => eprintln!("[bg] Content index error: {}", e),
                }
            }
        }
    });
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search { query, r#type, json, limit } => {
            let db = db::Database::open(&db_path())?;

            // Layer 1: Quick diff before search
            let new_count = quick_diff_sync(&db, &content_index_path());
            if new_count > 0 {
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
                    eprintln!("Try: findr index init");
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

            // Layer 2: Background full reindex if stale
            if needs_full_reindex(&db) {
                background_full_reindex(db_path(), content_index_path());
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        Commands::Index { action } => match action {
            IndexAction::Init { paths } | IndexAction::Rebuild { paths } => {
                let db = db::Database::open(&db_path())?;
                db.init_schema()?;

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

                eprintln!("Phase 1: Indexing file paths...");
                let stats = indexer::build_index(&db, scan_paths.as_deref())?;
                eprintln!(
                    "  {} files indexed, {} dirs scanned, {} errors in {}ms",
                    stats.files_indexed, stats.dirs_scanned, stats.errors, stats.elapsed_ms,
                );

                eprintln!("\nPhase 2: Indexing file contents...");
                let all_files: Vec<(String, String, Option<String>)> = db
                    .get_all_paths()?
                    .into_iter()
                    .map(|(path, filename, ext, _ts)| (path, filename, ext))
                    .collect();

                let cidx = content::ContentIndex::open_or_create(&content_index_path())?;
                let content_count = cidx.index_files(&all_files)?;
                eprintln!("  {} files with content indexed", content_count);

                db.set_meta("last_full_index_time", &chrono::Utc::now().to_rfc3339())?;
                eprintln!("\nDone. Ready to search.");
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
    }

    Ok(())
}
