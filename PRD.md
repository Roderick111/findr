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
- No OCR for scanned/image-only PDFs (planned for v2 via Apple Vision)
- No cloud/network drive indexing
- No semantic/AI search (planned for v2)

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
└──────────────────────────────────────────┘
```

## Unified Search

Single query searches both filenames and file contents. No `--content` flag needed. Results ranked by tiered scoring:

```
Tier 1 (10000): Filename starts with query     "Brainform.md" for "brainform"
Tier 2 (5000):  Filename contains query         "AI Readiness Report — Brainform.pdf"
Tier 3 (3000):  Filename typo match (Levenshtein) "Brainform.md" for "brainfrm"
Tier 4 (2000):  Content match (Tantivy BM25)    RIB.pdf containing "Revolut"
Tier 5 (1000):  Filename fuzzy (Nucleo)          subsequence matches

Within each tier:
  + file_type_bonus (PDFs +200, text +150, images +120, dev files +10)
  + recency_bonus (recent files score higher)
  + position_bonus (content matches near start of document score higher)
  + both_match_boost (+500 when file matches by both name and content)
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
  Tier 1 — Full rebuild (~25s)
    └── findr index init (first run) or findr index rebuild
    └── Nukes SQLite + Tantivy, re-walks + re-extracts everything
```

**Performance (measured on 9K files):**

| Scenario | Time |
|----------|------|
| No changes (steady state, FSEvents) | 40ms |
| New file detected (FSEvents) | 223ms |
| Deleted file detected (FSEvents) | 349ms |
| Incremental diff, no changes | 4.3s |
| Full rebuild | 25s |

## Content Extraction

| File type | Extractor | Status |
|-----------|-----------|--------|
| PDF | pdf-extract (with panic catching + stderr suppression) | Done |
| Plain text (.txt, .md, .csv, .log, .json, .yml) | Direct read (capped 100KB) | Done |
| Source code (.rs, .ts, .js, .py, .go, etc.) | Direct read | Done |
| Word (.docx) | ZIP + XML parse (word/document.xml) | Done |
| Excel (.xlsx) | ZIP + XML parse (xl/sharedStrings.xml) | Done |
| Images (.png, .jpg, .heic) | Apple Vision OCR | Planned (v2) |

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
| Error logging | Custom (~/.findr/error.log) | PDF panics, extraction failures, FDA issues |
| Raycast extension | TypeScript + React | useExec for CLI calls, bundled binary |
| CI/CD | GitHub Actions | Auto-build universal binary on tag push |

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

## v2 Roadmap

- [ ] OCR via Apple Vision framework (images + scanned PDFs)
- [ ] Semantic search via embeddings (fastembed + sqlite-vec)
- [ ] Search history / frecency tracking
- [ ] `findr config` for customizable scan paths and exclusions
- [ ] Homebrew formula
- [ ] Symlink and alias resolution
