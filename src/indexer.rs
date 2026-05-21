use anyhow::Result;
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::db::{Database, FileEntry};

const DEFAULT_SCAN_PATHS: &[&str] = &[
    "~/Documents",
    "~/Desktop",
    "~/Downloads",
    "~/Projects",
    "~/Pictures",
];

const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    ".git",
    ".DS_Store",
    "Library/Caches",
    "Library/Application Support",
    ".Trash",
    ".cargo",
    ".rustup",
    "target",
    ".npm",
    ".bun",
    ".venv",
    ".mypy_cache",
    "__pycache__",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".eggs",
    "*.egg-info",
    ".cache",
    ".gradle",
    ".idea",
    ".vscode",
    ".next",
    ".nuxt",
    "dist",
    "build",
    ".turbo",
    ".parcel-cache",
    ".angular",
    "Pods",
    ".dart_tool",
    ".pub-cache",
];

/// High-traffic folders where files get modified/downloaded most often.
/// Pass 1 (shallow) only scans these for modifications.
const HOT_FOLDERS: &[&str] = &[
    "~/Downloads",
    "~/Desktop",
    "~/Documents",
];

const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024; // 100MB

/// Returns expanded default scan paths.
pub fn default_scan_paths() -> Vec<String> {
    DEFAULT_SCAN_PATHS.iter().map(|p| expand_tilde(p)).collect()
}

/// Directories excluded specifically for the "full_home" preset.
const FULL_HOME_EXCLUDES: &[&str] = &[
    "Library",
];

/// OS directories excluded for the "everything" preset.
const EVERYTHING_OS_EXCLUDES: &[&str] = &[
    "/System",
    "/Library",
    "/usr",
    "/bin",
    "/sbin",
    "/private",
    "/cores",
    "/Volumes/Recovery",
];

/// Return scan paths for a named preset, optionally merged with custom additional paths.
/// Custom paths are additive — duplicates and paths that are subdirectories of
/// existing preset paths are included (deduped by exact match only).
pub fn scan_paths_for_preset(preset: &str, custom: Option<&str>) -> Vec<String> {
    let mut paths = match preset {
        "personal" => default_scan_paths(),
        "full_home" => {
            let home = expand_tilde("~");
            vec![home]
        }
        "everything" => {
            let mut paths = vec![expand_tilde("~")];
            // Add all mounted volumes except Recovery
            let volumes = Path::new("/Volumes");
            if volumes.exists() {
                if let Ok(entries) = std::fs::read_dir(volumes) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        let name = p.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if name == "Recovery" || name == "Macintosh HD" {
                            continue;
                        }
                        if p.is_dir() {
                            paths.push(p.to_string_lossy().to_string());
                        }
                    }
                }
            }
            paths
        }
        _ => default_scan_paths(),
    };

    // Merge custom paths (additive, deduplicated)
    if let Some(custom_str) = custom {
        for p in custom_str.split(',') {
            let expanded = expand_tilde(p.trim());
            if !expanded.is_empty() && !paths.contains(&expanded) {
                paths.push(expanded);
            }
        }
    }

    paths
}

/// Read stored scan paths from DB metadata, falling back to defaults.
/// Reconstructs from stored preset + custom paths to stay consistent.
pub fn stored_or_default_paths(db: &crate::db::Database) -> Vec<String> {
    // Try preset + custom reconstruction first
    if let Ok(Some(preset)) = db.get_meta("scan_preset") {
        let custom = db.get_meta("custom_paths").ok().flatten();
        return scan_paths_for_preset(&preset, custom.as_deref());
    }

    // Fall back to stored flat path list
    if let Ok(Some(stored)) = db.get_meta("scan_paths") {
        let paths: Vec<String> = stored.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    default_scan_paths()
}

/// Check if a path should be excluded for the "full_home" preset.
pub fn should_exclude_full_home(path: &Path) -> bool {
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        for excl in FULL_HOME_EXCLUDES {
            if name == *excl {
                return true;
            }
        }
    }
    false
}

/// Check if a path should be excluded for the "everything" preset.
pub fn should_exclude_everything(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    for excl in EVERYTHING_OS_EXCLUDES {
        if path_str.starts_with(excl) {
            return true;
        }
    }
    // Also exclude .app bundles contents
    if path_str.contains(".app/Contents") {
        return true;
    }
    false
}

pub struct IndexStats {
    pub files_indexed: usize,
    pub dirs_scanned: usize,
    pub errors: usize,
    pub elapsed_ms: u128,
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path == "~" {
        if let Some(home) = dirs_home() {
            return path.replacen("~", &home, 1);
        }
    }
    path.to_string()
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok()
}

fn should_exclude(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    for exclude in DEFAULT_EXCLUDES {
        if exclude.contains('/') {
            // Multi-component pattern like "Library/Caches": substring match with separators
            let pattern = format!("/{}/", exclude);
            let pattern_end = format!("/{}", exclude);
            if path_str.contains(&pattern) || path_str.ends_with(&pattern_end) {
                return true;
            }
        } else if let Some(suffix) = exclude.strip_prefix('*') {
            // Glob suffix pattern like "*.egg-info": match on any component
            for component in path.components() {
                if component.as_os_str().to_string_lossy().ends_with(suffix) {
                    return true;
                }
            }
        } else {
            // Exact component match: "node_modules", ".git", "build", "dist", etc.
            for component in path.components() {
                if component.as_os_str() == *exclude {
                    return true;
                }
            }
        }
    }
    false
}

/// Two-pass quick diff:
///   Pass 1 (depth 3): Shallow scan of all scan paths — catches modified existing files
///   Pass 2 (depth 20 + dir pruning): Deep scan — catches new files anywhere
/// Combined: ~300-700ms, covers both edits and new files at any depth.
pub fn quick_diff(db: &Database) -> Result<Vec<(String, String, Option<String>)>> {
    let last_ts = db.max_modified_ts()?;
    if last_ts == 0 {
        return Ok(vec![]); // No index exists yet
    }

    let default_paths = stored_or_default_paths(db);
    let preset = db.get_meta("scan_preset").ok().flatten();
    let preset_ref = preset.as_deref();
    let indexed = db.get_all_paths_map()?; // O(1) lookup instead of N+1 db.get_mtime() calls
    let mut new_files: Vec<FileEntry> = Vec::new();
    let mut updated_files: Vec<(String, String, Option<String>)> = Vec::new();
    let mut result: Vec<(String, String, Option<String>)> = Vec::new();

    // === Pass 1: Shallow scan (depth 3) of hot folders — catch modifications ===
    let hot_paths: Vec<String> = HOT_FOLDERS.iter().map(|p| expand_tilde(p)).collect();
    for scan_path in &hot_paths {
        let path = Path::new(scan_path.as_str());
        if !path.exists() {
            continue;
        }

        let walker = WalkBuilder::new(path)
            .hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .max_depth(Some(3))
            .build();

        for entry in walker.into_iter().flatten() {
            let entry_path = entry.path();
            if should_exclude(entry_path) || should_exclude_for_preset(entry_path, preset_ref) || entry_path.is_dir() {
                continue;
            }

            let metadata = match entry_path.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            if metadata.len() > MAX_FILE_SIZE {
                continue;
            }

            let modified_ts = file_mtime(&metadata);
            let path_str = entry_path.to_string_lossy().to_string();

            // Check if file exists in index with different mtime (O(1) hashmap lookup)
            match indexed.get(&path_str) {
                Some(&(stored_ts, _)) if stored_ts < modified_ts => {
                    // File was modified — update index
                    let _ = db.update_file(&path_str, metadata.len(), modified_ts);
                    let filename = entry_path.file_name()
                        .map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let extension = entry_path.extension()
                        .map(|e| e.to_string_lossy().to_lowercase());
                    updated_files.push((path_str, filename, extension));
                }
                None if modified_ts > last_ts => {
                    // New file — will be caught by pass 1 or pass 2
                    // Handle here for shallow new files to avoid double-processing
                    let filename = entry_path.file_name()
                        .map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let extension = entry_path.extension()
                        .map(|e| e.to_string_lossy().to_lowercase());

                    result.push((path_str.clone(), filename.clone(), extension.clone()));
                    new_files.push(FileEntry {
                        path: path_str,
                        filename,
                        extension,
                        size_bytes: metadata.len(),
                        modified_ts,
                        created_ts: file_birthtime(&metadata),
                        is_dir: false,
                    });
                }
                _ => {} // Unchanged or older
            }
        }
    }

    // === Pass 2: Deep scan with depth-aware mtime pruning ===
    // Depths ≤5: always check files (catches in-place edits that don't change dir mtime)
    // Depths >5: skip if dir mtime unchanged (optimization for deep trees)
    for scan_path in &default_paths {
        let root = Path::new(scan_path.as_str());
        if !root.exists() {
            continue;
        }

        let mut dirs_to_visit: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

        while let Some((dir, depth)) = dirs_to_visit.pop() {
            if should_exclude(&dir) || should_exclude_for_preset(&dir, preset_ref) {
                continue;
            }

            let dir_mtime = dir.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            // Only use dir-mtime pruning for deep directories
            let dir_has_changes = depth <= 5 || dir_mtime > last_ts;

            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for entry in entries.flatten() {
                let entry_path = entry.path();

                if should_exclude(&entry_path) || should_exclude_for_preset(&entry_path, preset_ref) {
                    continue;
                }

                let metadata = match entry_path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                if metadata.is_dir() {
                    if depth < 20 {
                        dirs_to_visit.push((entry_path, depth + 1));
                    }
                    continue;
                }

                if !dir_has_changes {
                    continue;
                }

                if metadata.len() > MAX_FILE_SIZE {
                    continue;
                }

                let modified_ts = file_mtime(&metadata);
                if modified_ts <= last_ts {
                    continue;
                }

                let path_str = entry_path.to_string_lossy().to_string();

                // Skip if already indexed (by pass 1 or previous index) — O(1) hashmap lookup
                if indexed.contains_key(&path_str) {
                    continue;
                }

                let filename = entry_path.file_name()
                    .map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let extension = entry_path.extension()
                    .map(|e| e.to_string_lossy().to_lowercase());

                result.push((path_str.clone(), filename.clone(), extension.clone()));
                new_files.push(FileEntry {
                    path: path_str,
                    filename,
                    extension,
                    size_bytes: metadata.len(),
                    modified_ts,
                    created_ts: file_birthtime(&metadata),
                    is_dir: false,
                });
            }
        }
    }

    // Persist new files
    if !new_files.is_empty() {
        db.insert_files_batch(&new_files)?;
    }

    if !new_files.is_empty() || !updated_files.is_empty() {
        db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339())?;
    }

    // Include updated files in result so content can be re-indexed
    result.extend(updated_files);

    Ok(result)
}

fn file_mtime(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn file_birthtime(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .created()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Check if Full Disk Access is likely granted by testing read access to protected folders.
pub fn check_full_disk_access() -> (bool, Vec<String>) {
    let protected = ["~/Documents", "~/Desktop", "~/Downloads"];
    let mut inaccessible = Vec::new();

    for p in &protected {
        let expanded = expand_tilde(p);
        let path = Path::new(&expanded);
        if path.exists() {
            match std::fs::read_dir(path) {
                Ok(mut entries) => {
                    // Try to actually read an entry — some permission errors only surface here
                    if let Some(Err(_)) = entries.next() {
                        inaccessible.push(p.to_string());
                    }
                }
                Err(_) => {
                    inaccessible.push(p.to_string());
                }
            }
        }
    }

    (inaccessible.is_empty(), inaccessible)
}

pub struct DiffResult {
    pub new_files: Vec<FileEntry>,
    pub modified_files: Vec<FileEntry>,
    pub deleted_paths: Vec<String>,
    pub dirs_scanned: usize,
    pub errors: usize,
    pub elapsed_ms: u128,
}

/// Walk filesystem and compare against SQLite to produce three change sets.
/// Does NOT modify any state — caller applies the changes.
pub fn compute_diff(db: &Database) -> Result<DiffResult> {
    let start = std::time::Instant::now();
    let mut dirs_scanned = 0;
    let mut errors = 0;

    // Load all indexed paths for O(1) lookup. Remove entries as we see them on disk.
    // Whatever remains after the walk = deleted files.
    let mut indexed: HashMap<String, (i64, u64)> = db.get_all_paths_map()?;

    let default_paths = stored_or_default_paths(db);
    let preset = db.get_meta("scan_preset").ok().flatten();
    let preset_ref = preset.as_deref();
    let mut new_files: Vec<FileEntry> = Vec::new();
    let mut modified_files: Vec<FileEntry> = Vec::new();

    for scan_path in &default_paths {
        let path = Path::new(scan_path.as_str());
        if !path.exists() {
            continue;
        }

        let walker = WalkBuilder::new(path)
            .hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .max_depth(Some(20))
            .build();

        for entry in walker {
            match entry {
                Ok(entry) => {
                    let entry_path = entry.path();
                    if should_exclude(entry_path) || should_exclude_for_preset(entry_path, preset_ref) {
                        continue;
                    }
                    if entry_path.is_dir() {
                        dirs_scanned += 1;

                        // Index directory for folder search
                        if let Some(dir_name) = entry_path.file_name() {
                            let dir_mtime = entry_path.metadata().ok()
                                .map(|m| file_mtime(&m))
                                .unwrap_or(0);
                            let path_str = entry_path.to_string_lossy().to_string();
                            let dir_entry = FileEntry {
                                path: path_str.clone(),
                                filename: dir_name.to_string_lossy().to_string(),
                                extension: None,
                                size_bytes: 0,
                                modified_ts: dir_mtime,
                                created_ts: 0,
                                is_dir: true,
                            };
                            match indexed.remove(&path_str) {
                                Some((stored_ts, _)) => {
                                    if dir_mtime > stored_ts {
                                        modified_files.push(dir_entry);
                                    }
                                }
                                None => {
                                    new_files.push(dir_entry);
                                }
                            }
                        }
                        continue;
                    }

                    let metadata = match entry_path.metadata() {
                        Ok(m) => m,
                        Err(_) => { errors += 1; continue; }
                    };
                    if metadata.len() > MAX_FILE_SIZE {
                        continue;
                    }

                    let path_str = entry_path.to_string_lossy().to_string();
                    let modified_ts = file_mtime(&metadata);
                    let filename = entry_path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let extension = entry_path.extension()
                        .map(|e| e.to_string_lossy().to_lowercase());

                    let file_entry = FileEntry {
                        path: path_str.clone(),
                        filename,
                        extension,
                        size_bytes: metadata.len(),
                        modified_ts,
                        created_ts: file_birthtime(&metadata),
                        is_dir: false,
                    };

                    match indexed.remove(&path_str) {
                        Some((stored_ts, _)) => {
                            if modified_ts > stored_ts {
                                modified_files.push(file_entry);
                            }
                            // else: unchanged, skip
                        }
                        None => {
                            new_files.push(file_entry);
                        }
                    }
                }
                Err(_) => { errors += 1; }
            }
        }
    }

    // Remaining entries in indexed = deleted from disk
    let deleted_paths: Vec<String> = indexed.into_keys().collect();

    Ok(DiffResult {
        new_files,
        modified_files,
        deleted_paths,
        dirs_scanned,
        errors,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

/// Process FSEvents changes into index updates.
/// Filters against exclusions, resolves renames, handles must-scan dirs.
/// Returns (files_to_update, paths_to_delete).
pub fn process_fsevents(
    result: &crate::fsevents::FsEventResult,
) -> (Vec<FileEntry>, Vec<String>) {
    let mut to_update: Vec<FileEntry> = Vec::new();
    let mut to_delete: Vec<String> = Vec::new();

    for change in &result.changes {
        // Handle must-scan directories: walk them shallowly
        if change.must_scan_dir {
            let dir = Path::new(&change.path);
            if dir.is_dir() {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.is_file() && !should_exclude(&p) {
                            if let Ok(meta) = p.metadata() {
                                if meta.len() <= MAX_FILE_SIZE {
                                    let filename = p.file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    let extension = p.extension()
                                        .map(|e| e.to_string_lossy().to_lowercase());
                                    to_update.push(FileEntry {
                                        path: p.to_string_lossy().to_string(),
                                        filename,
                                        extension,
                                        size_bytes: meta.len(),
                                        modified_ts: file_mtime(&meta),
                                        created_ts: file_birthtime(&meta),
                                        is_dir: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }

        let path = Path::new(&change.path);

        // Filter exclusions
        if should_exclude(path) {
            continue;
        }

        // Handle removed files
        if change.removed {
            to_delete.push(change.path.clone());
            continue;
        }

        // Handle renames: check if file exists at path
        if change.renamed {
            if crate::fsevents::resolve_rename(&change.path) {
                // File exists at this path — it was renamed TO here (treat as new/modified)
            } else {
                // File gone from this path — it was renamed FROM here (treat as deleted)
                to_delete.push(change.path.clone());
                continue;
            }
        }

        // Handle created/modified: read metadata and build entry
        if change.created || change.modified || change.renamed {
            if !path.exists() || !path.is_file() {
                continue;
            }
            let meta = match path.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() > MAX_FILE_SIZE {
                continue;
            }
            let filename = path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let extension = path.extension()
                .map(|e| e.to_string_lossy().to_lowercase());
            to_update.push(FileEntry {
                path: change.path.clone(),
                filename,
                extension,
                size_bytes: meta.len(),
                modified_ts: file_mtime(&meta),
                created_ts: file_birthtime(&meta),
                is_dir: false,
            });
        }
    }

    (to_update, to_delete)
}

/// Check if a path should be excluded based on the active scan preset.
/// Returns true if the path should be skipped.
fn should_exclude_for_preset(path: &Path, preset: Option<&str>) -> bool {
    match preset {
        Some("full_home") => should_exclude_full_home(path),
        Some("everything") => should_exclude_everything(path),
        _ => false,
    }
}

pub fn build_index(db: &Database, scan_paths: Option<&[String]>, preset: Option<&str>) -> Result<IndexStats> {
    let start = std::time::Instant::now();
    let mut files_indexed = 0;
    let mut dirs_scanned = 0;
    let mut errors = 0;

    // Check Full Disk Access
    let (fda_ok, inaccessible) = check_full_disk_access();
    if !fda_ok {
        eprintln!("Warning: Some folders are not accessible (Full Disk Access may be required):");
        for p in &inaccessible {
            eprintln!("  - {}", p);
        }
        eprintln!("Grant access: System Settings → Privacy & Security → Full Disk Access → add findr");
        eprintln!();
        crate::errors::log_error("fda", &format!("inaccessible folders: {:?}", inaccessible));
    }

    let default_paths: Vec<String> = DEFAULT_SCAN_PATHS.iter().map(|p| expand_tilde(p)).collect();
    let paths = scan_paths.unwrap_or(&default_paths);

    db.clear()?;

    let mut batch: Vec<FileEntry> = Vec::with_capacity(5000);

    for scan_path in paths {
        let path = Path::new(scan_path);
        if !path.exists() {
            eprintln!("Scan path does not exist: {}", scan_path);
            continue;
        }

        let walker = WalkBuilder::new(path)
            .hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .max_depth(Some(20))
            .build();

        for entry in walker {
            match entry {
                Ok(entry) => {
                    let entry_path = entry.path();

                    if should_exclude(entry_path) || should_exclude_for_preset(entry_path, preset) {
                        continue;
                    }

                    if entry_path.is_dir() {
                        dirs_scanned += 1;

                        // Index the directory itself for folder search
                        if let Some(dir_name) = entry_path.file_name() {
                            let dir_mtime = entry_path.metadata().ok()
                                .map(|m| file_mtime(&m))
                                .unwrap_or(0);
                            batch.push(FileEntry {
                                path: entry_path.to_string_lossy().to_string(),
                                filename: dir_name.to_string_lossy().to_string(),
                                extension: None,
                                size_bytes: 0,
                                modified_ts: dir_mtime,
                                created_ts: 0,
                                is_dir: true,
                            });
                        }
                        continue;
                    }

                    let metadata = match entry_path.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            errors += 1;
                            continue;
                        }
                    };

                    if metadata.len() > MAX_FILE_SIZE {
                        continue;
                    }

                    let modified_ts = metadata
                        .modified()
                        .unwrap_or(SystemTime::UNIX_EPOCH)
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;

                    let filename = entry_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    let extension = entry_path
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase());

                    batch.push(FileEntry {
                        path: entry_path.to_string_lossy().to_string(),
                        filename,
                        extension,
                        size_bytes: metadata.len(),
                        modified_ts,
                        created_ts: file_birthtime(&metadata),
                        is_dir: false,
                    });

                    if batch.len() >= 5000 {
                        files_indexed += db.insert_files_batch(&batch)?;
                        batch.clear();
                        eprint!("\r  Scanning: {} files found...", files_indexed);
                    }
                }
                Err(_) => {
                    errors += 1;
                }
            }
        }
    }

    if !batch.is_empty() {
        files_indexed += db.insert_files_batch(&batch)?;
    }

    let elapsed_ms = start.elapsed().as_millis();

    db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339())?;
    db.set_meta("file_count", &files_indexed.to_string())?;

    Ok(IndexStats {
        files_indexed,
        dirs_scanned,
        errors,
        elapsed_ms,
    })
}
