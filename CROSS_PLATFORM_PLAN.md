# findr Cross-Platform Refactor Plan

## Context

findr is a 6,397-line Rust CLI currently macOS-only. 30 platform-specific touchpoints prevent compilation on Windows/Linux. Goal: make findr compile and work on all three platforms with zero macOS regression. This unlocks the MCP server use case (any agent, any OS) and Linux support for free.

## Strategy

**Platform module with `#[cfg]`-gated free functions** — no traits, no runtime dispatch. Each platform gets its own file with identically-named functions. Compile-time selection, zero cost.

**Keep macOS behavior identical** — `~/.findr` stays, FSEvents stays, Apple Vision OCR stays.

**mtime-diff for Windows/Linux sync** — `quick_diff_sync()` already exists (main.rs:493). No new sync algorithm needed.

**OCR via `ocrs` on Linux/Windows** — pure Rust OCR (ONNX models, zero C deps). Latin-only, lower accuracy than Apple Vision, but works everywhere. macOS keeps Apple Vision.

---

## Phase 1: Foundation (Compiles on All Platforms)

### 1.1 Update `Cargo.toml`
- Move `fsevent-sys`, `core-foundation` under `[target.'cfg(target_os = "macos")'.dependencies]`
- Move `libc` under `[target.'cfg(unix)'.dependencies]`
- Add `fs4 = "0.12"` (cross-platform file locking)
- Add `directories = "5"` (cross-platform data dirs)
- Add `ocrs = "0.12"` + `rten = "0.14"` + `rten-imageproc = "0.14"` (pure Rust OCR for non-macOS)
- Switch `ureq` to use `rustls` instead of system TLS: `ureq = { version = "2", features = ["json", "tls"], default-features = false }` + add `rustls` feature. Avoids OpenSSL compile issues on Windows/cross-compilation.

### 1.0 Set up CI for all platforms
Expand `.github/workflows/ci.yml` **before** any code changes so we get immediate feedback:

**Check job → matrix:**
```yaml
check:
  name: Clippy + Test (${{ matrix.name }})
  strategy:
    matrix:
      include:
        - os: macos-latest
          name: macOS
        - os: ubuntu-latest
          name: Linux
        - os: windows-latest
          name: Windows
  runs-on: ${{ matrix.os }}
```

**Conditional macOS-only steps:**
```yaml
- name: Build findr-ocr
  if: runner.os == 'macOS'
  run: cd findr-ocr && swift build -c release

- name: Build Raycast extension
  if: runner.os == 'macOS'
  working-directory: raycast-extension
  run: |
    npm install
    npx ray build
```

Release job stays macOS-only until Phase 4.

Note: CI will fail on Linux/Windows until steps 1.1-1.7 are complete. That's expected — the first green build across all 3 platforms is the Phase 1 milestone.

### 1.2 Create `src/platform/mod.rs`
Public API — free functions re-exported from platform-specific modules:
```
data_dir() -> PathBuf
home_dir() -> Option<String>
try_lock_exclusive(file: &File) -> bool
default_scan_paths() -> Vec<&'static str>
hot_folders() -> Vec<&'static str>
os_excludes() -> Vec<&'static str>
home_excludes() -> Vec<&'static str>
extra_volume_paths() -> Vec<String>
excluded_recent_patterns() -> Vec<&'static str>
should_exclude_os_bundle(path: &str) -> bool
check_permissions() -> (bool, Vec<String>)
secure_directory(path: &Path)
find_ocr_binary() -> Option<PathBuf>
extract_ocr(path: &Path) -> Option<OcrOutput>          // Apple Vision on macOS, ocrs on others
extract_ocr_batch(paths: &[&Path]) -> Vec<OcrResult>    // Apple Vision on macOS, ocrs on others
current_change_id() -> Option<u64>
get_changes_since(id: u64, paths: &[String]) -> Option<fsevents::FsEventResult>
```

### 1.3 Create `src/platform/macos.rs`
Move existing logic verbatim:
- `data_dir()` → `~/.findr` (backwards compat)
- `home_dir()` → `env::var("HOME")`
- `try_lock_exclusive()` → `libc::flock(fd, LOCK_EX | LOCK_NB)`
- Path constants from `indexer.rs` (DEFAULT_SCAN_PATHS, OS_EXCLUDES, etc.)
- `check_permissions()` → current `check_full_disk_access()` logic
- `find_ocr_binary()` → current logic from content.rs:685-706
- `current_change_id()` / `get_changes_since()` → delegates to `fsevents.rs`

### 1.4 Create `src/platform/linux.rs`
- `data_dir()` → `directories::ProjectDirs` → `~/.local/share/findr`
- `home_dir()` → `env::var("HOME")`
- `try_lock_exclusive()` → `libc::flock` (same as macOS, Linux supports it)
- `os_excludes()` → `["/proc", "/sys", "/dev", "/run", "/snap", "/boot"]`
- `home_excludes()` → `[".cache", ".local/share/Trash"]`
- `extra_volume_paths()` → scan `/mnt` and `/media`
- `should_exclude_os_bundle()` → false
- `check_permissions()` → `(true, vec![])`
- `find_ocr_binary()` → None (OCR handled in-process via `ocrs`, no external binary)
- `extract_ocr()` / `extract_ocr_batch()` → use `ocrs` crate directly (see Phase 2.6)
- `current_change_id()` / `get_changes_since()` → None

### 1.5 Create `src/platform/windows.rs`
- `data_dir()` → `directories::ProjectDirs` → `%APPDATA%\findr`
- `home_dir()` → `env::var("USERPROFILE")`
- `try_lock_exclusive()` → `fs4::FileExt::try_lock_exclusive()`
- `os_excludes()` → `["C:\\Windows", "C:\\Program Files", "C:\\Program Files (x86)", "C:\\ProgramData"]`
- `home_excludes()` → `["AppData\\Local\\Temp", "AppData\\Local\\Microsoft"]`
- `extra_volume_paths()` → enumerate drive letters D:-Z:
- `should_exclude_os_bundle()` → false
- `check_permissions()` → `(true, vec![])`
- `find_ocr_binary()` → None (OCR handled in-process via `ocrs`, no external binary)
- `extract_ocr()` / `extract_ocr_batch()` → use `ocrs` crate directly (see Phase 2.6)
- `current_change_id()` / `get_changes_since()` → None

### 1.6 Gate `src/fsevents.rs`
- Add `#[cfg(target_os = "macos")]` to module declaration in `src/lib.rs`

### 1.7 Update `src/lib.rs`
- Add `pub mod platform;`
- Gate fsevents: `#[cfg(target_os = "macos")] pub mod fsevents;`

**Verify**: `cargo check` passes on all platforms. `cargo test` passes on macOS with zero behavior change.

---

## Phase 2: Wire Up Platform Module (Search Works Everywhere)

### 2.1 Replace `data_dir()` — `src/main.rs:14-32`
Replace body to delegate to `platform::data_dir()`. Keep `create_dir_all`. Call `platform::secure_directory()` instead of inline `#[cfg(unix)]` block.

### 2.2 Replace `try_acquire_lock()` / `try_acquire_embed_lock()` — `src/main.rs:36-51`
Replace `libc::flock` + `AsRawFd` with `platform::try_lock_exclusive(&file)`. Remove `use std::os::unix::io::AsRawFd` and `unsafe` blocks.

### 2.3 Replace HOME in `src/errors.rs:7,11,40,44`
Both functions construct `~/.findr/error.log`. Replace with `platform::data_dir().join("error.log")`.

### 2.4 Replace HOME in `src/semantic.rs:41-45`
Replace `HOME` + `.findr/openrouter_key` with `platform::data_dir().join("openrouter_key")`.

### 2.5 Replace `dirs_home()` in `src/indexer.rs:200-202`
Delegate to `platform::home_dir()`.

### 2.6 Replace OCR with platform abstraction in `src/content.rs`

**macOS** (unchanged): spawns `findr-ocr` binary (Apple Vision). `platform::find_ocr_binary()` returns path, existing `extract_ocr()` / `extract_ocr_batch()` spawn it.

**Linux/Windows** (new): `ocrs` crate called in-process. No external binary.
- `platform::extract_ocr(path)` loads image → runs ocrs detection + recognition → returns `OcrOutput { text, confidence }`
- `platform::extract_ocr_batch(paths)` runs ocrs in parallel via rayon (same pattern as macOS batch)
- ONNX models (~6MB) auto-download to `data_dir()/models/` on first OCR run
- Model download: `ocrs` handles this via `rten` model loading. Cache in data_dir so it's one-time.

**`ocrs` integration in platform/linux.rs and platform/windows.rs:**
```rust
use ocrs::{OcrEngine, OcrEngineParams};
use rten::Model;

pub fn extract_ocr(path: &Path) -> Option<OcrOutput> {
    let detection_model = Model::load_file(model_path("text-detection.rten"))?;
    let recognition_model = Model::load_file(model_path("text-recognition.rten"))?;
    let engine = OcrEngine::new(OcrEngineParams {
        detection_model: Some(detection_model),
        recognition_model: Some(recognition_model),
        ..Default::default()
    }).ok()?;
    let img = image::open(path).ok()?;
    let input = engine.prepare_input(img.into())?;
    let text = engine.get_text(&input).ok()?;
    // ... return OcrOutput with text and estimated confidence
}
```

**Model download UX:**
- Models (~6MB total) stored in `platform::data_dir()/models/`
- `findr index init` pre-downloads models so first OCR run isn't surprising
- If models missing at OCR time, download with stderr progress: `Downloading OCR models (6MB)...`
- If offline and no cached models, OCR silently skipped (same as macOS when findr-ocr binary missing)

**Limitations to document:**
- Latin alphabet only (no CJK/Arabic)
- Lower accuracy than Apple Vision (~80-85% vs ~95%)
- First run downloads ~6MB models (cached after)
- Release builds only (debug is extremely slow)

**No EXIF/GPS on Linux/Windows initially** — the current OCR binary also extracts EXIF metadata via Apple's ImageIO. `ocrs` doesn't do EXIF. Can add `kamadak-exif` or `rexif` crate later if needed.

### 2.7 Replace HOME in tilde expansion — `src/main.rs:859`
Replace `std::env::var("HOME")` with `platform::home_dir()`.

### 2.8 Platform-aware paths in `src/indexer.rs`
Replace hardcoded constants with platform module calls:
- `DEFAULT_SCAN_PATHS` (line 9) → `platform::default_scan_paths()`
- `HOT_FOLDERS` (line 56) → `platform::hot_folders()`
- `FULL_HOME_EXCLUDES` (line 70) → `platform::home_excludes()`
- `EVERYTHING_OS_EXCLUDES` (line 75) → `platform::os_excludes()`
- `/Volumes` scan (line 99) → `platform::extra_volume_paths()`
- `.app/Contents` check (line 178) → `platform::should_exclude_os_bundle()`
- `check_full_disk_access()` (line 442) → `platform::check_permissions()`

### 2.9 Platform-aware exclusions in `src/db.rs`
Replace `RECENT_EXCLUDED_PATHS` (macOS patterns like `%.app/%`, `%/Library/%`) with `platform::excluded_recent_patterns()`.

### 2.10 Update description strings
- `Cargo.toml:4` → `"The fastest local file search"`
- `main.rs:62` → remove "for macOS"

**Verify**: On all platforms: `findr --help`, `findr index init`, `findr search test`, `findr doctor` work. macOS: zero regression.

---

## Phase 3: Sync Works Everywhere

### 3.1 Refactor `fsevents_sync()` — `src/main.rs:644-726`
Rename to `incremental_sync()`. Try platform fast-path first:
```rust
if let Some(result) = platform::get_changes_since(last_id, &scan_paths) {
    // existing FSEvents processing...
    return count;
}
// Fallback: mtime-diff (works everywhere)
quick_diff_sync(db, content_idx_path)
```

### 3.2 Gate FSEvents metadata — `src/main.rs:353-354`
Wrap event ID storage:
```rust
if let Some(event_id) = platform::current_change_id() {
    temp_db.set_meta("fsevent_last_id", &event_id.to_string())?;
}
```

### 3.3 Fix `std::fs::rename` for Windows — `src/main.rs:360-388`
POSIX `rename` overwrites destination atomically. Windows `rename` **fails** if destination exists. The atomic swap in `run_full_index()` does backup → rename:
```rust
// Current (breaks on Windows):
let _ = std::fs::rename(db_path(), &bak_db);       // move old to backup
std::fs::rename(&temp_db_path, db_path())?;         // move new into place
```
Fix: explicitly remove destination before rename on all platforms, or use a helper:
```rust
fn atomic_replace(src: &Path, dst: &Path) -> Result<()> {
    let _ = std::fs::remove_file(dst);  // ignore if doesn't exist
    std::fs::rename(src, dst)?;
    Ok(())
}
```

### 3.4 Verify sync command dispatch — `src/main.rs:1067`
Ensure `findr index sync` calls `incremental_sync()` which falls through to `quick_diff_sync()` on non-macOS.

**Verify**: macOS uses FSEvents as before. Linux/Windows: `findr index init` then `touch` a file, `findr index sync` picks it up via mtime diff.

---

## Phase 4: Distribution & Docs

CI check matrix already set up in Phase 1. This phase expands the **release** job and distribution.

### 4.1 Expand release job in `.github/workflows/ci.yml`
Add multi-platform binary builds (triggered on `v*` tags):

| Runner | Target | Asset name |
|--------|--------|------------|
| macos-latest | aarch64-apple-darwin | findr-macos-arm64 |
| macos-latest | x86_64-apple-darwin | findr-macos-x86_64 |
| macos-latest | (lipo) | findr-macos-universal |
| ubuntu-latest | x86_64-unknown-linux-gnu | findr-linux-x86_64 |
| ubuntu-latest | aarch64-unknown-linux-gnu (via `cross`) | findr-linux-arm64 |
| windows-latest | x86_64-pc-windows-msvc | findr-windows-x86_64.exe |

macOS-only extras (keep existing): universal binary via `lipo`, codesign, findr-ocr universal binary.
Linux: static link with musl for zero-dependency binary (`x86_64-unknown-linux-musl`).

### 4.2 Update `install.sh`
- Detect OS via `uname -s` (Darwin/Linux)
- Download correct binary per OS+arch
- Point Windows users to `install.ps1`

### 4.3 Create `install.ps1`
- PowerShell script for Windows
- Download to `$env:LOCALAPPDATA\findr\`
- Add to user PATH

### 4.4 Update `Makefile`
- Conditional codesign (only on Darwin)
- Skip findr-ocr copy on non-macOS

### 4.5 Update `.gitignore`
- Add `*.pdb`, `*.exe`, `*.dll`, `*.dSYM`

### 4.6 Update `README.md`
- Remove "for macOS" from tagline
- Add Linux/Windows install instructions
- Note: macOS gets Apple Vision OCR (best), Linux/Windows get ocrs (good, Latin-only)
- Platform feature matrix table

---

## Phase 5: Future (Not in This Plan)

- **Cloud OCR opt-in** — `--ocr-backend=google-vision` flag accepting `GOOGLE_VISION_API_KEY` env var. Best accuracy across all platforms, trivial HTTP implementation, but requires internet + costs ~$1.50/1K images.
- **EXIF/GPS extraction** on Linux/Windows via `kamadak-exif` crate
- **Tesseract opt-in** — higher-accuracy OCR backend on Linux (`leptess` crate, `apt install tesseract-ocr`). Feature flag: `cargo build --features tesseract`
- **Windows USN Journal** via `usn-journal-rs` for faster sync (requires admin)
- **Linux fanotify** via `change-journal` for faster sync
- **Tauri v2 desktop app** (v4 roadmap) — cross-platform GUI, replaces Raycast on non-macOS
- **Linux launcher integration** — Ulauncher/Albert plugin as Linux equivalent to Raycast extension

---

## Risk Areas

1. **Path separators**: indexer.rs does string matching on paths (`.contains(".app/Contents")`, `format!("/{}/", exclude)`). Windows uses backslash. Mitigation: all platform exclude patterns use the correct separator for their OS.

2. **`std::fs::rename` on Windows**: fails if destination exists (unlike POSIX). **Fixed in Phase 3.3** with `atomic_replace()` helper that removes dest first.

3. **Background process spawning**: `spawn_background()` (main.rs:729) uses `Stdio::null()`. Works on Windows but behavior of detached processes differs. CI tests will catch crashes; edge cases fixed from user reports.

4. **SQLite on Windows**: `rusqlite` with bundled feature compiles fine. NTFS vs HFS+ case sensitivity may affect LIKE queries in db.rs. SQLite LIKE is case-insensitive for ASCII by default — should be fine.

5. **`~/.findr` migration**: macOS keeps `~/.findr`. No migration needed. Windows/Linux start fresh with platform-correct paths.

6. **`ureq` TLS backend**: Switching from system TLS to `rustls` in Phase 1.1. Low risk — `rustls` is battle-tested and used by most Rust HTTP clients. Only affects semantic search (OpenRouter API calls). If issues, can fall back to native-tls behind feature flag.

7. **`ocrs` model download on first run**: Requires internet on first OCR invocation. Mitigated by pre-downloading during `findr index init`. If offline, OCR silently skipped.

---

## Verification Plan

After each phase, run:
1. `cargo check` on macOS (native)
2. `cargo check --target x86_64-unknown-linux-gnu` (cross or CI)
3. `cargo check --target x86_64-pc-windows-msvc` (cross or CI)
4. `cargo test` on macOS (full regression)
5. Manual: `findr index init` + `findr search` + `findr index sync` on each platform

For Phase 4, verify CI builds green on all three OS runners.

---

## Tools & Dependencies Chosen

| Problem | Solution | Why |
|---------|----------|-----|
| File locking | `fs4` crate | Active fork of abandoned fs2. Drop-in `try_lock_exclusive()`. Windows via LockFileEx. |
| Data directory | `directories` crate | `ProjectDirs::from("","","findr")` gives platform-correct paths. Better than `dirs` (app-scoped). |
| FS change detection (macOS) | Keep raw FSEvents | `notify` can't do historical replay. Nothing else matches `since_event_id`. |
| FS change detection (Win/Linux) | mtime-diff via existing `quick_diff_sync()` | Already built. Zero new deps. USN journal/fanotify can come later. |
| OCR (macOS) | Keep Apple Vision | Best accuracy, zero friction, already works. |
| OCR (Linux/Windows) | `ocrs` crate (pure Rust) | Zero C deps, compiles everywhere, ~80-85% accuracy on Latin text. No Tesseract system lib needed. |
| TLS | `rustls` (replace system TLS) | Pure Rust, no OpenSSL dependency. Eliminates Windows/cross-compile headaches for `ureq` HTTP client. |
| Platform abstraction | `#[cfg]`-gated free functions | Zero cost. No traits needed. One file per OS. |
| CI | GitHub Actions matrix | macOS + Ubuntu + Windows runners. `cross` for Linux ARM. |
| File rename (Windows) | `atomic_replace()` helper | POSIX rename overwrites; Windows fails. Helper removes dest first. |
| Windows installer | PowerShell script | Native, no dependencies. |

---

## Files Modified (by phase)

| Phase | File | Action |
|-------|------|--------|
| 1 | `.github/workflows/ci.yml` | Multi-platform matrix (macOS + Ubuntu + Windows) |
| 1 | `Cargo.toml` | Conditional deps, add fs4 + directories + ocrs + rten |
| 1 | `src/lib.rs` | Add platform module, gate fsevents |
| 1 | `src/platform/mod.rs` | **NEW** — conditional re-exports |
| 1 | `src/platform/macos.rs` | **NEW** — macOS implementations |
| 1 | `src/platform/linux.rs` | **NEW** — Linux implementations (includes ocrs OCR) |
| 1 | `src/platform/windows.rs` | **NEW** — Windows implementations (includes ocrs OCR) |
| 2 | `src/main.rs` | Replace data_dir, flock, HOME, fsevents refs |
| 2 | `src/errors.rs` | Replace HOME with platform::data_dir() |
| 2 | `src/semantic.rs` | Replace HOME with platform::data_dir() |
| 2 | `src/indexer.rs` | Replace all hardcoded paths/constants |
| 2 | `src/content.rs` | Replace OCR with platform abstraction (Apple Vision on macOS, ocrs on others) |
| 2 | `src/db.rs` | Platform-aware exclusion patterns |
| 3 | `src/main.rs` | Refactor fsevents_sync → incremental_sync + atomic_replace() for Windows rename |
| 4 | `Makefile` | Conditional codesign |
| 4 | `install.sh` | OS detection (Darwin + Linux) |
| 4 | `install.ps1` | **NEW** — Windows installer |
| 4 | `.gitignore` | Windows/Linux artifacts |
| 4 | `README.md` | Multi-platform docs |

---

## Platform-Specific Code Inventory (30 touchpoints)

### Will compile-fail on Windows (must fix)
1. `main.rs:40` — `libc::flock` (sync lock)
2. `main.rs:49` — `libc::flock` (embed lock)
3. `main.rs:39` — `std::os::unix::io::AsRawFd`
4. `main.rs:48` — `std::os::unix::io::AsRawFd`
5. `fsevents.rs` — entire 182-line module (fsevent-sys, core-foundation)
6. `Cargo.toml:20-21` — unconditional fsevent-sys + core-foundation deps

### Wrong paths on Windows/Linux (will run but break)
7. `main.rs:15` — `env::var("HOME")` in data_dir()
8. `main.rs:859` — tilde expansion via HOME
9. `semantic.rs:41` — HOME for API key
10. `errors.rs:7` — HOME for error log
11. `errors.rs:40` — HOME for error log read
12. `indexer.rs:201` — dirs_home() returns HOME
13. `content.rs:697` — HOME for OCR binary at ~/.local/bin

### macOS-only logic (will silently misbehave)
14. `main.rs:353` — fsevents::current_event_id()
15. `main.rs:644-726` — fsevents_sync() function
16. `indexer.rs:76-84` — EVERYTHING_OS_EXCLUDES (/System, /Library, etc.)
17. `indexer.rs:99` — /Volumes scanning
18. `indexer.rs:178` — .app/Contents detection
19. `indexer.rs:442-465` — check_full_disk_access()
20. `db.rs:22-23` — %.photoslibrary/%, %.app/%, %/Library/%
21. `indexer.rs:9-15` — DEFAULT_SCAN_PATHS (work but OS excludes wrong)
22. `indexer.rs:56-60` — HOT_FOLDERS
23. `indexer.rs:70-72` — FULL_HOME_EXCLUDES (Library)

### Already gated (no change needed)
24. `main.rs:24-29` — `#[cfg(unix)]` permissions block
25. `semantic.rs:48-60` — `#[cfg(unix)]` key file permissions

### Build/CI (macOS-only tooling)
26. `.github/workflows/ci.yml` — macos-latest only, lipo, codesign, swift build
27. `Makefile` — codesign commands
28. `install.sh` — macOS binaries only
29. `findr-ocr/` — Swift, Apple Vision (macOS-only by nature)
30. `raycast-extension/` — macOS-only (Raycast is macOS-only, expected)
