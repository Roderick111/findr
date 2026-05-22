use std::fs::OpenOptions;
use std::io::Write;

/// Append an error entry to data_dir/error.log
pub fn log_error(context: &str, error: &str) {
    let log_path = crate::platform::data_dir().join("error.log");

    // Ensure parent directory exists
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let timestamp = chrono::Utc::now().to_rfc3339();
    let entry = format!("[{}] {}: {}\n", timestamp, context, error);

    // Write entry, then close file handle before any truncation
    let should_truncate = if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = file.write_all(entry.as_bytes());
        std::fs::metadata(&log_path).map(|m| m.len() > 1_000_000).unwrap_or(false)
    } else {
        false
    };
    // File handle dropped here — safe to truncate
    if should_truncate {
        if let Ok(content) = std::fs::read_to_string(&log_path) {
            let half = content.len() / 2;
            if let Some(pos) = content[half..].find('\n') {
                let _ = std::fs::write(&log_path, &content[half + pos + 1..]);
            }
        }
    }
}

/// Read last N lines from error log
pub fn read_recent_errors(max_lines: usize) -> String {
    let log_path = crate::platform::data_dir().join("error.log");

    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(max_lines);
            lines[start..].join("\n")
        }
        Err(_) => String::from("(no errors logged)"),
    }
}
