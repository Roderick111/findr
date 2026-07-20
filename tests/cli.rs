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

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "--json mode must always produce output"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("JSON output should be valid JSON: {e}\nGot: {stdout}"));

    if output.status.success() {
        assert!(
            json.get("query").is_some(),
            "JSON should have 'query' field"
        );
        assert!(
            json.get("results").is_some(),
            "JSON should have 'results' field"
        );
        assert!(
            json.get("elapsed_ms").is_some(),
            "JSON should have 'elapsed_ms' field"
        );
    } else {
        // Non-zero exit (e.g. no index) must still produce JSON with an error field
        assert!(
            json.get("error").is_some(),
            "Non-zero exit in --json mode must include 'error' field, got: {}",
            stdout
        );
    }
}

#[test]
fn search_limit_flag() {
    let output = findr()
        .args(["search", "test", "--json", "--limit", "3"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "--json mode must always produce output"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("JSON output should be valid JSON: {e}\nGot: {stdout}"));

    if output.status.success() {
        if let Some(results) = json.get("results").and_then(|r| r.as_array()) {
            assert!(
                results.len() <= 3,
                "limit=3 but got {} results",
                results.len()
            );
        }
    } else {
        assert!(
            json.get("error").is_some(),
            "Non-zero exit in --json mode must include 'error' field, got: {}",
            stdout
        );
    }
}

#[test]
fn search_type_filter_accepted() {
    // Verify the --type flag is accepted and produces valid JSON regardless of index state.
    let output = findr()
        .args(["search", "test", "--json", "--type", "md"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "--json mode must always produce output"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--type flag should produce valid JSON: {e}\nGot: {stdout}"));

    if !output.status.success() {
        assert!(
            json.get("error").is_some(),
            "Non-zero exit in --json mode must include 'error' field, got: {}",
            stdout
        );
    }
}

// ── Index subcommand ──

#[test]
fn index_status_runs() {
    // Should succeed if index exists, or fail gracefully if not
    let output = findr().args(["index", "status"]).output().unwrap();

    // Either succeeds or gives a clear error — should never crash
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.is_empty(),
        "index status should produce some output"
    );
}

#[test]
fn index_status_json_flag_is_supported() {
    findr()
        .args(["index", "status", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn config_commands_are_discoverable() {
    findr()
        .args(["config", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("set-key"))
        .stdout(predicate::str::contains("get-key"));
}

#[test]
fn remove_path_command_is_discoverable() {
    Command::cargo_bin("findr")
        .unwrap()
        .args(["index", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("remove-path"));
}

// ── Doctor subcommand ──

#[test]
fn doctor_runs() {
    findr().arg("doctor").assert().success();
}

#[test]
fn doctor_json_output() {
    // doctor should always work regardless of index state
    let output = findr().args(["doctor", "--json"]).output().unwrap();

    assert!(
        output.status.success(),
        "doctor --json should always exit 0, got: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "doctor --json must produce output"
    );

    let _json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json should produce valid JSON: {e}\nGot: {stdout}"));
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
    findr().arg("nonexistent").assert().failure();
}

#[test]
fn search_missing_query_fails() {
    findr().arg("search").assert().failure();
}
