//! Linux platform implementation.
//! Uses XDG-compliant paths, libc::flock, mtime-diff sync (no FSEvents).

use std::fs::File;
use std::path::{Path, PathBuf};

// --- Data directory ---

pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "findr")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| {
            match std::env::var("HOME") {
                Ok(home) => PathBuf::from(home).join(".local/share/findr"),
                Err(_) => {
                    eprintln!("Warning: HOME environment variable not set. Using /tmp/findr as fallback.");
                    PathBuf::from("/tmp/findr")
                }
            }
        })
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
    &["/proc", "/sys", "/dev", "/run", "/snap", "/boot"]
}

pub fn home_excludes() -> &'static [&'static str] {
    &[".cache", ".local/share/Trash"]
}

pub fn extra_volume_paths() -> Vec<String> {
    let mut paths = Vec::new();
    for mount_root in &["/mnt", "/media"] {
        let root = Path::new(mount_root);
        if root.exists() {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        paths.push(p.to_string_lossy().to_string());
                    }
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
        "%/.local/share/Trash/%",
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
    // Linux uses ocrs in-process, no external binary needed.
    // Return None — content.rs checks this for the external binary path only.
    None
}

// --- Change detection (FSEvents not available on Linux) ---
// Linux uses mtime-diff via quick_sync. No kernel change journal.
