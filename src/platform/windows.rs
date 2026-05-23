//! Windows platform implementation.
//! Uses %APPDATA%, fs4 file locking, mtime-diff sync (no FSEvents).

use std::fs::File;
use std::path::{Path, PathBuf};

// --- Data directory ---

pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "findr")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| {
            match std::env::var("USERPROFILE") {
                Ok(home) => PathBuf::from(home).join("AppData\\Roaming\\findr"),
                Err(_) => {
                    eprintln!("Warning: USERPROFILE environment variable not set. Using C:\\Temp\\findr as fallback.");
                    PathBuf::from("C:\\Temp\\findr")
                }
            }
        })
}

pub fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE").ok()
}

// --- File locking ---

pub fn try_lock_exclusive(file: &File) -> bool {
    use fs4::fs_std::FileExt;
    file.try_lock_exclusive().is_ok()
}

// --- Directory permissions ---

pub fn secure_directory(_path: &Path) {
    // Windows uses ACLs, not Unix permissions. User directories are
    // already restricted to the owner by default. No-op.
}

// --- Scan paths ---

pub fn default_scan_paths() -> &'static [&'static str] {
    &[
        "~/Documents",
        "~/Desktop",
        "~/Downloads",
        "~/Projects",
        "~/Pictures",
    ]
}

pub fn hot_folders() -> &'static [&'static str] {
    &["~/Downloads", "~/Desktop", "~/Documents"]
}

pub fn os_excludes() -> &'static [&'static str] {
    &[
        "C:\\Windows",
        "C:\\Program Files",
        "C:\\Program Files (x86)",
        "C:\\ProgramData",
    ]
}

pub fn home_excludes() -> &'static [&'static str] {
    &["AppData\\Local\\Temp", "AppData\\Local\\Microsoft"]
}

pub fn extra_volume_paths() -> Vec<String> {
    let mut paths = Vec::new();
    // Check drive letters D: through Z:
    for letter in b'D'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        let path = Path::new(&drive);
        if path.exists() {
            paths.push(drive);
        }
    }
    paths
}

pub fn excluded_recent_patterns() -> &'static [&'static str] {
    &[
        "%\\node_modules\\%",
        "%\\.git\\%",
        "%\\target\\%",
        "%\\.build\\%",
        "%\\__pycache__\\%",
        "%\\.venv\\%",
        "%\\dist\\%",
        "%\\.next\\%",
        "%\\.cache\\%",
        "%\\AppData\\Local\\Temp\\%",
    ]
}

pub fn should_exclude_os_bundle(_path: &str) -> bool {
    false
}

// --- Permissions check ---

pub fn check_permissions() -> (bool, Vec<String>) {
    (true, vec![])
}

// --- OCR binary discovery ---

pub fn find_ocr_binary() -> Option<PathBuf> {
    // Windows uses ocrs in-process, no external binary needed.
    None
}

// --- Change detection (FSEvents not available on Windows) ---
// Windows uses mtime-diff via quick_sync. USN Journal support is a future enhancement.
