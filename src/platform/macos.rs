//! macOS platform implementation.
//! Preserves all existing behavior — ~/.findr, FSEvents, Apple Vision OCR, flock.

use std::fs::File;
use std::path::{Path, PathBuf};

// --- Data directory ---

pub fn data_dir() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".findr"),
        Err(_) => {
            eprintln!("Warning: HOME environment variable not set. Using /tmp/findr as fallback.");
            PathBuf::from("/tmp/findr")
        }
    }
}

pub fn home_dir() -> Option<String> {
    std::env::var("HOME").ok()
}

// --- File locking ---

pub fn try_lock_exclusive(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    ret == 0
}

// --- Directory permissions ---

pub fn secure_directory(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)) {
        crate::errors::log_error(
            "permissions",
            &format!("Failed to set 0700 on {}: {}", path.display(), e),
        );
    }
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
        "/System",
        "/Library",
        "/usr",
        "/bin",
        "/sbin",
        "/private",
        "/cores",
        "/Volumes/Recovery",
    ]
}

pub fn home_excludes() -> &'static [&'static str] {
    &["Library"]
}

pub fn extra_volume_paths() -> Vec<String> {
    let mut paths = Vec::new();
    let volumes = Path::new("/Volumes");
    if volumes.exists() {
        if let Ok(entries) = std::fs::read_dir(volumes) {
            for entry in entries.flatten() {
                let p = entry.path();
                let name = p
                    .file_name()
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

pub fn excluded_recent_patterns() -> &'static [&'static str] {
    &[
        "%/node_modules/%",
        "%/.git/%",
        "%/target/%",
        "%/.build/%",
        "%/__pycache__/%",
        "%/.venv/%",
        "%/dist/%",
        "%/.next/%",
        "%/.cache/%",
        "%.photoslibrary/%",
        "%.app/%",
        "%.xcodeproj/%",
        "%.xcworkspace/%",
        "%/Library/%",
    ]
}

pub fn should_exclude_os_bundle(path: &str) -> bool {
    path.contains(".app/Contents")
}

// --- Permissions check (Full Disk Access) ---

pub fn check_permissions() -> (bool, Vec<String>) {
    let protected = ["~/Documents", "~/Desktop", "~/Downloads"];
    let mut inaccessible = Vec::new();

    for p in &protected {
        let expanded = if let Some(home) = home_dir() {
            p.replacen("~", &home, 1)
        } else {
            continue;
        };
        let path = Path::new(&expanded);
        if path.exists() {
            match std::fs::read_dir(path) {
                Ok(mut entries) => {
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

// --- OCR binary discovery ---

pub fn find_ocr_binary() -> Option<PathBuf> {
    // 1. Same directory as current executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("findr-ocr");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // 2. ~/.local/bin
    if let Ok(home) = std::env::var("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin/findr-ocr");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

// --- Change detection ---
// FSEvents sync is handled directly in main.rs behind #[cfg(target_os = "macos")].
// No platform abstraction needed since only macOS has kernel change journals.
