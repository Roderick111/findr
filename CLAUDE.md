# findr

Rust CLI + Raycast extension for local file search. See PRD.md for full architecture.

## Build

```bash
cargo build --release              # Rust CLI
cd findr-ocr && swift build -c release  # Swift OCR helper (macOS only)
make deploy                        # builds both + copies to Raycast assets + codesigns
```

## Testing

### Run before every commit
```bash
cargo test                           # all tests, must pass
cargo clippy -- -D warnings          # zero warnings policy
cd raycast-extension && npx ray lint # ESLint + Prettier
```

### Run before every release
```bash
cargo audit                          # CVE check (RustSec advisory DB)
cargo machete                        # unused Cargo deps
cargo bloat --release --crates -n 20 # binary size tracking
```

### CRAP score (coverage risk analysis)
```bash
cargo llvm-cov --lcov --output-path lcov.info --lib --test golden --test cli
cargo crap --lcov lcov.info --top 20
```
High CRAP = complex + untested. Target: pipeline.rs and indexer.rs functions.

### Test suites
- `tests/golden.rs` — search quality (shared corpus via LazyLock)
- `tests/integration.rs` — tier ordering, content index, perf benchmarks
- `tests/cli.rs` — flag acceptance, JSON output, doctor diagnostics
- `tests/indexer_tests.rs` — build_index, compute_diff, quick_sync
- `tests/pipeline_tests.rs` — full/incremental index, reconcile, OCR

### Code review
9 parallel agents from `CODE_REVIEW_OBJECTIVES.md`. Tool-augmented reviewers (security, perf, dead code) run cargo audit/machete/bloat first, then manual review.

## Key files

- `src/search.rs` — unified_search, tiered ranking, scoring
- `src/db.rs` — SQLite schema, queries, interaction tracking
- `src/pipeline.rs` — index orchestration (extracted from main.rs)
- `src/indexer.rs` — filesystem walking, diff, sync
- `src/content.rs` — Tantivy index, text extraction, OCR
- `src/main.rs` — CLI dispatch, process spawning
- `raycast-extension/src/search.tsx` — Raycast UI
- `raycast-extension/src/utils.ts` — binary download, helpers

## Gotchas

- After any Rust change: `cp target/release/findr raycast-extension/assets/findr` (or `make deploy`). Raycast runs the bundled binary, not target/release.
- Never exit(1) in JSON mode — Raycast treats it as crash. Return JSON error + exit 0.
- Tantivy writes before SQLite writes in sync paths (crash-safe ordering).
- `FileRow`/`FilePathRow` are named structs, not tuples. Use field names.
- `quick_sync` (not quick_diff) — the function mutates the DB.
