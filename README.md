# findr

The fastest local file search. Finds what your OS search can't.

Cross-platform (macOS, Linux, Windows). Searches filenames (fuzzy matching via Nucleo), file contents (full-text via Tantivy, including PDFs), and meaning (semantic embedding via OpenRouter). Single query, unified results, tiered ranking.

## The Problem

Your OS search doesn't search inside PDFs reliably. It misses files. It's slow. You know a file exists but search disagrees. And when you search "venture capital fundraising", it can't find your `business-plan.md` because those exact words don't appear in the file.

findr indexes your filesystem, builds a full-text content index, and optionally embeds file content for semantic search. One query searches filenames AND contents AND meaning simultaneously.

## Demo

```
$ findr search "revolut"

Found 3 results in 48ms

  1. [pdf] RIB.pdf
     /Users/me/Documents/Banking/RIB.pdf
     >> ...account details for Revolut Bank UAB, SWIFT: REVOLT21...
  2. [pdf] bank-statements-2024.pdf
     /Users/me/Documents/Finance/bank-statements-2024.pdf
     >> ...transfer from Revolut to savings account on 2024-03-15...
  3. [csv] transactions.csv
     /Users/me/Documents/Finance/transactions.csv
     >> ...Revolut,EUR,1250.00,2024-03-15...
```

The file is called `RIB.pdf`. "Revolut" only appears inside the PDF content. Spotlight won't find it. findr will.

## Installation

### Quick Install (macOS / Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/Roderick111/findr/main/install.sh | bash
```

### Windows

```powershell
irm https://raw.githubusercontent.com/Roderick111/findr/main/install.ps1 | iex
```

Downloads a pre-built binary. No Rust toolchain needed. First search auto-builds the index.

### Raycast / Vicinae Extension

Install from the Raycast Store (search "findr"), or run locally:

```bash
cd raycast-extension
bun install && bun run dev
```

Works with [Vicinae](https://github.com/vicinaehq/vicinae) (Raycast alternative for Linux) — same extension format.

### Build from Source

```bash
cargo install --path .
findr index init
```

### Semantic Search (optional)

Semantic search finds files by meaning, not just keywords. Requires an OpenRouter API key (~$0.06 to embed 5K files).

```bash
# Set your API key (get one at https://openrouter.ai)
# macOS: ~/.findr/openrouter_key
# Linux: ~/.local/share/findr/openrouter_key
# Windows: %APPDATA%\findr\openrouter_key
echo "sk-or-..." > ~/.findr/openrouter_key
chmod 600 ~/.findr/openrouter_key

# Embed your files
findr index embed

# Check progress
findr index embed --status
```

Or set `OPENROUTER_API_KEY` as an environment variable. In Raycast, add the key in extension preferences.

## Usage

```bash
# Search (filenames + content + semantic)
findr search "quarterly report"

# Filter by file type (inline)
findr search "resume pdf"

# Filter by type (flag)
findr search "readme" -t md

# Scope to folders (searches inside matching folders)
findr search "dharma in:daily"
findr search "report pdf in:downloads"
findr search "in:obsidian"              # scoped recent files

# Folder search
findr search "projects /"               # trailing / = folders only

# Path filter (for scripts/agents)
findr search "revolut" --path ~/Documents --json
findr search "revolut" --snippet-length 500 --json

# JSON output (for scripts/Raycast)
findr search "invoice" --json --limit 10

# Index management
findr index status
findr index rebuild
findr index embed
findr index embed --status
```

## How Auto-Indexing Works

Three-layer system keeps the index fresh without manual rebuilds.

**Layer 1 -- Incremental sync (every search):** On macOS, replays the FSEvents kernel change journal (~40ms). On Linux/Windows, compares file mtimes against the database. Catches new, modified, and deleted files since the last search.

**Layer 2 -- Incremental diff (every 24 hours):** Walks the filesystem, compares against SQLite by path+mtime. Only processes changes. Safety net for anything Layer 1 missed.

**Layer 3 -- Full rebuild (manual):** `findr index init` (first run) or `findr index rebuild`. Nukes everything, re-walks, re-extracts. OCR and semantic embedding run as background processes after text indexing completes.

Indexing respects `.gitignore` files by default — dependency folders and build artifacts are excluded automatically.

## Tech Stack

- **Rust** -- CLI and core search engine (cross-platform via `src/platform/` abstraction)
- **Nucleo** -- Fuzzy filename matching (same engine as Helix editor)
- **Tantivy** -- Full-text content search index with BM25 scoring
- **Levenshtein** -- Typo tolerance for filenames
- **pdf-extract** -- PDF text extraction with panic catching + quality validation
- **SQLite** (rusqlite, WAL mode) -- File metadata + semantic vector + query cache storage
- **findr-ocr** (Swift, macOS) -- Apple Vision OCR + EXIF extraction
- **ocrs** (Rust, Linux/Windows) -- Pure-Rust OCR via ONNX models, zero C dependencies
- **reverse_geocoder** -- Offline GPS → city/country resolution
- **ureq** -- OpenRouter API client for semantic embeddings
- **rayon** -- Parallel content extraction + OCR processing
- **Raycast/Vicinae API** -- Extension UI (TypeScript/React)

## Platform Support

| Feature | macOS | Linux | Windows |
|---------|-------|-------|---------|
| Filename search | full | full | full |
| Content search (PDF/DOCX/XLSX) | full | full | full |
| Semantic search | full | full | full |
| OCR (image text extraction) | Apple Vision | ocrs (pure Rust) | ocrs (pure Rust) |
| Incremental sync | FSEvents (kernel) | mtime-diff | mtime-diff |
| File locking | flock | flock | fs4 (LockFileEx) |
| Data directory | ~/.findr | ~/.local/share/findr | %APPDATA%\findr |
| Launcher UI | Raycast | Vicinae | CLI only |

## Ranking

Results use tiered scoring:

1. **Filename prefix match (10000)** -- query matches start of filename
2. **Filename contains (5000)** -- query found within filename
3. **Filename typo match (3000)** -- Levenshtein distance ≤2
4. **Content match (2000)** -- BM25 full-text hit inside file (position + BM25 score weighted)
5. **Semantic match (1500)** -- cosine similarity via embedding (optional, needs API key)
6. **Filename fuzzy (1000)** -- Nucleo subsequence match

Within each tier: file type bonus (documents > media > dev files), recency bonus, and both-match boost (+500 when found by multiple methods).

## License

MIT
