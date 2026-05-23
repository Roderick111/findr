# Code Review Objectives

Different review lenses for catching different classes of issues. Each should be run as a separate focused review — mixing objectives dilutes depth.

Each reviewer should: run the listed tools first, report exact output, then do manual review. Tag every finding as TOOL or MANUAL to track which tools add value.

## 1. Security (OWASP)

Focus: injection, path traversal, supply chain, information disclosure.

```
Tools (run first, report exact output):
  cargo audit              # CVE check against RustSec advisory DB — instant
  cargo clippy -- -D warnings  # catches some unsafe patterns

Check for:
- Command injection (process spawning with user input)
- Path traversal (user-controlled paths escaping intended directories)
- Unsafe deserialization (JSON parsing from subprocess output)
- Denial of service (unbounded loops, memory exhaustion, resource leaks)
- Information disclosure (error messages leaking sensitive paths)
- Unsafe code blocks (raw pointers, undefined behavior)
- Supply chain risks (dependencies with known vulnerabilities)
- File permission issues (world-readable/writable files)
- API key leakage in error messages
```

### Tool evaluation

`cargo audit` — **KEEP**. Caught `lru` unsoundness (RUSTSEC-2026-0002) and unmaintained deps that manual review missed. Instant, zero config, real signal.

`cargo geiger` — **SKIP**. Maps unsafe across dep tree but output is noisy (315 "warnings" for normal crypto/FFI/SIMD crates). Not actionable without deep dep knowledge. Slow to install.

`cargo deny` — **SKIP**. Needs a `deny.toml` config file. Without one it rejects all licenses. Setup cost not worth it for agent use. `cargo audit` covers the critical advisory check.

## 2. Performance

Focus: hot paths, memory, I/O efficiency, query patterns.

```
Tools (run first, report exact output):
  cargo bloat --release --crates -n 20  # binary size by crate — shows dep bloat
  ls -lh target/release/findr           # total binary size
  cargo clippy -- -W clippy::large_enum_variant -W clippy::box_collection \
    -W clippy::redundant_allocation -W clippy::vec_init_then_push \
    -W clippy::unnecessary_to_owned     # allocation anti-patterns

Check for:
- Hot path inefficiency (code that runs on every search or every file)
- Unnecessary heap allocations (Vec, String, HashMap in tight loops)
- O(n²) or worse algorithms hidden in loops
- Blocking I/O in performance-critical paths
- Memory pressure (peak memory during parallel extraction)
- Query efficiency (Tantivy, SQLite — missing indexes, N+1 queries)
- String processing overhead (unnecessary clones, UTF-8 conversions)
- Disk I/O patterns (sequential vs random reads)
- format!() allocations inside loops with static data
```

### Tool evaluation

`cargo bloat` — **KEEP**. Revealed rustls (400KB), bitpacking (334KB), reverse_geocoder bloat that manual review wouldn't quantify. Useful for tracking binary size over releases.

`clippy allocation lints` — **MARGINAL**. Found zero issues on a well-written codebase. The extra `-W` flags add no value beyond default clippy. Keep default clippy, skip extra flags.

`cargo flamegraph` / `criterion` / `dhat` — **SKIP for agents**. Runtime profiling tools need actual workloads and are better suited for developer use, not automated review agents.

## 3. Error Handling

Focus: panics, swallowed errors, recovery, user-facing messages.

```
Check for:
- Panics in production code (unwrap, expect, unchecked indexing)
- Swallowed errors (let _ = on fallible operations, empty catch blocks)
- Missing error context (anyhow errors without .context())
- Unrecoverable states (corrupted index with no detection/repair)
- Error propagation gaps (Result converted to Option losing info)
- User-facing error messages (are they actionable?)
- Graceful degradation (partial functionality when component fails)
- Crash recovery (state left after panic/kill)
- process::exit() bypassing cleanup (especially in JSON/Raycast mode)
```

No tools — pure manual review. LLM excels here.

## 4. Concurrency

Focus: races, deadlocks, lock contention, shared state.

```
Check for:
- Data races (shared mutable state without synchronization)
- Deadlocks (lock ordering, nested locks)
- Lock contention (write locks, busy timeouts)
- TOCTOU races (check-then-act on locks/files)
- Process coordination gaps (concurrent processes corrupting shared state)
- Singleton safety (OnceLock initialization races)
- Panic propagation in parallel workers (rayon)
- Channel safety (deadlock, leak)
- File locking correctness (flock implementation)
- Global state mutation (panic hooks, env vars)
```

No tools — pure manual review. LLM caught the critical embed lock TOCTOU that no tool would find.

## 5. Architecture

Focus: scalability, coupling, atomicity, data integrity.

```
Check for:
- Dual-store consistency (can two data stores drift out of sync?)
- Atomicity gaps (partial writes on crash)
- Scalability walls (O(n) operations that should be O(log n))
- Tight coupling between components
- Missing reconciliation mechanisms
- Background process coordination
- Schema migration safety
```

No tools — pure manual review.

## 6. API / CLI Ergonomics

Focus: user experience, discoverability, output format.

```
Check for:
- Confusing or missing error messages
- Surprising default behavior
- Missing help text or examples
- Exit code consistency
- Output format issues (JSON correctness, human-readable formatting)
- Progress feedback for long operations
- First-run experience
- Edge cases (empty input, special characters, unicode)
- Discoverability of features
- clap value_parser / ValueEnum for constrained inputs
```

No tools — pure manual review.

## 7. Testability

Focus: untestable code, missing coverage, coupling.

```
Check for:
- Untestable code paths (side effects mixed with logic)
- Missing edge case coverage
- Coupling that prevents mocking/isolation
- Integration test gaps
- Flaky test patterns
- Test data management
- Time-dependent logic without injectable clock
```

No tools — pure manual review.

## 8. Dead Code & Dependencies

Focus: unused code, stale imports, dependency bloat.

```
Tools (run first, report exact output):
  cargo machete            # fast unused dep detection — no nightly needed, ~1s
  cargo clippy -- -W dead_code -W unused_imports -W unused_variables
  cargo tree --duplicates  # duplicate crate versions in dep tree
  cd raycast-extension && npx ray lint  # TypeScript lint

Check for:
- Unused functions, constants, structs, enum variants
- Stale imports (use statements for removed items)
- Unreachable code branches
- Dependencies in Cargo.toml/package.json not used in source
- Heavy dependencies for light use (pulling in a large crate for one function)
- Duplicate functionality (two deps doing the same thing)
- Outdated/unmaintained dependencies
- Feature flags enabling unused functionality
- Platform-specific deps compiled unconditionally
```

### Tool evaluation

`cargo machete` — **KEEP**. Instant, zero false positives on our codebase. Catches unused deps that clippy can't (clippy only sees code, not Cargo.toml). Limitation: can't reason about "dep is too heavy for its use" or "dep only needed on one platform."

`cargo tree --duplicates` — **KEEP**. Showed fs4 v0.8/v0.12 duplicate. Informational but helps track dep hygiene.

`cargo clippy dead_code` — **Already running** via default clippy. Extra `-W` flags add no value.

`cargo udeps` — **SKIP**. More accurate than machete but needs nightly and is slow. Machete is good enough.

## 9. API Design

Focus: public interfaces, abstraction quality, consistency.

```
Check for:
- Confusing function signatures (too many params, unclear return types)
- Leaky abstractions (internal details exposed in public API)
- Inconsistent naming conventions across modules
- Missing or misleading type annotations
- God structs (one struct doing too many things)
- Unclear ownership semantics (who owns what data)
- Breaking changes in serialization formats (JSON output, DB schema)
- Missing builder patterns where construction is complex
- Tuple type aliases masking field semantics
```

No tools — pure manual review.

## Usage

Run one reviewer per objective in parallel (all 9 simultaneously) for maximum coverage with minimal overlap:

```
Agent(subagent_type="code-reviewer", prompt="[Objective] review of [project]. Focus ONLY on [objective]. Read all source files. Report findings classified as CRITICAL/HIGH/MEDIUM/LOW with file:line references.")
```

For tool-augmented reviewers (1, 2, 8), add to the prompt:

```
"Run the listed tools FIRST, report their EXACT output, then do manual review. Tag every finding as TOOL-caught or MANUAL-only."
```

## Tool Summary

| Tool | Install | Speed | Value | Keep? |
|------|---------|-------|-------|-------|
| cargo audit | `cargo install cargo-audit` | instant | Catches real CVEs in deps | YES |
| cargo machete | `cargo install cargo-machete` | ~1s | Catches unused Cargo deps | YES |
| cargo bloat | `cargo install cargo-bloat` | ~10s | Binary size breakdown by crate | YES |
| cargo tree --duplicates | built-in | instant | Shows duplicate dep versions | YES |
| cargo clippy | built-in | ~3s | Standard lint (already in CI) | YES (already used) |
| ray lint | built-in | ~2s | ESLint + Prettier for Raycast | YES (already used) |
| cargo geiger | `cargo install cargo-geiger` | slow install, ~30s run | Maps unsafe in dep tree — noisy | NO |
| cargo deny | `cargo install cargo-deny` | instant | Needs config file, setup cost | NO |
| cargo udeps | needs nightly | slow (full compile) | More accurate than machete | NO |

**Key insight from testing**: Every actionable finding came from manual LLM review. Tools confirmed hygiene (clean deps, no CVEs, no lint warnings) but missed all architectural, semantic, and logic issues. Tools provide a baseline; LLM provides the analysis.
