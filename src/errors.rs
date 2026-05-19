use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// Append an error entry to ~/.findr/error.log
pub fn log_error(context: &str, error: &str) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let log_path = Path::new(&home).join(".findr").join("error.log");

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
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return String::from("(no HOME)"),
    };
    let log_path = Path::new(&home).join(".findr").join("error.log");

    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(max_lines);
            lines[start..].join("\n")
        }
        Err(_) => String::from("(no errors logged)"),
    }
}
