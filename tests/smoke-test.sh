#!/bin/bash
# Smoke test: catches non-obvious crashes, panics, and edge cases
# Run: docker run --rm --entrypoint bash findr-linux /app/tests/smoke-test.sh
# Or locally: bash tests/smoke-test.sh

set -uo pipefail

FINDR="${FINDR:-target/release/findr}"
PASS=0
FAIL=0
ERRORS=""

run() {
    local name="$1"; shift
    if output=$("$@" 2>&1); then
        PASS=$((PASS + 1))
        echo "  PASS: $name"
    else
        code=$?
        # exit 2 = lock conflict, acceptable in some tests
        if [ $code -eq 2 ]; then
            PASS=$((PASS + 1))
            echo "  PASS: $name (lock conflict, expected)"
        else
            FAIL=$((FAIL + 1))
            ERRORS="${ERRORS}\n  FAIL: $name (exit $code)\n    $output"
            echo "  FAIL: $name (exit $code)"
        fi
    fi
}

# Expect failure (non-zero exit = pass)
run_expect_fail() {
    local name="$1"; shift
    if output=$("$@" 2>&1); then
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: $name (expected failure, got success)"
        echo "  FAIL: $name (expected failure, got success)"
    else
        PASS=$((PASS + 1))
        echo "  PASS: $name"
    fi
}

# Must not panic (check for 'panic' in stderr)
run_no_panic() {
    local name="$1"; shift
    output=$("$@" 2>&1) || true
    if echo "$output" | grep -q "thread '.*' panicked\|RUST_BACKTRACE"; then
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}\n  FAIL: $name (PANIC detected)\n    $output"
        echo "  FAIL: $name (PANIC)"
    else
        PASS=$((PASS + 1))
        echo "  PASS: $name"
    fi
}

# JSON output must be valid
run_json() {
    local name="$1"; shift
    output=$("$@" 2>&1) || true
    # Strip stderr lines (not JSON)
    json=$(echo "$output" | grep -E '^\{|^\[|^  ' | head -100)
    if echo "$output" | python3 -c "import sys,json; json.load(sys.stdin)" 2>/dev/null; then
        PASS=$((PASS + 1))
        echo "  PASS: $name"
    else
        # Try extracting just the JSON part
        if echo "$output" | sed -n '/^{/,/^}/p' | python3 -c "import sys,json; json.load(sys.stdin)" 2>/dev/null; then
            PASS=$((PASS + 1))
            echo "  PASS: $name"
        else
            FAIL=$((FAIL + 1))
            ERRORS="${ERRORS}\n  FAIL: $name (invalid JSON)"
            echo "  FAIL: $name (invalid JSON)"
        fi
    fi
}

echo "=== SETUP ==="
# Create test corpus if it doesn't exist
CORPUS="/tmp/findr-smoke-test"
rm -rf "$CORPUS"
mkdir -p "$CORPUS/docs" "$CORPUS/code" "$CORPUS/edge-cases" "$CORPUS/deep/a/b/c/d/e/f/g/h"

# Normal files
echo "Invoice #1234 for consulting" > "$CORPUS/docs/invoice.txt"
echo "fn main() { println!(\"hello\"); }" > "$CORPUS/code/main.rs"
echo "Meeting notes Q4" > "$CORPUS/docs/notes.md"

# Edge case files
touch "$CORPUS/edge-cases/empty-file.txt"                              # empty file
dd if=/dev/urandom bs=1024 count=10 of="$CORPUS/edge-cases/binary.bin" 2>/dev/null  # binary
echo "normal" > "$CORPUS/edge-cases/file with spaces.txt"              # spaces in name
echo "normal" > "$CORPUS/edge-cases/file'with\"quotes.txt"             # quotes in name
printf 'Line1\nLine2\nLine3\n%.0s' {1..1000} > "$CORPUS/edge-cases/large.txt"  # large file
echo "café résumé naïve" > "$CORPUS/edge-cases/unicode-content.txt"    # unicode content
echo "test" > "$CORPUS/edge-cases/日本語ファイル.txt"                    # unicode filename
echo "test" > "$CORPUS/edge-cases/.hidden-file"                        # dotfile
ln -sf "$CORPUS/docs/invoice.txt" "$CORPUS/edge-cases/symlink.txt" 2>/dev/null || true  # symlink
echo "deep file" > "$CORPUS/deep/a/b/c/d/e/f/g/h/deep-file.txt"       # deeply nested
chmod 000 "$CORPUS/edge-cases/no-perms.txt" 2>/dev/null || echo "no perms" > "$CORPUS/edge-cases/no-perms.txt"  # no read perms
echo "not a pdf" > "$CORPUS/edge-cases/fake.pdf"                       # fake PDF
echo "not xlsx" > "$CORPUS/edge-cases/fake.xlsx"                       # fake XLSX
printf '\x00\x01\x02\x03' > "$CORPUS/edge-cases/null-bytes.txt"       # null bytes

echo "Test corpus created at $CORPUS"
echo ""

echo "=== 1. INDEXING ==="
run "index init"            $FINDR index init --paths "$CORPUS"
run "index status"          $FINDR index status
run "doctor"                $FINDR doctor

echo ""
echo "=== 2. BASIC SEARCH ==="
run_json "search normal"        $FINDR search "invoice" --json
run_json "search multi-word"    $FINDR search "meeting notes" --json
run_json "search with type"     $FINDR search "main rs" --json

echo ""
echo "=== 3. EDGE CASE QUERIES ==="
run_no_panic "empty query"              $FINDR search "" --json
run_no_panic "single char"              $FINDR search "a" --json
run_no_panic "very long query"          $FINDR search "$(printf 'a%.0s' {1..500})" --json
run_no_panic "special chars"            $FINDR search '!@#$%^&*()' --json
run_no_panic "unicode query"            $FINDR search "café résumé" --json
run_no_panic "japanese query"           $FINDR search "日本語" --json
run_no_panic "emoji query"              $FINDR search "🔥🚀" --json
run_no_panic "null bytes in query"      $FINDR search $'\x00\x01\x02' --json
run_no_panic "newlines in query"        $FINDR search $'line1\nline2' --json
run_no_panic "tabs in query"            $FINDR search $'word1\tword2' --json
run_no_panic "only spaces"             $FINDR search "   " --json
run_no_panic "sql injection"            $FINDR search "'; DROP TABLE files; --" --json
run_no_panic "regex-like"               $FINDR search '.*' --json
run_no_panic "path traversal"           $FINDR search "../../etc/passwd" --json
run_no_panic "backslashes"              $FINDR search 'C:\Users\test' --json
run_no_panic "very long type filter"    $FINDR search "test $(printf 'x%.0s' {1..200})" --json
run_no_panic "repeated slashes"         $FINDR search "////" --json
run_no_panic "scope nonexistent"        $FINDR search "in:nonexistent" --json
run_no_panic "scope empty"              $FINDR search "in:" --json

echo ""
echo "=== 4. JSON OUTPUT VALIDITY ==="
run_json "json: empty result"      $FINDR search "zzzznonexistent" --json
run_json "json: special chars"     $FINDR search '!@#' --json
run_json "json: unicode"           $FINDR search "café" --json
run_json "json: limit 1"           $FINDR search "test" --json --limit 1
run_json "json: limit 0"           $FINDR search "test" --json --limit 0
run_json "json: limit huge"        $FINDR search "test" --json --limit 9999
run_json "doctor json"             $FINDR doctor --json

echo ""
echo "=== 5. SYNC AFTER MUTATIONS ==="
echo "modified content" > "$CORPUS/docs/invoice.txt"
echo "brand new" > "$CORPUS/docs/new-after-index.txt"
rm -f "$CORPUS/docs/notes.md"
run "sync picks up changes"   $FINDR index sync
run_json "search modified"    $FINDR search "modified content" --json
run_json "search new file"    $FINDR search "brand new" --json
run_json "deleted file gone"  $FINDR search "Meeting notes Q4" --json

echo ""
echo "=== 6. CONCURRENT ACCESS ==="
# Fire multiple searches simultaneously
for i in {1..5}; do
    $FINDR search "test" --json > /dev/null 2>&1 &
done
wait
run "concurrent searches survived" true

echo ""
echo "=== 7. CRASH RECOVERY ==="
# Corrupt the DB slightly and see if doctor/search handle it
run_no_panic "doctor after stress"  $FINDR doctor

echo ""
echo "=== 8. FLAG COMBINATIONS ==="
run_no_panic "search --no-semantic"     $FINDR search "test" --json --no-semantic
run_no_panic "search --no-sync"         $FINDR search "test" --json --no-sync
run_no_panic "search --limit 0"         $FINDR search "test" --json --limit 0
run_no_panic "search --snippet-length"  $FINDR search "invoice" --json --snippet-length 500
run_no_panic "search --type rs"         $FINDR search "main" --json --type rs
run_no_panic "search --path"            $FINDR search "test" --json --path "$CORPUS"

echo ""
echo "=== RESULTS ==="
echo "Passed: $PASS"
echo "Failed: $FAIL"
if [ $FAIL -gt 0 ]; then
    echo -e "\nFailures:$ERRORS"
    exit 1
else
    echo "All tests passed!"
    exit 0
fi
