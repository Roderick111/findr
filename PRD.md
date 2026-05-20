# Findr — Product Requirements Document

> The fastest local file search for macOS. Finds what Finder can't.

## Problem

macOS Finder/Spotlight fails to find files that exist on disk. PDF bank statements, downloaded contracts, screenshots, project assets — Spotlight's index silently misses files, corrupts, or excludes content. Every existing alternative either depends on the same broken Spotlight index (Raycast, Alfred, HoudahSpot) or does brute-force filesystem crawls that are painfully slow (EasyFind, Find Any File).

**Core user pain:** "I know this file exists on my laptop. I roughly know its name — or something inside it. I cannot find it."

## Solution

A Rust CLI (`findr`) that maintains its own filesystem index and delivers instant search results. Accessed primarily through a Raycast extension (bundled binary, zero setup) for zero-friction UX.

## Non-Goals (v1)

- Not a Finder replacement (no file management, no folder browsing)
- Not a Spotlight replacement (not hooking into system-wide search)
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
│  │   filename  │  │      content index  │ │
│  └────────────┘  └─────────────────────┘ │
│  ┌────────────┐  ┌─────────────────────┐ │
│  │ Levenshtein │  │   pdf-extract       │ │
│  │ typo match  │  │   + docx/xlsx (zip) │ │
│  └────────────┘  └─────────────────────┘ │
│  ┌──────────────────────────────────────┐ │
│  │   SQLite (file metadata + index meta) │ │
│  └──────────────────────────────────────┘ │
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
```

## CLI Interface

```bash
# Search (unified — filenames + content in one query)
findr search "revolut"              # finds RIB.pdf via content match
findr search "resume pdf"           # inline type filter
findr search "brainfrm"             # typo tolerance finds "Brainform"
findr search "revolut" --json       # JSON output for Raycast
findr search "revolut" --limit 30   # default: 30 results

# Indexing
findr index init                    # first-time full index
findr index status                  # stats, health, last sync times
findr index rebuild                 # full nuke + rebuild (manual)
findr index sync                    # incremental diff (usually automatic)
findr index ocr                     # run OCR on pending images (usually background)

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
- Right: content snippet, full path, file type, size, modified date
- Quick Look preview via Cmd+Y
- Actions: Open, Show in Finder, Copy Path, Copy Filename, Report Bug

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
| Fuzzy matching | Nucleo | 6x faster than skim, better ranking than fzf |
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
| Parallel extraction | rayon | Multi-core text extraction in Phase 2 |
| Error logging | Custom (~/.findr/error.log) | PDF panics, extraction failures, FDA issues |
| Raycast extension | TypeScript + React | useExec for CLI calls, bundled binary |
| CI/CD | GitHub Actions | Auto-build universal binaries (Rust + Swift) on tag push |

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

**Search:** Tier 5 at score 1500 (between BM25 content at 2000 and fuzzy at 1000). Cosine similarity threshold 0.15. BOTH_MATCH_BOOST (+500) when file also found by keyword/filename. Query embedded via single API call (~100ms).

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

## v2 Roadmap

- [ ] Search history / frecency tracking
- [ ] `findr config` for customizable scan paths and exclusions
- [ ] Homebrew formula
- [ ] Symlink and alias resolution
- [ ] Apple Photos library integration (Photos.framework)
