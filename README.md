# findr

The fastest local file search for macOS. Finds what Finder can't.

Searches filenames (fuzzy matching via Nucleo), file contents (full-text via Tantivy, including PDFs), and meaning (semantic embedding via OpenRouter). Single query, unified results, tiered ranking.

## The Problem

Spotlight doesn't search inside PDFs reliably. It misses files. It's slow. You know a file exists but Spotlight disagrees. And when you search "venture capital fundraising", Spotlight can't find your `business-plan.md` because those exact words don't appear in the file.

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

### CLI

```bash
cargo install --path .
findr index init
```

By default, indexes `~/Documents`, `~/Desktop`, `~/Downloads`, and `~/Projects`. To specify paths:

```bash
findr index init --paths ~/Documents,~/Code,~/Notes
```

### Raycast Extension

```bash
cd raycast-extension
npm install
npm run dev
```

### Semantic Search (optional)

Semantic search finds files by meaning, not just keywords. Requires an OpenRouter API key (~$0.06 to embed 5K files).

```bash
# Set your API key (get one at https://openrouter.ai)
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

# JSON output (for scripts/Raycast)
findr search "invoice" --json --limit 10

# Index status
findr index status

# Rebuild index
findr index rebuild

# Embed files for semantic search
findr index embed
findr index embed --status
```

## How Auto-Indexing Works

Three-layer system keeps the index fresh without manual rebuilds.

**Layer 1 -- FSEvents (every search, ~40ms):** Replays the macOS kernel change journal to catch new, modified, and deleted files since the last search. Near-instant, no filesystem walking.

**Layer 2 -- Incremental diff (every 24 hours):** Walks the filesystem, compares against SQLite by path+mtime. Only processes changes. Safety net for anything FSEvents missed.

**Layer 3 -- Full rebuild (manual):** `findr index init` (first run) or `findr index rebuild`. Nukes everything, re-walks, re-extracts. OCR and semantic embedding run as background processes after text indexing completes.

## Tech Stack

- **Rust** -- CLI and core search engine
- **Nucleo** -- Fuzzy filename matching (same engine as Helix editor)
- **Tantivy** -- Full-text content search index with BM25 scoring
- **Levenshtein** -- Typo tolerance for filenames
- **pdf-extract** -- PDF text extraction with panic catching + quality validation
- **SQLite** (rusqlite, WAL mode) -- File metadata + semantic vector storage
- **findr-ocr** (Swift) -- Apple Vision OCR + EXIF extraction
- **reverse_geocoder** -- Offline GPS → city/country resolution
- **ureq** -- OpenRouter API client for semantic embeddings
- **rayon** -- Parallel content extraction + OCR processing
- **Raycast API** -- Extension UI (TypeScript/React)

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
