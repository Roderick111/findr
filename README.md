# findr

Fast local file search for macOS. Finds what Spotlight can't.

Searches both filenames (fuzzy matching via Nucleo) and file contents (full-text via Tantivy, including PDFs). Single query, unified results, tiered ranking. 9.1MB binary, searches 9K files in <200ms.

## The Problem

Spotlight doesn't search inside PDFs reliably. It misses files. It's slow. You know a file exists but Spotlight disagrees.

findr indexes your filesystem and builds a full-text content index. One query searches filenames AND file contents simultaneously, with intelligent ranking.

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

Set the findr binary path in Raycast extension preferences.

## Usage

```bash
# Search (filenames + content)
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
```

## How Auto-Indexing Works

Two-layer system keeps the index fresh without manual rebuilds.

**Layer 1 -- Quick diff (every search):** Before each search, findr does a shallow scan to detect new or modified files since the last index. New files get indexed immediately. Fast enough to run inline.

**Layer 2 -- Full rebuild (weekly):** If the index is older than 7 days, findr spawns a background thread to do a full filesystem scan with pruned directory walking. Runs silently while your search results return immediately.

## Tech Stack

- **Rust** -- CLI and core search engine
- **Nucleo** -- Fuzzy filename matching (same engine as Helix editor)
- **Tantivy** -- Full-text content search index
- **pdf-extract** -- PDF text extraction with panic catching for malformed files
- **SQLite** (rusqlite) -- File metadata storage
- **ignore** -- Respects .gitignore-style patterns during directory walking
- **Clap** -- CLI argument parsing
- **Raycast API** -- Extension UI (TypeScript/React)

## Ranking

Results use tiered scoring:

1. **Filename prefix match** -- query matches start of filename
2. **Filename contains** -- query found within filename
3. **Content match** -- full-text hit inside file (position-weighted: matches near document start rank higher)
4. **Fuzzy-only** -- fuzzy filename match with no exact substring

Within each tier, ties break by file recency.

## License

MIT
