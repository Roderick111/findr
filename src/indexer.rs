use anyhow::Result;
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::db::{Database, FileEntry};

fn platform_scan_paths() -> &'static [&'static str] {
    crate::platform::default_scan_paths()
}

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
fn hot_folders() -> &'static [&'static str] {
    crate::platform::hot_folders()
}

const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024; // 100MB

/// Returns expanded default scan paths.
pub fn default_scan_paths() -> Vec<String> {
    platform_scan_paths().iter().map(|p| expand_tilde(p)).collect()
}

/// Directories excluded specifically for the "full_home" preset.
fn full_home_excludes() -> &'static [&'static str] {
    crate::platform::home_excludes()
}

/// OS directories excluded for the "everything" preset.
fn everything_os_excludes() -> &'static [&'static str] {
    crate::platform::os_excludes()
}

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
            paths.extend(crate::platform::extra_volume_paths());
            paths
        }
        other => {
            eprintln!("Warning: unknown scan preset '{other}', using 'personal'");
            default_scan_paths()
        }
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
        for excl in full_home_excludes() {
            if name == *excl {
                return true;
            }
        }
    }
    false
}

/// Check if a path should be excluded for the "everything" preset.
pub fn should_exclude_everything(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    let path_str = crate::platform::normalize_path_str(&raw);
    for excl in everything_os_excludes() {
        if path_str.starts_with(excl) {
            return true;
        }
    }
    // Also exclude OS-specific bundles (e.g. .app/Contents on macOS)
    if crate::platform::should_exclude_os_bundle(&path_str) {
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

pub fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path.starts_with("~\\") || path == "~" {
        if let Some(home) = dirs_home() {
            return path.replacen("~", &home, 1);
        }
    }
    path.to_string()
}

fn dirs_home() -> Option<String> {
    crate::platform::home_dir()
}

/// Pre-computed exclude patterns to avoid format! allocations on every file.
struct ExcludePatterns {
    multi_component: Vec<(String, String)>, // (pattern "/{}/", pattern_end "/{}")
    glob_suffix: Vec<String>,               // suffix after "*"
    exact: Vec<String>,                     // exact component name
}

static EXCLUDE_PATTERNS: std::sync::OnceLock<ExcludePatterns> = std::sync::OnceLock::new();

fn get_exclude_patterns() -> &'static ExcludePatterns {
    EXCLUDE_PATTERNS.get_or_init(|| {
        let mut patterns = ExcludePatterns {
            multi_component: Vec::new(),
            glob_suffix: Vec::new(),
            exact: Vec::new(),
        };
        for exclude in DEFAULT_EXCLUDES {
            if exclude.contains('/') {
                patterns.multi_component.push((
                    format!("/{}/", exclude),
                    format!("/{}", exclude),
                ));
            } else if let Some(suffix) = exclude.strip_prefix('*') {
                patterns.glob_suffix.push(suffix.to_string());
            } else {
                patterns.exact.push(exclude.to_string());
            }
        }
        patterns
    })
}

fn should_exclude(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    let path_str = crate::platform::normalize_path_str(&raw);
    let patterns = get_exclude_patterns();

    for (pattern, pattern_end) in &patterns.multi_component {
        if path_str.contains(pattern.as_str()) || path_str.ends_with(pattern_end.as_str()) {
            return true;
        }
    }
    for suffix in &patterns.glob_suffix {
        for component in path.components() {
            if component.as_os_str().to_string_lossy().ends_with(suffix.as_str()) {
                return true;
            }
        }
    }
    for exact in &patterns.exact {
        for component in path.components() {
            if component.as_os_str() == exact.as_str() {
                return true;
            }
        }
    }
    false
}

/// Changes detected by quick_sync. Caller applies Tantivy first, then SQLite.
#[derive(Debug)]
pub struct QuickSyncResult {
    pub added: Vec<FileEntry>,
    pub modified: Vec<FileEntry>,
    pub deleted: Vec<String>,
}

impl QuickSyncResult {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }

    /// Paths that need content re-indexing (added + modified).
    pub fn changed_for_content(&self) -> Vec<(String, String, Option<String>)> {
        self.added
            .iter()
            .chain(self.modified.iter())
            .map(|f| (f.path.clone(), f.filename.clone(), f.extension.clone()))
            .collect()
    }
}

fn path_under_scan_roots(path: &str, scan_paths: &[String]) -> bool {
    let file_path = Path::new(path);
    scan_paths.iter().any(|root| {
        let root_path = Path::new(root.as_str());
        file_path == root_path || file_path.starts_with(root_path)
    })
}

/// Two-pass quick sync: walks filesystem and returns changes (no DB writes).
///   Pass 1 (depth 3): Shallow scan of hot folders — catches modified existing files
///   Pass 2 (depth 20 + dir pruning): Deep scan — catches new/modified files anywhere
/// Combined: ~300-700ms, covers both edits and new files at any depth.
pub fn quick_sync(db: &Database) -> Result<QuickSyncResult> {
    let last_ts = db.max_modified_ts()?;
    if last_ts == 0 {
        return Ok(QuickSyncResult {
            added: vec![],
            modified: vec![],
            deleted: vec![],
        }); // No index exists yet
    }

    let default_paths = stored_or_default_paths(db);
    let preset = db.get_meta("scan_preset").ok().flatten();
    let preset_ref = preset.as_deref();
    let indexed = db.get_all_paths_map()?; // O(1) lookup instead of N+1 db.get_mtime() calls
    let mut still_indexed: HashMap<String, (i64, u64)> = indexed.clone();
    let mut added_files: Vec<FileEntry> = Vec::new();
    let mut modified_files: Vec<FileEntry> = Vec::new();

    // === Pass 1: Shallow scan (depth 3) of hot folders — catch modifications ===
    let hot_paths: Vec<String> = hot_folders().iter().map(|p| expand_tilde(p)).collect();
    for scan_path in &hot_paths {
        let path = Path::new(scan_path.as_str());
        if !path.exists() {
            continue;
        }

        let walker = WalkBuilder::new(path)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
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
                Some(&(stored_ts, _)) => {
                    still_indexed.remove(&path_str);
                    if stored_ts < modified_ts {
                        let filename = entry_path.file_name()
                            .map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        let extension = entry_path.extension()
                            .map(|e| e.to_string_lossy().to_lowercase());
                        modified_files.push(FileEntry {
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
                None if modified_ts > last_ts => {
                    // New file — handle here for shallow new files to avoid double-processing
                    let filename = entry_path.file_name()
                        .map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let extension = entry_path.extension()
                        .map(|e| e.to_string_lossy().to_lowercase());
                    added_files.push(FileEntry {
                        path: path_str,
                        filename,
                        extension,
                        size_bytes: metadata.len(),
                        modified_ts,
                        created_ts: file_birthtime(&metadata),
                        is_dir: false,
                    });
                }
                None => {} // Older mtime new file — pass 2 may catch if under scan roots
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
                let path_str = entry_path.to_string_lossy().to_string();

                match indexed.get(&path_str) {
                    Some(&(stored_ts, _)) => {
                        still_indexed.remove(&path_str);
                        if stored_ts < modified_ts {
                            let filename = entry_path.file_name()
                                .map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                            let extension = entry_path.extension()
                                .map(|e| e.to_string_lossy().to_lowercase());
                            modified_files.push(FileEntry {
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
                    None => {
                        // New file not yet indexed
                        let filename = entry_path.file_name()
                            .map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        let extension = entry_path.extension()
                            .map(|e| e.to_string_lossy().to_lowercase());
                        added_files.push(FileEntry {
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
        }
    }

    // Indexed paths under scan roots that no longer exist on disk
    let deleted_paths: Vec<String> = still_indexed
        .keys()
        .filter(|p| path_under_scan_roots(p, &default_paths))
        .filter(|p| !Path::new(p.as_str()).exists())
        .cloned()
        .collect();

    Ok(QuickSyncResult {
        added: added_files,
        modified: modified_files,
        deleted: deleted_paths,
    })
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

/// Check platform-specific permissions (e.g. Full Disk Access on macOS).
pub fn check_full_disk_access() -> (bool, Vec<String>) {
    crate::platform::check_permissions()
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
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
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
#[cfg(target_os = "macos")]
pub fn process_fsevents(
    result: &crate::fsevents::FsEventResult,
    preset: Option<&str>,
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
                        if p.is_file()
                            && !should_exclude(&p)
                            && !should_exclude_for_preset(&p, preset)
                        {
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

        // Filter exclusions (global + preset-specific)
        if should_exclude(path) || should_exclude_for_preset(path, preset) {
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

    // Check platform-specific permissions
    let (perms_ok, inaccessible) = check_full_disk_access();
    if !perms_ok {
        eprintln!("Warning: Some folders are not accessible:");
        for p in &inaccessible {
            eprintln!("  - {}", p);
        }
        #[cfg(target_os = "macos")]
        eprintln!("Grant access: System Settings → Privacy & Security → Full Disk Access → add findr");
        #[cfg(not(target_os = "macos"))]
        eprintln!("Check folder permissions for the paths above.");
        eprintln!();
        crate::errors::log_error("permissions", &format!("inaccessible folders: {:?}", inaccessible));
    }

    let default_paths: Vec<String> = platform_scan_paths().iter().map(|p| expand_tilde(p)).collect();
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
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
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

    Ok(IndexStats {
        files_indexed,
        dirs_scanned,
        errors,
        elapsed_ms,
    })
}

/// Index a single directory additively (no clear, no rebuild).
/// Walks the path and inserts new files into the existing index.
pub fn index_single_path(db: &Database, scan_path: &str, preset: Option<&str>) -> Result<IndexStats> {
    let start = std::time::Instant::now();
    let mut files_indexed = 0;
    let mut dirs_scanned = 0;
    let mut errors = 0;

    let path = Path::new(scan_path);
    if !path.exists() {
        return Err(anyhow::anyhow!("Path does not exist: {}", scan_path));
    }

    let mut batch: Vec<FileEntry> = Vec::with_capacity(5000);

    let walker = WalkBuilder::new(path)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
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
                    Err(_) => { errors += 1; continue; }
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
                }
            }
            Err(_) => { errors += 1; }
        }
    }

    if !batch.is_empty() {
        files_indexed += db.insert_files_batch(&batch)?;
    }

    let elapsed_ms = start.elapsed().as_millis();
    db.set_meta("last_index_time", &chrono::Utc::now().to_rfc3339())?;

    Ok(IndexStats {
        files_indexed,
        dirs_scanned,
        errors,
        elapsed_ms,
    })
}

#[cfg(test)]
mod preset_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn should_exclude_for_preset_none_never_excludes() {
        assert!(!should_exclude_for_preset(Path::new("/any/path/file.txt"), None));
        assert!(!should_exclude_for_preset(Path::new("/any/path/file.txt"), Some("personal")));
    }

    #[test]
    fn should_exclude_for_preset_full_home_excludes_home_dirs() {
        for excl in crate::platform::home_excludes() {
            let path = Path::new("/Users/me").join(excl).join("cache.dat");
            assert!(
                should_exclude_for_preset(&path, Some("full_home")),
                "full_home should exclude path under {excl}: {}",
                path.display()
            );
        }
        assert!(!should_exclude_for_preset(
            Path::new("/Users/me/Documents/report.pdf"),
            Some("full_home")
        ));
    }

    #[test]
    fn should_exclude_for_preset_everything_excludes_os_paths() {
        for excl in crate::platform::os_excludes() {
            let path = Path::new(excl).join("bin").join("tool");
            assert!(
                should_exclude_for_preset(&path, Some("everything")),
                "everything should exclude under {excl}"
            );
        }
        assert!(!should_exclude_for_preset(
            Path::new("/Users/me/Documents/report.pdf"),
            Some("everything")
        ));
    }
}
