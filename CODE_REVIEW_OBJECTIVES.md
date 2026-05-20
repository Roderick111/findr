# Code Review Objectives

Different review lenses for catching different classes of issues. Each should be run as a separate focused review — mixing objectives dilutes depth.

## 1. Security (OWASP)

Focus: injection, path traversal, supply chain, information disclosure.

```
Check for:
- Command injection (process spawning with user input)
- Path traversal (user-controlled paths escaping intended directories)
- Unsafe deserialization (JSON parsing from subprocess output)
- Denial of service (unbounded loops, memory exhaustion, resource leaks)
- Information disclosure (error messages leaking sensitive paths)
- Unsafe code blocks (raw pointers, undefined behavior)
- Supply chain risks (dependencies with known vulnerabilities)
- File permission issues (world-readable/writable files)
- Binary integrity (codesigning, tampering detection)
```

## 2. Performance

Focus: hot paths, memory, I/O efficiency, query patterns.

```
Check for:
- Hot path inefficiency (code that runs on every search or every file)
- Unnecessary heap allocations (Vec, String, HashMap in tight loops)
- O(n²) or worse algorithms hidden in loops
- Blocking I/O in performance-critical paths
- Memory pressure (peak memory during parallel extraction)
- Query efficiency (Tantivy, SQLite — missing indexes, N+1 queries)
- String processing overhead (unnecessary clones, UTF-8 conversions)
- Disk I/O patterns (sequential vs random reads)
```

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
```

## 4. Concurrency

Focus: races, deadlocks, lock contention, shared state.

```
Check for:
- Data races (shared mutable state without synchronization)
- Deadlocks (lock ordering, nested locks)
- Lock contention (write locks, busy timeouts)
- Process coordination gaps (concurrent processes corrupting shared state)
- Singleton safety (OnceLock initialization races)
- Panic propagation in parallel workers (rayon)
- Channel safety (deadlock, leak)
- File locking correctness (flock implementation)
```

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
```

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
```

## 8. Dead Code & Dependencies

Focus: unused code, stale imports, dependency bloat.

```
Check for:
- Unused functions, constants, structs, enum variants
- Stale imports (use statements for removed items)
- Unreachable code branches
- Dependencies in Cargo.toml/package.json not used in source
- Heavy dependencies for light use (pulling in a large crate for one function)
- Duplicate functionality (two deps doing the same thing)
- Outdated/unmaintained dependencies
- Feature flags enabling unused functionality
```

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
```


## Usage

Run one reviewer per objective in parallel (up to 5 at a time) for maximum coverage with minimal overlap:

```
Agent(subagent_type="code-reviewer", prompt="[Objective] review of [project]. Focus ONLY on [objective]. Read all source files. Report findings classified as CRITICAL/HIGH/MEDIUM/LOW with file:line references.")
```
