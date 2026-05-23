# Findr — Product Requirements Document

> The fastest local file search. Cross-platform (macOS, Linux, Windows). Finds what your OS search can't.

## Problem

macOS Finder/Spotlight fails to find files that exist on disk. PDF bank statements, downloaded contracts, screenshots, project assets — Spotlight's index silently misses files, corrupts, or excludes content. Every existing alternative either depends on the same broken Spotlight index (Raycast, Alfred, HoudahSpot) or does brute-force filesystem crawls that are painfully slow (EasyFind, Find Any File).

**Core user pain:** "I know this file exists on my laptop. I roughly know its name — or something inside it. I cannot find it."

## Solution

A Rust CLI (`findr`) that maintains its own filesystem index and delivers instant search results. Cross-platform (macOS, Linux, Windows). Accessed through Raycast (macOS), Vicinae (Linux), or CLI directly.

## Non-Goals (v1)

- Not a Finder replacement (no file management, no folder browsing)
- Not a system-wide search replacement (not hooking into Spotlight/Windows Search)
- No cloud/network drive indexing
- ~~No semantic/AI search~~ — Added in v1.1 (optional, requires OpenRouter API key)
- No Apple Photos library integration (requires Photos.framework, separate feature)

## Architecture

```
User types in Raycast
        │
        ▼
┌──────────────────────────────┐
│   Raycast Extension (TS)     │  Bundled universal binary in assets
│   useExec → findr CLI        │  Zero setup for end users
└──────────┬───────────────────┘
           │  findr search "revolut" --json
           ▼
┌──────────────────────────────────────────┐
│          findr CLI (Rust binary)          │
│                                          │
│  ┌────────────┐  ┌─────────────────────┐ │
│  │   Nucleo    │  │      Tantivy        │ │
│  │   fuzzy     │  │      full-text      │ │
│  │  (parallel) │  │      content index  │ │
│  └────────────┘  └─────────────────────┘ │
│  ┌────────────┐  ┌─────────────────────┐ │
│  │ Levenshtein │  │   pdf-extract       │ │
│  │ typo match  │  │   + docx/xlsx (zip) │ │
│  └────────────┘  └─────────────────────┘ │
│  ┌────────────┐  ┌─────────────────────┐ │
│  │   HNSW     │  │   SQLite            │ │
│  │  semantic   │  │   file metadata +   │ │
│  │  ANN index  │  │   index meta        │ │
│  └────────────┘  └─────────────────────┘ │
│  ┌──────────────────────────────────────┐ │
│  │   Three-tier indexing engine          │ │
│  │   FSEvents → Incremental Diff → Full │ │
│  └──────────────────────────────────────┘ │
│  ┌──────────────────────────────────────┐ │
│  │   findr-ocr (Swift CLI helper)        │ │
│  │   Apple Vision OCR + EXIF extraction  │ │
│  └──────────────────────────────────────┘ │
│  ┌──────────────────────────────────────┐ │
│  │   Reverse geocoder (offline)          │ │
│  │   GeoNames → GPS to city/country     │ │
│  └──────────────────────────────────────┘ │
└──────────────────────────────────────────┘
```

### Source File Map

| Cluster | Responsibility | Files |
|---------|---------------|-------|
| Indexing Engine | Filesystem scanning, content extraction, DB management | `indexer.rs`, `content.rs`, `db.rs` |
| Search Core | Query parsing, fuzzy matching, BM25 content search, semantic retrieval, tiered ranking | `search.rs`, `semantic.rs` |
| Platform Layer | Cross-platform abstractions (paths, locking, OCR, sync) | `platform/mod.rs`, `platform/macos.rs`, `platform/linux.rs`, `platform/windows.rs`, `platform/ocr_engine.rs` |
| Pipeline | Index orchestration, sync, reconcile, OCR/embed batch, HNSW rebuild | `pipeline.rs` |
| System Integration | FSEvents monitoring (macOS), CLI entry point, subcommand dispatch | `fsevents.rs`, `main.rs` |
| Raycast/Vicinae UI | Search interface, result display, reindex command, bug reporting | `search.tsx`, `reindex.tsx`, `utils.ts`, `bug-report.ts`, `types.ts` |
| OCR (macOS) | Apple Vision OCR, EXIF extraction, reverse geocoding | `findr-ocr/Sources/main.swift` |
| OCR (Linux/Windows) | Pure-Rust OCR via ocrs/ONNX models | `platform/ocr_engine.rs` |
| Tests | Golden search quality tests, integration pipeline tests, CLI tests | `tests/golden.rs`, `tests/integration.rs`, `tests/cli.rs` |

**Critical hotspots** (highest regression risk): `db.rs` (touched by every module), `indexer.rs` (orchestrates db + content + fsevents + semantic), `search.rs` (all ranking logic).

### CLI Flags

| Flag | Effect |
|------|--------|
| `--json` | JSON output (for launchers/scripts) |
| `--limit N` | Max results (default 30) |
| `--type ext` | Filter by extension |
| `--path ~/dir` | Filter to directory |
| `--snippet-length N` | Content snippet length (default 200) |
| `--no-semantic` | Skip API call, fast filename+content only |
| `--no-sync` | Skip index sync, return cached results instantly |

### Extension UX Patterns

- **Two-phase search**: Phase 1 fires `--no-semantic` immediately (300ms debounce), Phase 2 fires with semantic 1s later. User sees fast results first, semantic enrichment arrives silently.
- **Stale-then-refresh recent files**: Phase 1 fires `--no-sync` for instant cached results, Phase 2 fires with sync in background. New downloads appear ~1-2s after opening.
- **File previews**: Images (raycast-height constrained), PDF thumbnails (qlmanage), text/code (syntax highlighted), CSV (markdown table), markdown (native rendering).
- **Query vector caching**: Repeat semantic queries return from SQLite cache (instant), no API call needed.

## Unified Search

Single query searches both filenames and file contents. No `--content` flag needed. Results ranked by tiered scoring:

```
Tier 1 (10000): Filename starts with query     "Brainform.md" for "brainform"
Tier 2 (5000):  Filename contains query         "AI Readiness Report — Brainform.pdf"
Tier 3 (3000):  Filename typo match (Levenshtein) "Brainform.md" for "brainfrm"
Tier 4 (2000):  Content match (Tantivy BM25)    RIB.pdf containing "Revolut"
Tier 5 (1500):  Semantic match (embedding)       business-plan.md for "venture capital"
Tier 6 (1000):  Filename fuzzy (Nucleo)          subsequence matches

Within each tier:
  + bm25_bonus (Tantivy BM25 score × 30, capped at 500 — content tier only)
  + file_type_bonus (PDFs +200, text +150, images +120, dev files +10)
  + recency_bonus (recent files score higher)
  + position_bonus (content matches near start of document score higher)
  + both_match_boost (+500 when file matches by both name and content)
  + semantic_similarity_bonus (cosine × 500 — semantic tier only)
  + interaction_boost (ln(weighted_count+1) × 150, capped at 500 — frequency-based)

Within-tier sort: results in the same scoring tier sort by modification date (newest first).
Tier bucketing prevents small bonus differences from overriding recency.

Interaction boost uses time-decay buckets:
  - Last 7 days: weight 1.0
  - Last 30 days: weight 0.5
  - Last 90 days: weight 0.2
  - Last 365 days: weight 0.1
  - Older: pruned
Log scale: 1 recent interaction → 104 boost, 5 → 269, 20 → 457. Never crosses tier boundaries.
```

## CLI Interface

```bash
# Search (unified — filenames + content in one query)
findr search "revolut"              # finds RIB.pdf via content match
findr search "resume pdf"           # inline type filter
findr search "brainfrm"             # typo tolerance finds "Brainform"
findr search "revolut" --json       # JSON output for Raycast
findr search "revolut" --limit 30   # default: 30 results
findr search "" --json              # recent files (mode: "recent")
findr search "projects /"           # folder filter (trailing /)
findr search "dharma in:daily"      # scope to folders named "daily"
findr search "report in:downloads"  # scope to Downloads
findr search "in:obsidian"          # scoped recent files
findr search revolut --path ~/Docs  # explicit path filter
findr search revolut --snippet-length 500  # longer content snippets

# Indexing
findr index init                    # first-time full index
findr index init --preset full_home # scan with preset scope
findr index status                  # stats, health, last sync times
findr index rebuild                 # full nuke + rebuild (manual)
findr index rebuild --preset everything --paths ~/Code  # preset + custom paths
findr index sync                    # incremental diff (usually automatic)
findr index ocr                     # run OCR on pending images (usually background)
findr index add-path ~/Projects     # index a new folder without rebuilding

# Interaction tracking
findr track /path/to/file --action open   # record interaction (open, finder, copy, preview)

# Diagnostics
findr doctor                        # health report (index, FDA, errors)
findr doctor --json                 # machine-readable for bug reports
```

## Three-Tier Indexing Architecture

No manual reindexing needed. Index stays fresh automatically.

```
Every search (~40ms):
  Tier 3 — FSEvents journal replay
    └── Reads macOS kernel change journal since last stored event ID
    └── Catches: new files, modifications, deletions, renames
    └── Near-instant — no filesystem walking
    └── Falls back to quick_diff if FSEvents unavailable

Every 24 hours (~4s, background):
  Tier 2 — Incremental diff
    └── Walks filesystem, compares against SQLite by {path, mtime}
    └── Only processes changes (new, modified, deleted)
    └── Tantivy delete-by-term + re-add (no full rebuild)
    └── Safety net for anything FSEvents missed

Manual only:
  Tier 1 — Full rebuild (~48s)
    └── findr index init (first run) or findr index rebuild
    └── Nukes SQLite + Tantivy, re-walks + re-extracts everything
    └── Text extraction parallelized via rayon (uses all CPU cores)
    └── OCR runs as background process after text indexing completes

Background (spawned after full rebuild):
  OCR phase — Apple Vision via findr-ocr
    └── Processes .png, .jpg, .jpeg, .heic images
    └── Scanned PDF fallback (pdf-extract yields <50 chars → OCR)
    └── Parallel: multiple findr-ocr processes via rayon
    └── Tracked in ocr_status table (skip already-processed on re-run)
    └── Search available immediately — OCR results appear as they complete
```

**Performance (measured on 9K files, 1218 images):**

| Scenario | Time |
|----------|------|
| No changes (steady state, FSEvents) | 40ms |
| New file detected (FSEvents) | 223ms |
| Deleted file detected (FSEvents) | 349ms |
| Incremental diff, no changes | 4.3s |
| Full rebuild (text, parallel) | 48s (347% CPU) |
| OCR background (1218 images, parallel) | ~2-3 min |

## Content Extraction

| File type | Extractor | Status |
|-----------|-----------|--------|
| PDF | pdf-extract (with panic catching + stderr suppression) | Done |
| Plain text (.txt, .md, .csv, .log, .json, .yml) | Direct read (capped 100KB) | Done |
| Source code (.rs, .ts, .js, .py, .go, etc.) | Direct read | Done |
| Word (.docx) | ZIP + XML parse (word/document.xml) | Done |
| Excel (.xlsx) | ZIP + XML parse (xl/sharedStrings.xml) | Done |
| Images (.png, .jpg, .jpeg, .heic) | Apple Vision OCR via findr-ocr (background) | Done |
| Scanned PDFs (image-only) | pdf-extract fallback → Apple Vision OCR | Done |
| Image EXIF metadata | Date taken, GPS → city/country (offline geocoding) | Done |

## Raycast Extension

**Zero setup:** Binary bundled in extension assets (universal arm64 + x86_64). Install from store → search immediately.

### Commands

**Search Files** — Unified search with detail panel:
- Left: filename list with type icons
- Right: content snippet, full path, file type, size, modified date, interaction count
- Quick Look preview via Cmd+Y
- Actions: Open, Show in Finder, Quick Look, Copy Path, Copy Filename, Report Bug (all except Quick Look track interactions)

**Rebuild Index** — Manual reindex with toast notification.

### Error Handling
- Auto-index on first run (shows "Building index..." state)
- One-click bug reporting → opens GitHub issue with `findr doctor` diagnostics pre-filled
- Error log at `~/.findr/error.log`
- Full Disk Access detection with instructions

## Tech Stack

| Component | Tool | Why |
|-----------|------|-----|
| CLI | Rust (clap) | Performance, access to all search/index crates |
| Fuzzy matching | Nucleo (parallel via rayon) | 6x faster than skim, parallel scoring across all CPU cores |
| Typo tolerance | Levenshtein (custom) | Catches "brainfrm" → "brainform" |
| Full-text index | Tantivy 0.22 | Embedded, BM25 scoring, delete-by-term updates |
| Metadata store | SQLite (rusqlite, WAL mode) | Reliable, zero-config, fast path lookups |
| PDF extraction | pdf-extract | Rust-native, panic catching for malformed PDFs |
| DOCX/XLSX | zip crate + XML strip | Lightweight, no external dependencies |
| File watching | FSEvents (fsevent-sys) | macOS kernel journal, no filesystem walking |
| Filesystem walking | ignore crate | For initial index and incremental diff fallback |
| OCR | findr-ocr (Swift CLI, Apple Vision) | Neural Engine on M-series, .accurate level |
| EXIF extraction | CGImageSource (via findr-ocr) | Date taken, GPS coordinates |
| Reverse geocoding | reverse_geocoder (GeoNames) | Offline GPS → city/country, 140K cities |
| Parallel extraction | rayon | Multi-core text extraction, parallel search scoring |
| Semantic ANN index | hnsw_rs (HNSW) | Approximate nearest neighbor for 512d vectors |
| Error logging | Custom (~/.findr/error.log) | PDF panics, extraction failures, FDA issues |
| Raycast extension | TypeScript + React | useExec for CLI calls, bundled binary |
| CI/CD | GitHub Actions | Auto-build universal binaries (Rust + Swift) on tag push |

## Testing & Quality

### Test Suite (198 tests)

| Suite | Count | What it tests | Runtime |
|-------|-------|---------------|---------|
| Unit (`src/lib.rs`) | 152 | Scoring, Levenshtein, content extraction, semantic, HNSW, query parsing | ~3.5s |
| Golden (`tests/golden.rs`) | 15 | End-to-end search quality against a 40-file corpus. Shared via `LazyLock` (built once). | ~0.2s |
| Integration (`tests/integration.rs`) | 19 | Tier ordering, recency, type filter, content index, 10K-file perf | ~9s |
| CLI (`tests/cli.rs`) | 12 | Flag acceptance, JSON output validity, doctor diagnostics | ~2s |

### Quality Tools

Run before every release:

```bash
cargo test                           # 198 tests
cargo clippy -- -D warnings          # lint (zero warnings policy)
cargo audit                          # CVE check (RustSec advisory DB)
cargo machete                        # unused Cargo dependencies
cargo bloat --release --crates -n 20 # binary size by crate
```

Run for coverage analysis:

```bash
cargo llvm-cov --lcov --output-path lcov.info --lib --test golden --test cli
cargo crap --lcov lcov.info --top 20
```

Raycast extension:

```bash
cd raycast-extension
npx ray lint --fix                   # ESLint + Prettier
bun run build                        # TypeScript compilation
```

### CRAP Score (Change Risk Anti-Patterns)

`cargo crap` combines cyclomatic complexity + test coverage into a single risk score. High CRAP = complex code with low test coverage = where bugs hide.

Formula: `CRAP(m) = complexity² × (1 - coverage)³ + complexity`

Current top risk functions (v1.4.2):

| CRAP | CC | Coverage | Function | File |
|------|-----|---------|----------|------|
| 3863 | 109 | 31.9% | `main` | main.rs |
| 1640 | 40 | 0% | `quick_sync` | indexer.rs |
| 930 | 30 | 0% | `run_full_index` | pipeline.rs |
| 812 | 28 | 0% | `run_incremental_index` | pipeline.rs |
| 506 | 22 | 0% | `build_index` | indexer.rs |
| 420 | 20 | 0% | `compute_diff` | indexer.rs |
| 420 | 20 | 0% | `fsevents_sync` | pipeline.rs |

**Coverage gap:** The entire indexing/sync pipeline (`pipeline.rs`, `indexer.rs`) has 0% test coverage. These functions are now `pub` and testable — tests are the next priority.

**Well-tested areas:** Search ranking (golden tests), content extraction (unit tests), semantic scoring (unit tests), HNSW index (unit tests).

### Code Review Process

Nine review agents run in parallel, each focused on a single objective (see `CODE_REVIEW_OBJECTIVES.md`):

1. Security (OWASP) — with `cargo audit`
2. Performance — with `cargo bloat`
3. Error Handling
4. Concurrency
5. Architecture
6. CLI Ergonomics
7. Testability
8. Dead Code & Dependencies — with `cargo machete`, `cargo tree --duplicates`
9. API Design

Tool-augmented agents (1, 2, 8) run CLI tools first, then do manual review, tagging every finding as TOOL-caught or MANUAL-only. Key insight from evaluation: every actionable finding came from manual LLM review. Tools confirm hygiene (clean deps, no CVEs); LLM provides the analysis.

## Build & Deployment

The Rust source code (`src/*.rs`) is compiled into a machine-executable binary (`findr`) via `cargo build --release`. Similarly, the Swift source (`findr-ocr/`) compiles to a `findr-ocr` binary. Users don't compile anything — they get pre-built binaries.

### Repository structure

Two GitHub repos are involved:

- **`Roderick111/findr`** — Main repo. Contains Rust CLI source, Swift OCR source, Raycast extension TypeScript source (at `raycast-extension/`), CI workflows, tests, and this PRD.
- **`Roderick111/extensions`** — Fork of `raycast/extensions` (the Raycast Store monorepo). Contains a frozen copy of the Raycast extension + compiled binaries in `extensions/findr/assets/`. PR #28127 submits this for Raycast Store review.

### How binaries reach users

The Raycast extension downloads `findr` and `findr-ocr` from GitHub Releases on first launch. Binaries are cached in Raycast's `supportPath/bin/` and reused across sessions. Downloads include optional SHA-256 checksum verification via `checksums.txt` release asset. No binaries are bundled in the extension package — this satisfies the Raycast Store guideline: "Don't bundle opaque binaries where sources are unavailable."

For CLI users (non-Raycast): `curl | bash` install script fetches the latest GitHub Release binary directly.

### Release & sync workflow

#### Step 1: Push to main + tag

```bash
git push origin main
git tag v1.x.x
git push origin v1.x.x
```

CI (`.github/workflows/ci.yml`) triggers on the tag push. Builds universal binaries (arm64 + x86_64) for both `findr` and `findr-ocr`, creates a GitHub Release with downloadable assets. Takes ~3 min.

#### Step 2: Verify CI passes

```bash
gh run list -R Roderick111/findr --limit 3     # check status
gh run view <run-id> --log-failed               # debug failures
```

**Common CI failure:** test compilation errors. The `tests/golden.rs` and `tests/integration.rs` construct `FileEntry` directly — if you add a field to `FileEntry` (like `created_ts`), tests break. Always run `cargo test --no-run` locally before tagging.

**If CI fails after tagging:** fix the issue, push to main, delete the old tag, retag:
```bash
git tag -d v1.x.x                              # delete local tag
git push origin :refs/tags/v1.x.x              # delete remote tag
git tag v1.x.x                                 # retag at HEAD
git push origin main v1.x.x                    # push both
```

#### Step 3: Users install via one-liner

```bash
curl -fsSL https://raw.githubusercontent.com/Roderick111/findr/main/install.sh | bash
```

The install script fetches the latest GitHub Release binary. No Rust toolchain needed.

#### Step 4: Update Raycast Store PR (only when ready)

```bash
# Download release assets
gh release download v1.x.x -R Roderick111/findr -D /tmp/findr-release

# Copy to extensions fork
cp /tmp/findr-release/findr-macos-universal raycast-extension/raycast-ext-sync/extensions/findr/assets/findr
cp /tmp/findr-release/findr-ocr-macos-universal raycast-extension/raycast-ext-sync/extensions/findr/assets/findr-ocr

# Push to fork
cd raycast-extension/raycast-ext-sync
git add -A && git commit -m "Update findr to v1.x.x" && git push
```

**Critical:** Code changes to `Roderick111/findr` do NOT automatically update the Raycast PR. The PR (#28127) contains a frozen snapshot of the binaries at a specific version. Only an explicit manual sync (step 4) updates the PR. This means bug fixes to the CLI are safe — they won't disrupt a pending Raycast review.

A local sync directory exists at `raycast-extension/raycast-ext-sync/` (blobless sparse checkout of the fork) to avoid cloning the full 10GB+ Raycast extensions monorepo.

#### Local development: bundled binary gotcha

The Raycast extension runs the binary from `raycast-extension/assets/findr`, NOT `target/release/findr`. After any Rust change, you must copy it:

```bash
cargo build --release
cp target/release/findr raycast-extension/assets/findr
cd raycast-extension && bun run build
```

Or use `make deploy` which does all three. **If you skip this, Raycast runs a stale binary** and new CLI features won't work — this has caused hours of debugging (see Bugs Fixed: stale bundled binary).

### Build commands

```bash
# Development iteration (fast, ~12s, arm64 only)
cargo build --profile dev-release

# Full release (slow, ~2min, LTO optimized)
cargo build --release

# Deploy to local Raycast (builds + copies + codesigns)
make deploy
```

## What's Implemented

- [x] Unified search (filenames + content, single query, tiered ranking)
- [x] Tiered ranking with file type bonus, recency, position scoring
- [x] Typo tolerance (Levenshtein for filenames, Tantivy fuzzy for content)
- [x] PDF, DOCX, XLSX, text, code content extraction
- [x] Three-tier auto-indexing (FSEvents → incremental diff → full rebuild)
- [x] FSEvents kernel journal integration (Tier 3)
- [x] Incremental diff with Tantivy delete-by-term (Tier 2)
- [x] Auto-index on first run
- [x] Raycast extension with bundled universal binary
- [x] Detail panel with content snippets + metadata
- [x] Quick Look preview (Cmd+Y)
- [x] One-click bug reporting with diagnostics
- [x] `findr doctor` diagnostic command
- [x] Error logging to ~/.findr/error.log
- [x] Full Disk Access detection
- [x] GitHub Actions CI (clippy + test + release build)
- [x] Install script (curl one-liner)
- [x] OCR via Apple Vision framework (images + scanned PDFs)
- [x] Swift CLI helper (findr-ocr) — batch mode, .accurate recognition, 10s timeout
- [x] Scanned PDF detection (pdf-extract <50 chars → OCR fallback)
- [x] EXIF metadata extraction (date taken, GPS coordinates)
- [x] Offline reverse geocoding (GPS → city/country via GeoNames)
- [x] Parallel text extraction via rayon (347% CPU utilization)
- [x] Background OCR (search available in ~48s, OCR runs after)
- [x] Parallel OCR batch processing (multiple findr-ocr processes)
- [x] OCR status tracking in SQLite (ocr_status table, skip re-processing)
- [x] `findr index ocr` subcommand for manual/background OCR
- [x] OCR stats in `index status` and `doctor` report
- [x] Graceful degradation when findr-ocr binary not found
- [x] CI/CD: Swift universal binary build + release asset
- [x] Atomic rebuild via double-buffer (temp files → rename on success)
- [x] Lockfile (~/.findr/sync.lock) for background process mutual exclusion
- [x] SQLite/Tantivy reconciliation check (detects drift, triggers re-index)
- [x] Separator-normalized filename matching ("code review" → "code_review")
- [x] Lazy snippet extraction (only final top-N results, not all candidates)
- [x] Levenshtein ASCII byte-level optimization
- [x] Security hardening: 0700 dir permissions, zip bomb protection, query escaping
- [x] Enforced lock checking on index commands
- [x] FSEvents incomplete replay detection + fallback
- [x] `make deploy` — builds both binaries, copies to Raycast assets, codesigns
- [x] `index init` skips if exists, `index rebuild` always rebuilds
- [x] `--help` with usage examples
- [x] Status/doctor output to stdout for piping
- [x] Semantic search via OpenRouter embedding API (optional, pplx-0.6b d=512)
- [x] Format-specific embed rules (md/pdf/docx full, code header-only, skip images)
- [x] `findr index embed` subcommand + `--status` flag
- [x] Separate `embed.lock` for parallel OCR + embedding
- [x] Embed hash (FNV-1a) prevents re-embedding unchanged files
- [x] FSEvents invalidation of semantic vectors on file change
- [x] Semantic tier in unified search (1500, between content and fuzzy)
- [x] BOTH_MATCH_BOOST for files found by both keyword and semantic
- [x] Raycast extension: OpenRouter API key preference (password field)
- [x] BM25 score used for within-tier content ranking
- [x] BM25 minimum score threshold (rejects noise matches < 20% of top)
- [x] PDF quality validation (rejects raw binary, PDF structure markers)
- [x] API key permission check (warns if ~/.findr/openrouter_key not 0600)
- [x] API error sanitization (redacts API key from log messages)
- [x] Parallel filename scoring (Nucleo + Levenshtein merged, rayon, >2K files threshold)
- [x] HNSW approximate nearest neighbor index for semantic search (hnsw_rs, DistCosine)
- [x] HNSW atomic rebuild (temp-dir build + rename swap, same pattern as SQLite)
- [x] HNSW catch_unwind safety (hnsw_rs panics on corrupt files → graceful fallback)
- [x] HNSW → brute-force cosine fallback chain (transparent to user)
- [x] HNSW staleness detection (vector count comparison, skips stale index)
- [x] HNSW parallel insert for large vector sets (>1000 vectors)
- [x] HNSW status in `findr doctor` and `findr index status`
- [x] HNSW roundtrip unit tests (build, query, delete, empty, nonexistent)
- [x] Scan scope presets (Personal/Full Home/Everything + additive custom paths)
- [x] Folder search (is_dir indexing, folder icon, Show in Finder action)
- [x] Folder filter via trailing `/` (e.g. `"projects /"`)
- [x] Recent files default view (empty query → 20 most recent files)
- [x] Debounced search input (300ms, prevents useExec racing)
- [x] Min 2-char query guard (CLI + Raycast, prevents Nucleo overload)
- [x] Schema v3 migration (ALTER TABLE is_dir, background rebuild on upgrade)
- [x] Preset-specific exclusions (full_home skips ~/Library, everything skips OS dirs)
- [x] `in:scope` inline path filter (discovers matching folders via search, scopes results)
- [x] `--path` CLI flag (explicit path scoping for agents)
- [x] `--snippet-length` CLI flag (configurable content snippet length, default 200)
- [x] Tier-bucketed sort (same-tier results sort by modification date, newest first)
- [x] `SearchOptions` struct (replaces 8 positional params on `unified_search`)
- [x] Killed-process callback guard (prevents intermittent Raycast "Command failed" errors)
- [x] UTF-8 boundary safety in binary file snippet extraction
- [x] Interaction frequency tracking (`interactions` table, `findr track` command)
- [x] Log-scaled interaction boost with time-decay (7d/30d/90d/365d buckets, capped at 500)
- [x] `findr track <path> --action <type>` CLI command (<10ms fire-and-forget)
- [x] `findr index add-path <dir>` additive single-directory indexing (no rebuild)
- [x] Raycast action tracking (Open, Show in Finder, Copy Path, Copy Filename)
- [x] Interaction count in Raycast detail metadata panel
- [x] Auto-detect new custom scan paths in Raycast preferences (10s debounce, index only new dirs)
- [x] Interaction pruning on search startup (>1 year old)

## OCR Architecture

```
findr index init / rebuild
  Phase 1: Filesystem walk → SQLite              (~5s)
  Phase 2: Parallel text extraction → Tantivy     (~48s, rayon)
    → Search available here ←
  Phase 3: Background OCR (spawned as detached process, sync.lock)
    └── findr-ocr (Swift CLI) called via batch mode
    └── Multiple processes in parallel (rayon)
    └── Apple Vision VNRecognizeTextRequest (.accurate)
    └── EXIF: date taken + GPS (→ city/country via reverse_geocoder)
    └── Confidence threshold: 0.3 (below = skip)
    └── Results written to Tantivy via update_files_with_content
    └── Status tracked in ocr_status table (path + mtime)

  Phase 4: Background semantic embedding (spawned in parallel, embed.lock)
    └── Runs alongside OCR (network-bound vs CPU-bound)
    └── Format-specific: md/txt 800ch, pdf/docx/xlsx 800ch, code 200ch, skip images
    └── Batched API calls (20 files per request) to OpenRouter
    └── FNV-1a hash check skips unchanged files on rebuild
    └── Vectors stored in semantic_vectors table (path, BLOB, mtime, hash)
    └── Only runs if OpenRouter API key is configured
    └── After embedding: rebuilds HNSW index (atomic temp-dir swap)

findr-ocr <path1> [path2] ...
  Input:  One or more image/PDF paths
  Output: One JSON line per file to stdout
    {"path": "...", "text": "...", "confidence": 0.85,
     "exif": {"date_taken": "2024-01-15T10:30:00Z", "gps": "48.856614,2.352222"}}
  Errors: Per-file in JSON, always exit 0
  Timeout: 10s per image via DispatchSemaphore
  PDF OCR: PDFDocument → render pages to CGImage → Vision per page
```

## Semantic Search (v1.1)

Optional embedding-based search that finds files by meaning, not just keywords. Requires an OpenRouter API key.

**Model:** pplx-embed-v1-0.6b via OpenRouter, 512 dimensions, $0.01/1M tokens (~$0.06 for 5K files).

**Preprocessing:** `"File: {filename}\n{first_line}\n\n{stripped_content[:800]}"` — tested 12+ configs across 6 models. Format-specific rules:
- Markdown/text: filename + title + stripped content (800ch)
- PDF/DOCX/XLSX: filename + extracted content (800ch, proper extractors)
- Source code: filename + first 200ch (header/imports only)
- CSV: filename + first 400ch (header + sample rows)
- Images/config/media/archives: skipped

**Indexing:** Background process (`findr index embed`), parallel to OCR. Uses `embed.lock` (separate from `sync.lock`). Embed hash (FNV-1a) prevents re-embedding unchanged files on rebuild. Incremental: FSEvents invalidates vectors for changed files.

**Search:** Tier 5 at score 1500 (between BM25 content at 2000 and fuzzy at 1000). Cosine similarity threshold 0.15. BOTH_MATCH_BOOST (+500) when file also found by keyword/filename. Query embedded via single API call (~100ms). HNSW approximate nearest neighbor index used for O(log n) vector search; brute-force cosine scan as fallback if index unavailable.

**Benchmark results (50 files, 20 queries):**

| Configuration | Top-1 | Top-3 |
|---|---|---|
| Keyword + BM25 only | 10/20 | 12/20 |
| + Semantic tier (pplx-0.6b) | 13/20 | 14/20 |
| OAI-3-small (best single model) | 16/20 | 17/20 |
| Gemini-2-preview (best recall) | 15/20 | 19/20 |

**Rejected approaches:** RRF fusion (flattened scores), cross-encoder reranking (Cohere Rerank 4 Fast dropped top-1), MiniLM reranking (wrong domain), chunking (noise from generic files). Simple preprocessing consistently beat sophisticated architectures.

**Setup:**
```bash
echo "sk-or-..." > ~/.findr/openrouter_key
chmod 600 ~/.findr/openrouter_key
findr index embed
```

Or set `OPENROUTER_API_KEY` env var, or configure in Raycast extension preferences.

## v2 — Large-Scale Optimization (Implemented)

Two changes targeting 500K+ files and 2-5TB datasets. See [V3_OPTIMIZATION.md](V3_OPTIMIZATION.md) for technical rationale.

### Bugs Fixed

- [x] **Recent files not showing (stale bundled binary)** — Raycast extension showed "Type to search" instead of recent files on empty query. Root cause: the bundled binary in `raycast-extension/assets/findr` was a stale build that predated the `recent_files` feature. The CLI returned `mode=unified` instead of `mode=recent`. **This is a recurring trap** — after any Rust CLI change, the bundled asset must be manually copied (`cp target/release/findr raycast-extension/assets/findr`). Symptoms are confusing because the CLI works fine in terminal but Raycast runs the old binary.
- [x] **Single-word search hangs on loading** — Root cause: `useExec` fired on every keystroke with no debounce. Typing "test" = 4 subprocess spawns racing. `keepPreviousData: true` kept `isLoading` true until ALL resolved. Fix: 300ms debounce via `useDebouncedValue` hook + min 2-char query guard in both Rust CLI and Raycast extension. CLI returns `mode: "too_short"` for single-char queries.
- [x] **Quick Look crashes Raycast on first preview** — Root cause: `isShowingDetail` dynamically flipping from `false` → `true` when results arrived caused Raycast native renderer to tear down and rebuild the layout. `quickLook` prop wasn't registered with native host when Cmd+Y fired during this transition. Fix: `isShowingDetail={true}` always (static). Crash frequency reduced significantly. Remaining intermittent crashes likely a Raycast framework bug with `quickLook` + `detail` on same `List.Item` (docs never show them combined). See `QUICKLOOK_CRASH_DEBUG.md` for full investigation.

### Change 1: Parallel Filename Scoring

Replaced single-threaded `Pattern::score()` loop with a merged Nucleo + Levenshtein pass using rayon `par_iter().map_init()`. Each worker thread gets its own `Matcher`, UTF-32 buffer, and Levenshtein buffers — zero contention. Sequential path for <2000 files avoids rayon overhead.

**Expected speedup at scale:**

| Files | Before (single-thread) | After (parallel) |
|-------|----------------------|-------------------|
| 10K | 60ms | ~8ms |
| 100K | ~600ms | ~80ms |
| 1M | ~6s | ~40ms |

### Change 2: HNSW Semantic Index

Replaced brute-force cosine scan (O(n)) with HNSW approximate nearest neighbor search (O(log n)) via `hnsw_rs`. SQLite remains the source of truth for vectors; HNSW is a derived acceleration structure.

**Architecture:**
- **Build:** After `findr index embed`, all vectors loaded from SQLite, inserted into HNSW (parallel for >1000 vectors), saved to `~/.findr/` via atomic temp-dir + rename swap
- **Query:** At search time, loads HNSW from disk, searches for top-K neighbors. Wrapped in `catch_unwind` because hnsw_rs panics on corrupt files
- **Fallback:** If HNSW index missing, corrupt, or stale → transparent brute-force cosine scan
- **Staleness:** Compares stored vector count against current SQLite count; skips stale index
- **Files:** `semantic.hnsw.data`, `semantic.hnsw.graph`, `semantic.paths` (ID→path mapping)

**HNSW parameters:** `max_nb_connection=16`, `ef_construction=200`, `ef_search=32`, `max_layer=16`

**Expected memory/speed at scale:**

| Files | Before (brute-force) | After (HNSW) |
|-------|---------------------|--------------|
| 10K | ~20MB, 10ms | ~100MB, ~1ms |
| 100K | ~200MB, 100ms | ~100MB, ~1ms |
| 1M | ~2GB, ~1s | ~100MB, ~1ms |

### Other v2 Features

- [x] **Scan scope presets** — Raycast dropdown preference with three presets + additive custom paths:
  - **Personal** (default): `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Pictures`, `~/Projects`
  - **Full Home**: Everything under `~/` except `~/Library`, caches, build artifacts
  - **Everything**: All mounted volumes, excluding OS dirs (`/System`, `/Library`, `/usr`, `/bin`, `/sbin`, `/private`, `.app/Contents`)
  - **Additional Scan Paths**: Comma-separated text field merged with selected preset (e.g. `~/Code, /Volumes/External`). Duplicates ignored.
  - Implementation: `--preset` flag on `findr index init/rebuild`, `--paths` for custom additions. Stored in DB meta (`scan_preset`, `custom_paths`, `scan_paths`). `stored_or_default_paths()` reads stored config for sync/diff operations. Preset-specific exclusions enforced via `should_exclude_for_preset()` during all walks (build_index, quick_diff, compute_diff). Schema v3 triggers background rebuild on upgrade.
- [x] **Folder search** — Directories indexed with `is_dir: true` in SQLite `files` table. Schema migration via `ALTER TABLE ADD COLUMN is_dir INTEGER NOT NULL DEFAULT 0`. Folders appear in search results ranked by filename matching (same tiers as files). Folders never appear in content/semantic search (no content to index). Raycast: folder icon, "Folder" tag accessory, primary action = Show in Finder. Trailing `/` filter: `"projects /"` returns only directories matching "projects" (skips Tantivy + semantic passes entirely).
- [x] **Recent files as default view** — When search bar is empty, show 20 most recently modified files ordered by `modified_ts DESC`. Filters out dev noise: excludes code extensions (rs, ts, js, py, go, json, toml, yaml, lock, css, sh, etc.), dev paths (node_modules, .git, target, .build, dist, .next, .cache, __pycache__, .venv), system bundles (.photoslibrary, .app, .xcodeproj, Library), dotfiles, and directories. Result: user-facing files only (PDFs, images, screenshots, docs). Schema includes `created_ts` column (macOS birthtime) but unused — editors reset birthtime on save via write-to-temp+rename. Uses raw `useEffect` + `execFile` (not `useCachedPromise` — stale cache caused original bug). CLI returns `mode: "recent"` for empty query + `--json`.

## v3 — Agentic Search & Quality (v1.3.0)

Features targeting AI agent usability and search quality.

- [x] **`in:scope` inline path filter** — `"dharma in:daily"` discovers folders matching "daily" via folder search (top 20 results), then scopes the real search to those folders. Works with type filters (`"report pdf in:downloads"`), folder filters (`"in:obsidian projects /"`), and empty queries (`"in:obsidian"` → scoped recent files). No hardcoded paths — uses existing search ranking for folder discovery.
- [x] **`--path` CLI flag** — Explicit path scoping for agents: `findr search "dharma" --path ~/Obsidian/Daily --json`. Tilde expansion, path canonicalization, trailing slash normalization.
- [x] **`--snippet-length` CLI flag** — Configurable content snippet length (default 200): `findr search "revolut" --snippet-length 500 --json`. Agents get enough context to judge relevance without a separate file read.
- [x] **Tier-bucketed sort** — Same-tier results sort by modification date (newest first). Prevents small score bonuses from overriding recency within the same match quality.
- [x] **`SearchOptions` struct** — Replaces 8 positional parameters on `unified_search` with a named struct. Extensible without breaking callers.
- [x] **12 bug fixes** — UTF-8 panic on binary files, killed-process callback race in Raycast, exit(1) in JSON mode, scoped recent files returning 0, N+1 DB queries, API key permission handling, and more (see commit log).

## v4 — Interaction Frequency & Additive Indexing

- [x] **Interaction frequency tracking** — New `interactions` SQLite table stores per-file action events (open, finder, copy, preview) with timestamps. `findr track <path> --action <type>` inserts a row (<10ms). Raycast actions (Open File, Show in Finder, Copy Path, Copy Filename) call `findr track` fire-and-forget. Quick Look has no callback API in Raycast — not tracked.
- [x] **Frequency-based ranking boost** — Pass 4 in `unified_search()`: after candidate selection, queries interaction counts for ~30 result paths via `WHERE path IN (...)` with `SUM(CASE WHEN ...)` for time-decay buckets (7d: 1.0, 30d: 0.5, 90d: 0.2, 365d: 0.1). Boost: `ln(weighted_count + 1) * 150.0`, capped at 500. Never crosses tier boundaries (smallest gap: 500 between fuzzy and semantic). Log scale gives frequency index: 1 interaction → 104, 5 → 269, 20 → 457.
- [x] **`findr index add-path <dir>`** — Additive single-directory indexing. Walks only the specified path using same `WalkBuilder` + exclude rules. Inserts into existing SQLite + Tantivy without `db.clear()`. Updates stored `scan_paths` so future syncs include the new directory. Acquires `sync.lock`.
- [x] **Auto-detect new custom scan paths** — Raycast `useEffect` compares current `customPaths` preference against `LocalStorage`. New paths detected → 10s debounce (lets user finish editing) → `findr index add-path <path>` fire-and-forget. First run stores current paths without triggering index.
- [x] **Interaction count in metadata panel** — `interactions` field added to `SearchResult` (Rust) and `SearchResult` (TypeScript). Shown in Raycast detail panel when > 0.
- [x] **Interaction pruning** — `DELETE FROM interactions WHERE timestamp < now - 365 days` runs on every search startup. Sub-millisecond for typical data volumes.

## v5 Roadmap

- [ ] **Desktop app** — Standalone macOS app (separate repo: `findr-desktop`). Tauri v2 + React + Vite shell, calls findr CLI via `--json`. Global hotkey, persistent window, search history. Same binary, no coupling to Raycast extension. Persistent process → HNSW stays in memory (eliminates per-search disk load).
- [ ] Memory-mapped HNSW index (eliminate per-search disk load cost for CLI mode)
- [ ] CLI flags for HNSW control (`--rebuild-hnsw`, `--no-hnsw`)
- [ ] HNSW dimension/model metadata validation on load
- [ ] Homebrew formula
