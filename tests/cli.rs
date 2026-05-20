//! CLI subprocess tests — verify the actual findr binary behaves correctly.
//! Uses assert_cmd to spawn the real binary and check stdout/stderr/exit codes.

use assert_cmd::Command;
use predicates::prelude::*;

fn findr() -> Command {
    Command::cargo_bin("findr").unwrap()
}

// ── Help & Version ──

#[test]
fn help_flag() {
    findr()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("fastest local file search"))
        .stdout(predicate::str::contains("EXAMPLES"));
}

#[test]
fn version_flag() {
    findr()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("findr"));
}

// ── Search subcommand ──

#[test]
fn search_help() {
    findr()
        .args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Search query"));
}

#[test]
fn search_json_output_is_valid_json() {
    // Search for something unlikely to exist — should still return valid JSON
    let output = findr()
        .args(["search", "zzz_nonexistent_query_12345", "--json"])
        .output()
        .unwrap();

    // Might exit 0 (no results) or produce JSON output
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
            assert!(parsed.is_ok(), "JSON output should be valid JSON, got: {}", stdout);

            let json = parsed.unwrap();
            assert!(json.get("query").is_some(), "JSON should have 'query' field");
            assert!(json.get("results").is_some(), "JSON should have 'results' field");
            assert!(json.get("elapsed_ms").is_some(), "JSON should have 'elapsed_ms' field");
        }
    }
}

#[test]
fn search_limit_flag() {
    let output = findr()
        .args(["search", "test", "--json", "--limit", "3"])
        .output()
        .unwrap();

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            if let Some(results) = json.get("results").and_then(|r| r.as_array()) {
                assert!(results.len() <= 3, "limit=3 but got {} results", results.len());
            }
        }
    }
}

#[test]
fn search_type_filter_accepted() {
    // Just verify the --type flag is accepted and doesn't crash.
    // We can't assert on result content since it depends on the live index.
    let output = findr()
        .args(["search", "test", "--json", "--type", "md"])
        .output()
        .unwrap();

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
            assert!(parsed.is_ok(), "--type flag should produce valid JSON");
        }
    }
}

// ── Index subcommand ──

#[test]
fn index_status_runs() {
    // Should succeed if index exists, or fail gracefully if not
    let output = findr()
        .args(["index", "status"])
        .output()
        .unwrap();

    // Either succeeds or gives a clear error — should never crash
    let combined = format!("{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr));
    assert!(!combined.is_empty(), "index status should produce some output");
}

// ── Doctor subcommand ──

#[test]
fn doctor_runs() {
    findr()
        .arg("doctor")
        .assert()
        .success();
}

#[test]
fn doctor_json_output() {
    let output = findr()
        .args(["doctor", "--json"])
        .output()
        .unwrap();

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
            assert!(parsed.is_ok(), "doctor --json should produce valid JSON, got: {}", stdout);
        }
    }
}

// ── Error handling ──

#[test]
fn no_args_shows_help() {
    findr()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn unknown_subcommand_fails() {
    findr()
        .arg("nonexistent")
        .assert()
        .failure();
}

#[test]
fn search_missing_query_fails() {
    findr()
        .arg("search")
        .assert()
        .failure();
}
