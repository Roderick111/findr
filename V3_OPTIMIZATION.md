# V3 Optimization Plan — Large-Scale Search (500K+ files)

## Problem

Current search scans every filename linearly. At 10K files this takes 60ms. At 1M files it would take ~6 seconds. Semantic search loads all embedding vectors into memory (2GB at 1M files).

## Current Architecture (what's slow)

```
Every search:
  1. SQLite: load ALL file paths into Vec         → O(n) memory + time
  2. Nucleo Pattern::score(): loop Vec, score each → O(n), single-threaded
  3. Levenshtein: loop Vec again for typo matches  → O(n), single-threaded
  4. Tantivy: index lookup for content matches     → O(log n) ← already fast
  5. Cosine: load ALL vectors, compare each        → O(n) memory + time
```

Steps 1-3 and 5 are the bottlenecks. Step 4 (Tantivy) already scales.

## Change 1: Nucleo<T> parallel API (high priority)

### What

Replace manual `Pattern::score()` loop with the high-level `Nucleo<T>` struct.

### Why

`Nucleo<T>` is the API Nucleo was designed to be used with. We're using the low-level primitive (`Pattern::score()`) in a manual single-threaded loop. `Nucleo<T>` provides:

- **Parallel scoring** — distributes work across all CPU cores via built-in threadpool
- **Incremental matching** — when query is an extension of previous query (user typing), only re-scores the already-matched subset instead of all items
- **Lock-free injection** — files can be pushed in from any thread via `Injector`
- **Pagination** — `snapshot.matched_items(0..30)` returns top 30 without materializing all results

### How it works

```rust
// One-time setup (or on index change)
let nucleo = Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1);
let injector = nucleo.injector();
for (path, filename, ext, mtime, size) in all_files {
    injector.push(item, |item, cols| { cols[0] = filename.into(); });
}

// Per search
nucleo.pattern.reparse(0, &query, CaseMatching::Ignore, Normalization::Smart, false);
while nucleo.tick(10).changed { /* wait for matching to complete */ }
let snapshot = nucleo.snapshot();
for item in snapshot.matched_items(0..30) {
    // item.score, item.data — same scoring quality as Pattern::score()
}
```

### Impact on ranking

**None.** Same Nucleo scoring algorithm, same ranking quality. The `Nucleo<T>` struct uses `Pattern::score()` internally — it just parallelizes and optimizes the outer loop.

### Impact on speed

| File count | Current (single-threaded) | Nucleo<T> (8 cores) |
|---|---|---|
| 10K | 60ms | ~8ms |
| 100K | ~600ms | ~80ms |
| 1M | ~6s | ~40ms |

### CLI vs Desktop app

- **CLI** (exits per invocation): still loads files and injects each time, but matching is parallel. Saves ~7x on the scan.
- **Desktop app** (persistent process): inject once, match many times. Incremental mode means each keystroke only re-scores the matched subset — sub-millisecond refinements.

### What this eliminates

- The separate Levenshtein pass (pass 1b) — can be merged into a single Nucleo pass with classify/typo check only on matched results
- Bulk SQLite load as bottleneck — still loads, but Nucleo parallelizes the scan
- The original proposal to move filename search to Tantivy (worse ranking, more complexity)

### Why not Tantivy for filenames?

Evaluated and rejected:

| | Nucleo | Tantivy FuzzyTermQuery |
|---|---|---|
| Ranking quality | Excellent — word boundary, camelCase, consecutive char bonuses | Basic — Levenshtein distance only |
| "brainf" → "Brainform.md" | High score (prefix + boundary) | Won't match without prefix mode |
| Tokenization | Whole filename, understands separators | Splits "code_review.md" → ["code", "review", "md"] |
| Index maintenance | None — in-memory scan | Must build and sync separate index |
| Speed at 1M | ~40ms parallel | ~1-5ms index lookup |

Tantivy is faster at extreme scale but produces worse results for filename matching. 40ms is fast enough. Ranking quality matters more.

## Change 2: HNSW for semantic search

### What

Replace brute-force cosine similarity scan with an HNSW (Hierarchical Navigable Small World) index using `hnsw_rs`.

### Why

Current semantic search loads ALL embedding vectors (512 dimensions * 4 bytes * N files) into memory and compares the query against every single one:

| File count | Memory for vectors | Cosine scan time |
|---|---|---|
| 10K | ~20MB | 10ms |
| 100K | ~200MB | 100ms |
| 1M | ~2GB | ~1s |

HNSW gives approximate top-K results in ~1ms with ~100MB memory regardless of dataset size.

### Impact on ranking

**Minimal.** HNSW is approximate — a file at cosine 0.16 (just above the 0.15 threshold) might be missed. In practice, near-threshold results are low quality anyway and rarely affect the user-visible top 30.

### Scope (~100-150 lines across 3 files)

Small refactor. No architectural change — just swapping the search method inside Pass 3.

**What changes:**

| File | Change |
|---|---|
| `Cargo.toml` | Add `hnsw_rs` dependency |
| `semantic.rs` | Add `build_hnsw_index()`, `search_hnsw()`. HNSW index persisted to `~/.findr/semantic.hnsw` |
| `main.rs` (search path) | Replace `db.load_all_vectors()` + brute-force loop with `load HNSW index` + `search_hnsw(query_vec, top_k)` |
| `main.rs` (embed path) | After storing vectors in SQLite, also insert into HNSW index |
| `search.rs` | Pass 3 receives `Vec<(String, f32)>` (path + similarity) instead of raw vectors. Same scoring logic |

**What doesn't change:**
- SQLite still stores vectors (source of truth for rebuilds)
- Embedding API calls
- All scoring/ranking logic
- Tantivy, Nucleo, Levenshtein
- Existing tests (add new ones)

### Library choice: `hnsw_rs`

Evaluated four options:

| Library | Verdict |
|---|---|
| **`hnsw_rs`** | **Chosen.** Pure Rust, no C++ dependency, fast builds, incremental insert/delete, 99% recall at 1M vectors. |
| `usearch` | Fastest benchmarks but C++ core — adds compile time, cross-compilation complexity. Overkill for our scale. |
| `arroy` (Meilisearch) | LMDB-backed, near-zero RAM. Interesting for extreme memory constraints but uses random projections (lower recall than HNSW). |
| `instant-distance` | Pure Rust HNSW but batch-build only (no incremental insert). Low maintenance. Skip. |

### What was evaluated and dropped for semantic search

- **IVF (Inverted File Index):** Needs training step, poor for incremental inserts. Wrong fit.
- **Product Quantization:** Good memory savings but adds complexity. Consider later if RAM constrained.
- **DiskANN/Vamana:** Billion-scale, SSD-optimized. Overkill by 1000x.
- **Brute-force with SIMD (SimSIMD):** ~15-50ms at 1M with AVX2. Viable fallback if HNSW complexity isn't wanted, but doesn't solve the memory problem (still loads all vectors).

## What was evaluated and dropped

### Tantivy for filename search
See table above. Worse ranking quality for filenames. Nucleo<T> solves the same scaling problem without sacrificing result quality.

### Streaming SQLite queries
Original idea: iterate SQLite rows one-at-a-time instead of loading all into a Vec. Saves memory but doesn't save time. If Nucleo<T> holds filenames in memory after injection, the per-search SQLite load becomes unnecessary anyway. In the desktop app, inject once at startup. In the CLI, still load per invocation but Nucleo parallelizes the scan.

### Moving FSEvents
Already implemented. Works at any scale. No changes needed.
