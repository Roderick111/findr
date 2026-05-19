# Findr — Product Requirements Document

> Fast, reliable local file search for macOS. Finds what Spotlight can't.

## Problem

macOS Finder/Spotlight fails to find files that exist on disk. PDF bank statements, downloaded contracts, screenshots, project assets — Spotlight's index silently misses files, corrupts, or excludes content. Every existing alternative either depends on the same broken Spotlight index (Raycast, Alfred, HoudahSpot) or does brute-force filesystem crawls that are painfully slow (EasyFind, Find Any File).

**Core user pain:** "I know this file exists on my laptop. I roughly know its name — or something inside it. I cannot find it."

This isn't infrequent — finding files (screenshots, logos, docs, configs, statements) happens multiple times per day across different contexts.

## Solution

A Rust CLI (`findr`) that maintains its own filesystem index and delivers instant fuzzy search results. Accessed primarily through a Raycast extension for zero-friction UX.

## Non-Goals (v1)

- Not a Finder replacement (no file management, no folder browsing)
- Not a Spotlight replacement (not hooking into system-wide search)
- Not a product for distribution (personal tool, no onboarding, no App Store)
- No OCR for scanned/image-only PDFs
- No cloud/network drive indexing
- No semantic/AI search (planned for v2)

## Architecture

```
User types in Raycast
        │
        ▼
┌─────────────────────────┐
│  Raycast Extension (TS) │  Thin glue — calls CLI, renders results
│  ~150 lines per command  │  Uses useExec hook to shell out
└────────┬────────────────┘
         │  findr search "revolut" --content --json
         ▼
┌─────────────────────────────────────┐
│        findr CLI (Rust binary)       │
│                                      │
│  ┌──────────┐  ┌──────────────────┐ │
│  │  Nucleo   │  │    Tantivy       │ │
│  │  fuzzy    │  │    full-text     │ │
│  │  filename │  │    content index │ │
│  └──────────┘  └──────────────────┘ │
│  ┌──────────────────────────────────┐│
│  │     pdf-extract (PDF text)       ││
│  └──────────────────────────────────┘│
│  ┌──────────────────────────────────┐│
│  │  SQLite (file metadata cache)    ││
│  └──────────────────────────────────┘│
│  ┌──────────────────────────────────┐│
│  │  Two-layer auto-indexing engine  ││
│  └──────────────────────────────────┘│
└─────────────────────────────────────┘
```

**Two search modes:**
1. **Filename search (default):** Nucleo fuzzy matches against SQLite path index. ~4-200ms.
2. **Content search (`--content`):** Tantivy full-text search across extracted file contents. ~6-50ms.

## CLI Interface

```bash
# Search
findr search "revolut"                    # fuzzy filename search
findr search "revolut" --content          # search inside file contents
findr search "revolut" --type pdf         # filter by file type
findr search "resume pdf"                 # inline type filter (same as --type)
findr search "revolut" --content --json   # JSON output for Raycast

# Output
findr search "revolut" --json             # structured output for Raycast
findr search "revolut" --limit 20         # cap results (default: 20)

# Indexing
findr index init                          # first-time full index (paths + content)
findr index status                        # stats, health, last index times
findr index rebuild                       # nuke and rebuild everything
```

**JSON output format (for Raycast):**
```json
{
  "query": "revolut",
  "mode": "content",
  "elapsed_ms": 36,
  "total_results": 10,
  "results": [
    {
      "path": "/Users/daniel/Downloads/Telegram Desktop/RIB.pdf",
      "filename": "RIB.pdf",
      "score": 12.76,
      "match_type": "content",
      "size_bytes": 45000,
      "modified": "2026-03-15T10:30:00+00:00",
      "file_type": "pdf",
      "content_snippet": "Revolut France, succursale de Revolut Bank UAB"
    }
  ]
}
```

## Indexing Strategy

### What gets indexed

| Layer | What | Storage | Speed |
|-------|------|---------|-------|
| **File paths** | Full path + filename for every file in scan paths | SQLite | ~5s for 9K files |
| **Metadata** | size, modified date, file type | SQLite | Extracted during walk |
| **Content** | Extracted text from supported file types | Tantivy | ~20s for 6K files |

### Two-Layer Auto-Indexing (implemented)

No manual reindexing needed for daily use. Every search triggers automatic index maintenance:

```
findr search "anything"
    │
    ├── Layer 1 (synchronous, ~130-650ms):
    │   ├── Pass 1: Shallow scan (depth 3) of hot folders
    │   │   └── Catches: modified files in ~/Downloads, ~/Desktop, ~/Documents
    │   └── Pass 2: Deep scan (depth 20) with dir-mtime pruning
    │       └── Catches: new files anywhere in the tree
    │       └── Skips directories whose mtime hasn't changed (99% of dirs)
    │
    ├── Search: Query the now-updated index → return results
    │
    └── Layer 2 (background thread, only if index >7 days old):
        └── Full recursive reindex of all scan paths
        └── Catches: deletions, renames, moved files
```

**Performance measured on real filesystem (9K files):**

| Scenario | Time |
|----------|------|
| No changes (steady state) | 133ms |
| New file in Downloads | 280ms |
| New file deep in project tree | 188ms |
| Modified existing file | 157ms |
| Full reindex (background) | 25s |

### Supported content extraction (v1)

| File type | Extractor | Status |
|-----------|-----------|--------|
| PDF | pdf-extract (with panic catching) | Done |
| Plain text (.txt, .md, .csv, .log, .json, .yml) | Direct read (capped 100KB) | Done |
| Source code (.rs, .ts, .js, .py, .go, etc.) | Direct read | Done |
| Word (.docx) | TBD — zip + XML parse | v2 |
| Excel (.xlsx) | TBD | v2 |

### Exclusions (default)

```
node_modules/    .git/           .DS_Store        Library/Caches/
Library/Application Support/     .Trash/          .cargo/
.rustup/         target/         .npm/            .bun/
.venv/           .mypy_cache/    __pycache__/     .pytest_cache/
.ruff_cache/     .cache/         .gradle/         .idea/
.vscode/         .next/          .nuxt/           dist/
build/           .turbo/         .parcel-cache/   Pods/
```

Max file size for content extraction: 100 MB.

### Default scan paths

```
~/Documents  ~/Desktop  ~/Downloads  ~/Projects  ~/Pictures
```

## Search & Ranking

### Filename search (default mode)

Nucleo fuzzy matcher against indexed filenames. Ranking:

```
score = nucleo_match_score * 2.0 (filename match bonus)
      + recency_bonus (100 / (1 + sqrt(age_in_days)))
```

Minimum score threshold filters garbage subsequence matches (e.g., "revolut" won't match "evaluation").

### Content search (--content flag)

Tantivy BM25 scoring across extracted text. Returns content snippets showing where the match occurred.

### Inline type filters

Query parser recognizes type tokens automatically:
- `"resume pdf"` → fuzzy match "resume", filter to .pdf files
- `"screenshot png"` → fuzzy match "screenshot", filter to .png
- `"revolut .pdf"` → same as above

No special syntax needed. If last token matches a known file extension, treat as filter.

## Raycast Extension

Three commands:

### Search Files (filename search)
- Single text input with debounced queries
- Results as `List` with file type icons, path, size, relative date
- Actions: Open (Enter), Show in Finder (Cmd+Enter), Copy Path (Cmd+C), Copy Filename (Cmd+Shift+C)

### Search File Contents (content search)
- Same input, calls CLI with `--content` flag
- Detail panel showing content match snippet + file metadata
- Same actions as filename search

### Rebuild Index
- No-view command, shows toast with progress
- Calls `findr index rebuild`

### Preferences
| Setting | Default | Description |
|---------|---------|-------------|
| Findr Binary Path | ~/.local/bin/findr | Path to the findr binary |
| Max Results | 20 | Results cap per query |

## Performance (measured)

| Metric | Target | Actual |
|--------|--------|--------|
| Filename search latency | < 200ms | 4-25ms |
| Content search latency | < 200ms | 6-57ms |
| Initial full index (9K files) | < 60s | 25s |
| Binary size | < 15MB | 9.1MB |
| Quick diff (no changes) | < 1s | 133ms |
| Quick diff (new file, any depth) | < 1s | 157-280ms |

## Tech Stack

| Component | Tool | Why |
|-----------|------|-----|
| CLI | Rust (clap) | Performance, access to all search crates |
| Fuzzy matching | Nucleo | 6x faster than skim, better ranking than fzf |
| Full-text index | Tantivy 0.22 | Embedded, BM25 scoring, fuzzy support |
| Metadata store | SQLite (rusqlite) | Reliable, zero-config, fast path lookups |
| PDF extraction | pdf-extract | Rust-native, handles most PDFs (with panic catching) |
| Filesystem walking | ignore crate | Fast, respects gitignore patterns |
| Raycast extension | TypeScript + React | Raycast's required stack, useExec for CLI calls |
| CLI-Raycast bridge | JSON over stdout | Simple, debuggable, no IPC complexity |

## What's Implemented (v1)

- [x] Rust CLI with clap argument parsing
- [x] Filesystem walker with smart exclusions
- [x] SQLite metadata index
- [x] Nucleo fuzzy filename search with score thresholds
- [x] Tantivy full-text content search
- [x] PDF text extraction (with panic catching for malformed PDFs)
- [x] Inline type filters ("resume pdf")
- [x] Recency-boosted ranking
- [x] JSON output for Raycast integration
- [x] Two-layer auto-indexing (shallow modifications + deep new files + weekly full rebuild)
- [x] Raycast extension: Search Files command
- [x] Raycast extension: Search File Contents command with detail panel
- [x] Raycast extension: Rebuild Index command

## v2 Roadmap

- [ ] Semantic search via embeddings (fastembed + sqlite-vec) for meaning-based content matching
- [ ] Word/Excel content extraction (.docx, .xlsx)
- [ ] Search history / frecency tracking
- [ ] FSEvents daemon for real-time file watching
- [ ] `findr config` for customizable scan paths and exclusions
- [ ] Full Disk Access detection and guidance
- [ ] Symlink and alias resolution
