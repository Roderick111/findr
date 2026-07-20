use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

static LOG_MUTEX: Mutex<()> = Mutex::new(());

/// Snap a byte offset to the nearest char boundary (avoids UTF-8 slice panics).
fn snap_to_char_boundary(s: &str, offset: usize, backward: bool) -> usize {
    let offset = offset.min(s.len());
    if s.is_char_boundary(offset) {
        return offset;
    }
    if backward {
        let mut i = offset;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    } else {
        let mut i = offset;
        while i < s.len() && !s.is_char_boundary(i) {
            i += 1;
        }
        i
    }
}

/// Append an error entry to data_dir/error.log (serialized across threads/processes).
pub fn log_error(context: &str, error: &str) {
    let _guard = LOG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let log_path = crate::platform::data_dir().join("error.log");

    // Ensure parent directory exists
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let timestamp = chrono::Utc::now().to_rfc3339();
    let entry = format!(
        "[{}] {}: {}\n",
        timestamp,
        crate::platform::redact_home_in_str(context),
        crate::platform::redact_home_in_str(error),
    );

    // Write entry with an exclusive file lock, then close before any truncation.
    let should_truncate =
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            lock_log_file(&file);
            let write_ok = file.write_all(entry.as_bytes()).is_ok();
            unlock_log_file(&file);
            write_ok
                && std::fs::metadata(&log_path)
                    .map(|m| m.len() > 1_000_000)
                    .unwrap_or(false)
        } else {
            false
        };
    // File handle dropped here — safe to truncate
    if should_truncate {
        if let Ok(content) = std::fs::read_to_string(&log_path) {
            let half = snap_to_char_boundary(&content, content.len() / 2, true);
            if let Some(pos) = content[half..].find('\n') {
                let start = snap_to_char_boundary(&content, half + pos + 1, false);
                let _ = std::fs::write(&log_path, &content[start..]);
            }
        }
    }
}

#[cfg(unix)]
fn lock_log_file(file: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
}

#[cfg(unix)]
fn unlock_log_file(file: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
fn lock_log_file(file: &std::fs::File) {
    let _ = fs4::fs_std::FileExt::lock_exclusive(file);
}

#[cfg(windows)]
fn unlock_log_file(file: &std::fs::File) {
    let _ = fs4::fs_std::FileExt::unlock(file);
}

/// Read last N lines from error log (capped at 64KB to prevent OOM on large logs).
pub fn read_recent_errors(max_lines: usize) -> String {
    use std::io::Read;
    let log_path = crate::platform::data_dir().join("error.log");
    const MAX_READ: u64 = 64 * 1024;

    let content = match std::fs::File::open(&log_path) {
        Ok(file) => {
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let mut reader = if size > MAX_READ {
                // Read only the tail
                use std::io::Seek;
                let mut f = file;
                let _ = f.seek(std::io::SeekFrom::End(-(MAX_READ as i64)));
                f.take(MAX_READ)
            } else {
                file.take(MAX_READ)
            };
            let mut buf = String::new();
            if reader.read_to_string(&mut buf).is_err() {
                return String::from("(error log not readable)");
            }
            buf
        }
        Err(_) => return String::from("(no errors logged)"),
    };

    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}
