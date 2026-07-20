# findr Desktop v1 — Implementation Plan

## Implementation Status (2026-05-27)

### Round 1: Overlay + Hotkey + Tray ✅
- NSPanel overlay via `tauri-nspanel` plugin (works over fullscreen apps)
- Global hotkey: `Cmd+Shift+F` (macOS), `Ctrl+Shift+F` (Windows/Linux)
- System tray: left-click toggle, right-click menu (Show/Settings/Quit)
- macOS `ActivationPolicy::Accessory` (no dock icon)
- Hide on Esc, click-outside (`window_did_resign_key` handler)
- Split layout: result list (42%) + preview panel (58%)
- Frosted glass overlay styling, keyboard nav, file actions

### Round 2: License Validation + Trial ✅
- `license.rs`: Polar.sh activate/validate API, machine fingerprint, 14-day trial, 7-day offline grace
- `LicenseGate.tsx`: gates app on first launch, trial banner, blocks on expiry
- `tauri-plugin-store`: persists license state to `settings.json`
- Currently bypassed for dev (`unknown` status passes through)

### Round 3: Auto-updater + CI ✅
- `tauri-plugin-updater` with GitHub Releases endpoint
- `UpdateBanner.tsx`: check + download + install + relaunch
- `.github/workflows/release.yml`: tag-triggered, macOS/Linux/Windows matrix
- `findr_version.txt`: pinned CLI version (v1.4.5)
- Tauri signing key generated, pubkey set in `tauri.conf.json`
- GitHub secrets configured: `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`

### Round 4: Settings Window + Background Daemon ✅
- Second Tauri window (decorated, normal stacking, 600x700)
- Accessible via tray menu "Settings...", gear icon in overlay footer, or `Cmd+,` (overlay-scoped)
- `tauri-plugin-dialog` for folder picker
- Sections: Scan Paths, Appearance (theme), Search Hotkey, Launch at Login, Semantic Search (API key), Index Status (live polling), Reindex, License, About
- `background.rs`: `std::thread::spawn` daemon, checks initial index, runs `findr index sync` every 5min
- Uses `Instant::now()` for elapsed time, `tauri::async_runtime::handle().block_on()` for sidecar calls
- Emits `index-sync` Tauri events consumed by Settings UI
- New commands: `get_doctor_report`, `add_scan_path`, `remove_scan_path`, `run_reindex`, `run_sync`, `set_api_key`, `get_api_key_status`, `get_autostart_status`, `set_autostart`, `open_settings`, `get_theme`, `set_theme`

### Round 5: Design System + Actions Panel + Theming ✅
- CSS custom properties design system: 40+ tokens for colors, borders, backgrounds, icons
- All components use `var(--xxx)` — no hardcoded colors
- Light + dark + system theme support via `useTheme` provider
- `data-theme` attribute on `<html>`, persisted to `tauri-plugin-store`
- System theme: listens to `prefers-color-scheme` media query changes
- Floating actions panel (Raycast-style): `Cmd+K` or `Tab` opens contextual actions
- Actions: Open, Reveal in Finder, Copy Path, Copy Filename, Settings — each with keyboard shortcut hints
- Arrow key navigation in actions panel, Enter executes, Esc closes
- Blur fix: `will-change: backdrop-filter` + `transform: translateZ(0)` on overlay
- `Cmd+,` opens settings (overlay-scoped, not global — won't steal from other apps)

### Key Technical Decisions
- **NSPanel required** for macOS fullscreen overlay (NSWindow cannot cross into fullscreen Spaces regardless of window level/flags)
- **`nonactivatingPanel` style mask** prevents Space-switching when panel shows
- **`show()` + `make_key_window()`** for immediate keyboard focus (not `show_and_make_key()`)
- **Shortcut registered once** via `app.global_shortcut().register()` in setup; handler set via plugin builder
- **Cmd+, is overlay-scoped** (frontend keydown handler), not a global shortcut — avoids stealing from other apps
- **Single React entry point** for both windows: `getCurrent().label` determines overlay vs settings rendering
- **CSS custom properties** for theming instead of Tailwind dark: classes — works across both windows, simpler to maintain
- **No CLI `remove-path`**: path removal fetches current paths from `doctor --json`, filters, calls `rebuild --paths <remaining>`
- **API key via CLI** `findr config set-key` — not direct file I/O, preserves zero-coupling principle
- **Background daemon uses raw thread** (not async_runtime::spawn) for long-running work, with `block_on()` for sidecar calls

### Before First Release (TODO)
1. Re-enable license gate in `LicenseGate.tsx`
2. Wire up Sentry crash reporting (deps installed, not configured)
3. First-run onboarding experience
4. Schema drift CI tests

---

## Goal

Ship a standalone macOS + Windows desktop app (Linux v1.1) wrapping the existing `findr` CLI as a bundled subprocess. Global hotkey opens a floating search overlay (Spotlight-style). Sold direct — license key gated.

**Repo:** [`Roderick111/findr-app`](https://github.com/Roderick111/findr-app). Open source.

**Key constraint:** zero coupling to the CLI codebase. `Roderick111/findr` repo is untouched. Desktop ships in a separate repo with no Rust dependency on findr internals. CLI bugs/refactors cannot break the desktop; desktop work cannot break the Raycast extension.

---

## Pre-Implementation Setup (one-time, before Day 1)

### Polar.sh (✅ done)

| Item | Value |
|------|-------|
| Org slug | `findr` |
| Product ID | `499639ab-c131-4dc7-9fe7-a4cde74f56f4` |
| License benefit identifier | `dd74d90b-d4a9-4545-849b-ae8adaba389e` |
| Price | $150 USD, one-time |

Hardcode these as constants in `src-tauri/src/license.rs`. Safe to commit — they're public identifiers, not secrets. The Polar.sh API key for product management goes in CI secrets if/when desktop needs to programmatically manage licenses (not needed for v1 — validation uses customer portal API which only requires the license key itself).

### Tauri signing key (TODO before first release)

```bash
mkdir -p ~/.tauri
~/.bun/bin/bunx @tauri-apps/cli@latest signer generate -w ~/.tauri/findr-app.key
```

- Public key → `tauri.conf.json` `plugins.updater.pubkey`
- Private key → GitHub secret `TAURI_SIGNING_PRIVATE_KEY`
- Passphrase → GitHub secret `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
- **Back up offline.** Lost key = no more updates to deployed installs.

### Pinned findr version

`findr_version.txt` in repo root:
```
v1.4.5
```

Bump manually after smoke-testing desktop against new findr release.

### Day-0 spikes

| # | Spike | Status |
|---|-------|--------|
| 1 | Sidecar spawn on macOS — confirm GH release binary executes without Gatekeeper friction, verify stdout/stderr separation, parse JSON | ✅ **Done.** No `com.apple.quarantine` attr on downloaded binaries (only `com.apple.provenance`). stdout = pure JSON, stderr = empty when JSON path. Codesigned adhoc (`Signature=adhoc, TeamIdentifier=not set`). |
| 2 | Spawn latency benchmark — confirm subprocess overhead acceptable for interactive UX | ✅ **Done.** `--no-semantic` (phase 1): p50=57ms, p90=61ms, p99=187ms. Full search: p50=80ms, p99=213ms. Cached `--no-sync`: 40ms flat. All within UX budget. |
| 3 | Transparent overlay on Windows | ⏳ **TODO** — needs Windows machine. Defer to Day 3 of Week 1 (someone with Windows access runs it, or rent a Windows runner via GitHub Actions for the check). |

**Spike-confirmed facts now baked into plan:**
- JSON schema verified, `findr_client.rs` types match v1.4.5 exactly (see Day 3-4 below)
- Bundle size: findr binary 18.7MB + findr-ocr 0.3MB ≈ 19MB per-platform sidecar payload
- FSEvents sync is the dominant latency source (variance), not subprocess spawn itself
- HNSW reload concern was overblown — not in critical path at v1 scales

**Remaining unknown:** Tauri's `Command::new_sidecar()` is just a thin wrapper around `std::process::Command` that resolves the platform-suffixed binary name. Mechanics identical to what we benchmarked. Low risk to defer the full Tauri integration test until Week 1 Day 2-3.

**Action item for findr CLI:** binary `--version` outputs "findr 1.3.0" but it's from v1.4.5 release — Cargo.toml version field never bumped. Cosmetic but confusing. Track separately in findr repo, not blocking desktop work.

---

## Architecture Overview

```
┌───────────────────────────────────┐         ┌────────────────────────────────┐
│  Roderick111/findr  (UNTOUCHED)   │  CI     │  Roderick111/findr-app          │
│                                   │  pulls  │                                 │
│  - Rust CLI source                │ ──────► │  - Tauri 2 shell (Rust thin)    │
│  - GitHub Release: findr binary   │ binary  │  - React frontend               │
│  - GitHub Release: findr-ocr      │ at      │  - binaries/ directory holds    │
│    binary                         │ build   │    findr + findr-ocr (sidecar)  │
└───────────────────────────────────┘  time   └──────────────┬──────────────────┘
                                                             │
                                                             ▼
                                              ┌──────────────────────────────┐
                                              │  Desktop .app / .exe          │
                                              │  ├── findr  (bundled)         │
                                              │  ├── findr-ocr (bundled)      │
                                              │  └── Tauri shell              │
                                              │       │                       │
                                              │       │ spawn sidecar         │
                                              │       ▼                       │
                                              │  findr search "..." --json    │
                                              │       │                       │
                                              │       ▼                       │
                                              │  parse JSON → render UI       │
                                              └──────────────────────────────┘
```

Desktop talks to findr exclusively via:
- **stdin/stdout JSON** — Tauri sidecar spawns `findr <subcommand> --json`, parses output
- **Filesystem** — direct read of `~/.findr/` (or platform equivalent) for status checks

No library linking. No FFI. No shared Cargo workspace. Subprocess boundary = hard isolation.

```
findr-app/
├── src-tauri/
│   ├── Cargo.toml             ← Tauri-only deps (no findr-core)
│   ├── tauri.conf.json
│   ├── src/
│   │   ├── main.rs            ← Tauri app entry
│   │   ├── commands.rs        ← #[tauri::command] handlers
│   │   ├── findr_client.rs    ← subprocess invocation + JSON parsing
│   │   ├── background.rs      ← indexing/sync daemon supervisor
│   │   ├── license.rs         ← Polar.sh key validation
│   │   └── window.rs          ← overlay show/hide helpers
│   └── binaries/              ← findr + findr-ocr binaries pulled by CI
│       ├── findr-x86_64-apple-darwin
│       ├── findr-aarch64-apple-darwin
│       ├── findr-x86_64-pc-windows-msvc.exe
│       ├── findr-x86_64-unknown-linux-gnu
│       ├── findr-ocr-x86_64-apple-darwin
│       └── findr-ocr-aarch64-apple-darwin
└── src/                       ← React frontend
    ├── overlay/
    ├── settings/
    ├── components/
    └── main.tsx
```

**Two windows:**
- `overlay` — borderless, always-on-top, frameless, hidden by default, show/hide on hotkey
- `settings` — normal titled window, opened from tray

**Background indexing:** dedicated `std::thread` spawned at app start. Spawns `findr index sync` and `findr index ocr` as detached processes on schedule. Polls `findr index status --json` for progress. Communicates with frontend via Tauri events.

---

## Crate / Package Structure

### `src-tauri/Cargo.toml`

```toml
[package]
name = "findr-desktop"
version = "1.0.0"
edition = "2021"

[lib]
name = "findr_desktop_lib"
crate-type = ["staticlib", "cdylib", "rlib"]

[dependencies]
tauri = { version = "2", features = ["tray-icon", "image-png"] }
tauri-plugin-shell = "2"            # sidecar binary spawning
tauri-plugin-global-shortcut = "2"
tauri-plugin-autostart = "2"
tauri-plugin-updater = "2"
tauri-plugin-dialog = "2"
tauri-plugin-notification = "2"
tauri-plugin-fs = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
tokio = { version = "1", features = ["full"] }
ureq = { version = "2", features = ["json"] }
dirs = "5"                          # locate ~/.findr / %APPDATA%\findr for status reads
sha2 = "0.10"                       # machine fingerprint for license
```

No `findr-core`. No workspace. Single binary. Tauri only.

---

## Key Technical Decisions

### 1. Subprocess via Tauri sidecar pattern

Use `tauri-plugin-shell` with `Command::new_sidecar("findr")`. Sidecar binaries live in `src-tauri/binaries/` with the standard Tauri naming convention (`findr-<triple>` per platform). Tauri's bundler automatically picks the correct binary for the target platform and packages it inside the .app/.exe.

`tauri.conf.json`:
```json
{
  "bundle": {
    "externalBin": [
      "binaries/findr",
      "binaries/findr-ocr"
    ]
  }
}
```

Spawning a search:
```rust
use tauri_plugin_shell::ShellExt;

let output = app.shell()
    .sidecar("findr")?
    .args(["search", &query, "--json", "--limit", "30"])
    .output()
    .await?;

let response: SearchResponse = serde_json::from_slice(&output.stdout)?;
```

`SearchResponse` / `SearchResult` types defined in `findr_client.rs` — mirror the JSON shape from `src/search.rs` in the CLI repo. **Maintain manually** when CLI JSON shape changes (rare). One JSON contract diff per CLI release.

### 2. Overlay window: single window, show/hide (not create/destroy)

Create overlay at app startup with `visible: false`. On hotkey: `window.show() + set_focus()`. On Esc: `window.hide()`. Never destroy — avoids 150-300ms cold-start jank.

```json
{
  "label": "overlay",
  "title": "",
  "width": 680,
  "height": 520,
  "decorations": false,
  "transparent": true,
  "alwaysOnTop": true,
  "visible": false,
  "resizable": false,
  "center": true,
  "focus": true
}
```

### 3. Global shortcut

`tauri-plugin-global-shortcut` v2. Default: `Cmd+Shift+F` (macOS) / `Ctrl+Shift+F` (Windows/Linux). User-configurable from day 1. Persisted in app's own settings store (NOT in findr's DB — desktop owns its own preferences to maintain isolation).

### 4. Background indexing daemon supervisor

Single `std::thread::spawn` at app init. Supervises subprocess invocations on schedule:

```rust
pub fn spawn_index_supervisor(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        // First run: check if index exists via `findr index status --json`
        let status = run_findr_status();
        if status.files_indexed == 0 {
            // Spawn `findr index init` (long-running, emit progress events)
            stream_subprocess(&app, &["index", "init", "--json"], "index-progress");
        }

        // Steady state: periodic sync
        loop {
            std::thread::sleep(Duration::from_secs(300));
            stream_subprocess(&app, &["index", "sync", "--json"], "index-sync");
        }
    });
}
```

`stream_subprocess` spawns sidecar with `Stdio::piped()`, reads stdout line-by-line, emits each JSON line as a Tauri event. Frontend listens.

OCR + embedding runs as background subprocess after initial index — same way the CLI already handles it (`findr index init` spawns OCR detached). No special handling needed in desktop.

**Do NOT use `tauri::async_runtime::spawn` for long blocking subprocess work** — blocks the async executor. Use `spawn_blocking` or a raw thread.

### 5. Auto-updater: GitHub Releases (updates the WHOLE app, binary included)

`tauri-plugin-updater` v2 with GitHub Releases endpoint. Each desktop release bundles a specific findr CLI version (pinned at CI build time). Updates replace the entire .app/.exe atomically — desktop + bundled findr update together. No version skew possible.

```json
"updater": {
  "active": true,
  "endpoints": [
    "https://github.com/Roderick111/findr-app/releases/latest/download/latest.json"
  ],
  "dialog": false,
  "pubkey": "YOUR_ED25519_PUBLIC_KEY"
}
```

Generate `latest.json` in CI with `tauri-action`. Sign with ed25519 key (`tauri signer generate`).

### 6. Binary bundling at CI build time

Desktop CI workflow downloads the latest (or pinned) findr release from `Roderick111/findr` BEFORE running `tauri build`:

```yaml
- name: Fetch findr binaries
  run: |
    FINDR_VERSION=$(gh release view --repo Roderick111/findr --json tagName -q .tagName)
    mkdir -p src-tauri/binaries
    gh release download "$FINDR_VERSION" --repo Roderick111/findr \
      -p "findr-macos-arm64" -p "findr-macos-x86_64" \
      -p "findr-linux-x86_64" -p "findr-windows-x86_64.exe" \
      -p "findr-ocr-macos-arm64" -p "findr-ocr-macos-x86_64" \
      -D /tmp/findr-bins
    # Rename to Tauri sidecar naming convention
    cp /tmp/findr-bins/findr-macos-arm64 src-tauri/binaries/findr-aarch64-apple-darwin
    cp /tmp/findr-bins/findr-macos-x86_64 src-tauri/binaries/findr-x86_64-apple-darwin
    cp /tmp/findr-bins/findr-windows-x86_64.exe src-tauri/binaries/findr-x86_64-pc-windows-msvc.exe
    cp /tmp/findr-bins/findr-linux-x86_64 src-tauri/binaries/findr-x86_64-unknown-linux-gnu
    cp /tmp/findr-bins/findr-ocr-macos-arm64 src-tauri/binaries/findr-ocr-aarch64-apple-darwin
    cp /tmp/findr-bins/findr-ocr-macos-x86_64 src-tauri/binaries/findr-ocr-x86_64-apple-darwin
    chmod +x src-tauri/binaries/*
```

A `findr_version.txt` file in repo root pins the bundled findr version (manually bumped). CI reads this rather than always grabbing latest — lets you test compatibility before shipping.

### 7. Payments + License validation: Polar.sh

One platform for checkout + license keys + tax. One-time purchase.

**Validation flow:**
1. User enters key in Settings
2. `POST https://api.polar.sh/v1/customer-portal/license-keys/validate` with key + machine fingerprint
3. Cache result + timestamp in desktop's own settings store (NOT findr DB — isolation)
4. Validate on launch (cached — no internet required if cached token is fresh < 7 days)

**Machine fingerprint:** prefer stable hardware ID over MAC address.
- macOS: `IOPlatformUUID` via `system_profiler SPHardwareDataType -json`
- Windows: `wmic csproduct get uuid` or registry `MachineGuid`
- Linux: `/etc/machine-id`
- Fallback (any platform): SHA256 of hostname + first persistent disk UUID

MAC-based fingerprint breaks when user plugs in USB ethernet adapter or renames machine. Avoid.

**Why Polar.sh:** open source, 5% + 50c, no monthly fee, built-in license keys, tax compliance.

### 8. Settings storage: desktop's own store, NOT findr DB

Original plan stored desktop settings in `findr-core` DB meta table. With subprocess isolation, that's wrong — would couple desktop to findr's schema. Instead use `tauri-plugin-store` or simple JSON file in app data dir.

What desktop owns:
- Global hotkey binding
- Launch at login flag
- License key + cached validation status
- License machine fingerprint
- Theme preference

What findr CLI owns (read-only from desktop's perspective, via `findr index status --json`):
- Scan paths + presets
- Index health
- OCR/embed progress

Settings UI changes that affect findr (e.g., "edit scan paths") = spawn `findr index add-path` / `findr index rebuild --preset X --paths Y`. Never touch findr's DB directly.

### 9. Code signing: DEFERRED (v1 ships unsigned)

No Apple Developer cert or Windows code signing cert for v1. Users will see:
- **macOS**: "app can't be opened because Apple cannot check it for malicious software" → right-click → Open to bypass
- **Windows**: SmartScreen "Windows protected your PC" → "More info" → "Run anyway"

Add signing when revenue justifies:
- Apple Developer Program: $99/yr
- Windows EV code signing: ~$200/yr

`tauri-action` handles notarization via CI secrets once certs acquired.

**Important macOS detail:** the bundled findr/findr-ocr binaries inside the .app must also be signed (or notarization rejects the whole app). Existing CLI release already produces a signed `findr` (`make deploy` codesigns). Desktop CI re-signs after bundling. For v1 (unsigned), skip — but users need to `xattr -dr com.apple.quarantine /Applications/findr.app` to clear quarantine.

### 10. Distribution

- **Website**: `findr.beautiful-apps.com` — landing page + download links
- **Downloads**: GitHub Releases on `Roderick111/findr-app`
- **Updates**: `tauri-plugin-updater` pointing at `Roderick111/findr-app` releases
- **Checkout**: Polar.sh hosted checkout page, linked from website

---

## Tauri Commands Layer

`src-tauri/src/commands.rs` — each command is a thin async wrapper that spawns findr subprocess + parses JSON.

```rust
#[tauri::command]
pub async fn search(
    app: tauri::AppHandle,
    query: String,
    limit: usize,
    no_semantic: bool,
) -> Result<SearchResponse, String> {
    let mut args = vec!["search", &query, "--json", "--limit", &limit.to_string()];
    if no_semantic { args.push("--no-semantic"); }
    let output = app.shell().sidecar("findr").map_err(stringify)?
        .args(&args)
        .output().await.map_err(stringify)?;
    serde_json::from_slice(&output.stdout).map_err(stringify)
}

#[tauri::command]
pub async fn get_index_status(app: tauri::AppHandle) -> Result<IndexStatus, String> {
    let output = app.shell().sidecar("findr").map_err(stringify)?
        .args(["index", "status", "--json"])
        .output().await.map_err(stringify)?;
    serde_json::from_slice(&output.stdout).map_err(stringify)
}

#[tauri::command]
pub async fn run_full_reindex(app: tauri::AppHandle) -> Result<(), String> {
    // Spawn detached, stream progress via events
    spawn_streaming(app, &["index", "rebuild", "--json"], "index-progress").await
}

#[tauri::command]
pub async fn add_scan_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    app.shell().sidecar("findr").map_err(stringify)?
        .args(["index", "add-path", &path])
        .output().await.map_err(stringify)?;
    Ok(())
}

#[tauri::command]
pub async fn track_interaction(
    app: tauri::AppHandle,
    path: String,
    action: String,
) -> Result<(), String> {
    // Fire-and-forget — don't await
    let _ = app.shell().sidecar("findr").map_err(stringify)?
        .args(["track", &path, "--action", &action])
        .spawn();
    Ok(())
}

#[tauri::command]
pub async fn get_settings() -> Result<DesktopSettings, String> { /* read from store */ }

#[tauri::command]
pub async fn save_settings(settings: DesktopSettings) -> Result<(), String> { /* write to store */ }

#[tauri::command]
pub async fn validate_license(key: String) -> Result<LicenseStatus, String> { /* Polar.sh call */ }
```

`DesktopSettings` is desktop-owned (hotkey, autostart, theme, license). `IndexStatus`/`SearchResponse`/`SearchResult` mirror findr's `--json` output schema.

---

## Files to Create / Modify

| File | Action | Purpose |
|------|--------|---------|
| `Cargo.toml` (root) | N/A | None — desktop lives in separate repo, no workspace |
| `findr-app/` (new repo) | CREATE | Separate GitHub repo `Roderick111/findr-app` |
| `findr-app/src-tauri/Cargo.toml` | CREATE | Tauri-only deps |
| `findr-app/src-tauri/tauri.conf.json` | CREATE | externalBin sidecar config |
| `findr-app/src-tauri/src/main.rs` | CREATE | Tauri app entry, tray, window setup |
| `findr-app/src-tauri/src/commands.rs` | CREATE | Tauri commands (subprocess wrappers) |
| `findr-app/src-tauri/src/findr_client.rs` | CREATE | Subprocess invocation, JSON types |
| `findr-app/src-tauri/src/background.rs` | CREATE | Index daemon supervisor |
| `findr-app/src-tauri/src/license.rs` | CREATE | Polar.sh validation + fingerprint |
| `findr-app/src-tauri/src/window.rs` | CREATE | Overlay show/hide |
| `findr-app/src-tauri/binaries/` | CREATE (CI populates) | Sidecar binaries |
| `findr-app/src/overlay/SearchOverlay.tsx` | CREATE | Main search UI |
| `findr-app/src/settings/Settings.tsx` | CREATE | Settings window |
| `findr-app/src/components/ResultItem.tsx` | CREATE | Result row |
| `findr-app/src/components/FileIcon.tsx` | CREATE | File type icons |
| `findr-app/src/hooks/useSearch.ts` | CREATE | Debounced search hook |
| `findr-app/src/hooks/useIndexStatus.ts` | CREATE | Index progress listener |
| `findr-app/findr_version.txt` | CREATE | Pinned findr CLI version |
| `findr-app/.github/workflows/release.yml` | CREATE | CI: fetch findr → bundle → sign → release |

**Zero changes to `Roderick111/findr` repo.** Raycast extension untouched. CLI untouched.

---

## Detailed Task Breakdown

### Week 1: Scaffold + Subprocess Layer + Search UI

**Day 1-2: Repo + Tauri scaffold**

```bash
# Create new repo
gh repo create Roderick111/findr-app --public --description "findr desktop app"
git clone https://github.com/Roderick111/findr-app
cd findr-app

# Tauri 2 + React + TS scaffold
~/.bun/bin/bunx create-tauri-app . \
  --template react-ts \
  --manager bun \
  --bundle-identifier com.beautiful-apps.findr

~/.bun/bin/bun add -d @tauri-apps/cli@^2
~/.bun/bin/bun add @tauri-apps/api@^2
~/.bun/bin/bun add @tauri-apps/plugin-shell @tauri-apps/plugin-global-shortcut
~/.bun/bin/bun add @tauri-apps/plugin-autostart @tauri-apps/plugin-updater
~/.bun/bin/bun add @tauri-apps/plugin-dialog @tauri-apps/plugin-store
~/.bun/bin/bun add tailwindcss @tailwindcss/vite
~/.bun/bin/bun add lucide-react
```

Verify: `cargo tauri dev` opens a blank window.

**Day 2-3: Sidecar binary plumbing**

1. Download current findr release locally for dev:
   ```bash
   mkdir -p src-tauri/binaries
   gh release download --repo Roderick111/findr -p "findr-macos-arm64" -D /tmp/f
   cp /tmp/f/findr-macos-arm64 src-tauri/binaries/findr-aarch64-apple-darwin
   chmod +x src-tauri/binaries/findr-aarch64-apple-darwin
   ```
2. Add `externalBin` to `tauri.conf.json`
3. Write `findr_client.rs` with one test command: `version()` → spawns `findr --version`
4. Verify: invoke from frontend, see version string

**Day 3-4: Search command + JSON types**

Schema verified empirically against `v1.4.5` release binary (Day-0 spike).

`findr_client.rs`:
```rust
#[derive(Deserialize, Serialize, Debug)]
pub struct SearchResponse {
    pub query: String,
    pub mode: String,                       // "unified", "recent", "too_short"
    pub elapsed_ms: u64,
    pub total_results: u64,
    pub results: Vec<SearchResult>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SearchResult {
    pub path: String,
    pub filename: String,
    pub score: f64,
    pub match_type: String,                 // "unified", "recent", etc
    pub size_bytes: Option<u64>,
    pub modified: Option<String>,           // ISO 8601 string ("2026-05-24T17:11:06+00:00")
    pub file_type: Option<String>,
    pub content_snippet: Option<String>,
    pub is_dir: bool,
    pub interactions: u32,
}

pub async fn search(app: &AppHandle, query: &str, limit: usize, no_semantic: bool)
    -> Result<SearchResponse>
{
    let mut args = vec!["search".into(), query.into(), "--json".into(),
                        "--limit".into(), limit.to_string()];
    if no_semantic { args.push("--no-semantic".into()); }
    let output = app.shell().sidecar("findr")?.args(&args).output().await?;
    Ok(serde_json::from_slice(&output.stdout)?)  // stderr cleanly separated, ignore
}
```

Types mirror findr's actual JSON output. Verified against v1.4.5. Keep in sync by hand via CI schema test (see Observability section). One-time port + occasional drift fixes on findr version bumps.

**Day 4-5: Search overlay UI**

Reference `raycast-extension/src/search.tsx` for shape — same `SearchResponse`/`SearchResult`.

```
<div class="overlay-root">          ← full-screen transparent click-to-dismiss
  <div class="search-panel">        ← centered 680×520, rounded, frosted glass
    <SearchInput />                  ← autofocus, debounced 300ms
    <ResultsList />                  ← keyboard nav, virtualized if >50 results
    <StatusBar />                    ← "123 files · semantic ready · 45ms"
  </div>
</div>
```

Keyboard nav: `ArrowUp/Down` selection, `Enter` opens, `Cmd+Enter` reveals in Finder, `Esc` calls `hide_overlay` Tauri command.

Two-phase search (mirrors Raycast extension):
1. Fire fast search immediately (`no_semantic=true`)
2. After 800-1000ms delay, fire full search, merge results by path

```ts
const useSearch = (query: string) => {
  const [results, setResults] = useState<SearchResult[]>([]);
  const semanticTimer = useRef<ReturnType<typeof setTimeout>>();

  useEffect(() => {
    if (!query || query.length < 2) return;
    invoke<SearchResponse>("search", { query, limit: 30, noSemantic: true })
      .then(r => setResults(r.results));
    clearTimeout(semanticTimer.current);
    semanticTimer.current = setTimeout(() => {
      invoke<SearchResponse>("search", { query, limit: 30, noSemantic: false })
        .then(r => setResults(prev => mergeResults(prev, r.results)));
    }, 800);
  }, [query]);
};
```

---

### Week 2: Tray + Settings + Background Daemon + Updater

**Day 6-7: System tray**

`main.rs` tray setup with menu items: Open findr / Settings / Check for Updates / Quit.

macOS: `LSUIElement = true` in `Info.plist` (no dock icon), `app.set_activation_policy(ActivationPolicy::Accessory)`.

**Day 7-8: Background indexing daemon**

`background.rs`:
```rust
pub struct DaemonState {
    pub status: IndexStatus,
    pub last_sync: Option<DateTime<Utc>>,
}

pub fn spawn_index_supervisor(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        // 1. Check status, init if empty
        // 2. Loop: sleep 300s, run incremental sync
        // 3. Stream subprocess stdout as events
    });
}
```

Emit pattern (call from worker thread):
```rust
app.emit("index-progress", IndexProgressPayload { pct: 0.45, files_done: 12000 }).ok();
```

Frontend `useIndexStatus.ts` listens.

Subprocess management quirks to handle:
- `Stdio::null()` for stdin to detach
- Capture stderr separately (findr writes errors there in non-JSON mode)
- Kill child on app exit (Tauri's lifetime guarantees aren't enough for spawned subprocesses on Windows — use `Job Objects` or explicit `kill_on_drop`)
- On macOS, sleep/wake doesn't pause `thread::sleep` predictably — use `Instant::now()` comparisons instead of accumulated sleep duration

**Day 9-10: Settings window**

Separate Tauri window `settings` — normal decorations, `800×600`, not always-on-top.

Sections:
- **Scan Paths** — preset selector (Personal/Full Home/Everything) + add/remove custom paths. Saving = spawn `findr index rebuild --preset X --paths Y`
- **Search Hotkey** — capture key combo, persist to desktop store, re-register
- **Launch at Login** — `tauri-plugin-autostart`
- **Index Status** — read from `findr index status --json` every 2s when settings open
- **Reindex** — button triggers `run_full_reindex`
- **License** — text input + Activate button, shows cached status
- **About** — desktop version, bundled findr version, GitHub link

**Day 10-11: Auto-updater**

```rust
// In main.rs, on app ready:
tauri::async_runtime::spawn(async move {
    if let Ok(Some(update)) = app.updater().check().await {
        app.emit("update-available", UpdatePayload {
            version: update.version.clone(),
            notes: update.body.clone(),
        }).ok();
    }
});
```

When user accepts: `update.download_and_install()` → app restarts with new bundle (which includes new findr binary).

---

### Week 3: License + First-Run + Polish + Distribution

**Day 12-13: License validation**

`license.rs`:
```rust
// Public Polar.sh identifiers — safe to commit
const POLAR_ORG_SLUG: &str = "findr";
const POLAR_PRODUCT_ID: &str = "499639ab-c131-4dc7-9fe7-a4cde74f56f4";
const POLAR_BENEFIT_ID: &str = "dd74d90b-d4a9-4545-849b-ae8adaba389e";

pub async fn validate_key(key: &str, store: &Store) -> Result<LicenseStatus> {
    let fingerprint = machine_fingerprint()?;
    let resp = ureq::post("https://api.polar.sh/v1/customer-portal/license-keys/validate")
        .send_json(json!({
            "key": key,
            "organization_id": POLAR_ORG_SLUG,
            "conditions": { "activations": { "fingerprint": fingerprint } }
        }))?;
    let parsed: PolarValidateResponse = resp.into_json()?;
    store.set("license_status", json!(parsed.status));
    store.set("license_validated_at", json!(Utc::now().to_rfc3339()));
    store.set("license_key", json!(key));  // for re-validation on launch
    store.save()?;
    Ok(parsed.into())
}

fn machine_fingerprint() -> Result<String> {
    // Try stable hardware UUID first; fall back to hostname + disk UUID
    #[cfg(target_os = "macos")] { /* IOPlatformUUID via IORegistry */ }
    #[cfg(target_os = "windows")] { /* registry HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid */ }
    #[cfg(target_os = "linux")] { /* /etc/machine-id */ }
    // SHA-256, first 16 bytes hex
}
```

Offline grace period: if no internet, check cached timestamp. If < 7 days old, allow. Stored in desktop's own settings store.

**Trial mode:** if no license key entered, app runs in 14-day trial. First launch: `store.set("trial_started_at", now)`. On each launch: compute days remaining. After 14 days, search returns gracefully degraded results with a "Trial expired" banner + "Activate License" button. Trial is locally gameable (user can clear store) — acceptable for v1, fix in v2 with server-side activation tracking.

**Day 13-14: First-run experience**

Detect first run: parse `findr index status --json`, check `files_indexed == 0`.

First-run flow shown in **settings window** (NOT overlay — bigger surface, better fit):
1. Welcome screen
2. Path selection — preset cards + custom path picker via `tauri-plugin-dialog`
3. License input — optional, can skip (start 14-day trial)
4. "Start indexing" — calls `run_full_reindex`, transitions to overlay with progress
5. When done: dismiss settings, show normal search overlay

**Day 14-15: Polish**

- Frosted glass: CSS `backdrop-filter: blur(20px)` + semi-transparent bg. Requires `transparent: true` on window + `-webkit-backdrop-filter` for macOS
- Result icons: `lucide-react` or SVG map by extension. Port `raycast-extension/src/utils.ts` `getFileIcon` mapping
- Keyboard shortcuts tooltip in StatusBar
- Loading skeleton during search
- Empty state: "Type to search" / "No results for X"
- Error state: "Index not built — click to rebuild" + "findr binary not found" (fallback if sidecar fails)
- Smooth show/hide animation: CSS opacity + scale (100ms)
- Trial countdown banner if license = trial

**Day 15-16: Distribution setup**

CI `release.yml`:
```yaml
name: Release
on:
  push:
    tags: ['v*']

jobs:
  release:
    strategy:
      matrix:
        platform: [macos-latest, windows-latest, ubuntu-latest]
    runs-on: ${{ matrix.platform }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: oven-sh/setup-bun@v2

      - name: Read pinned findr version
        id: ver
        run: echo "version=$(cat findr_version.txt | tr -d '[:space:]')" >> $GITHUB_OUTPUT

      - name: Fetch findr binaries
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        shell: bash
        run: |
          mkdir -p src-tauri/binaries
          V=${{ steps.ver.outputs.version }}
          gh release download "$V" --repo Roderick111/findr \
            -p "findr-macos-arm64" -p "findr-macos-x86_64" \
            -p "findr-linux-x86_64" -p "findr-windows-x86_64.exe" \
            -p "findr-ocr-macos-arm64" -p "findr-ocr-macos-x86_64" \
            -D /tmp/findr-bins
          cp /tmp/findr-bins/findr-macos-arm64 src-tauri/binaries/findr-aarch64-apple-darwin
          cp /tmp/findr-bins/findr-macos-x86_64 src-tauri/binaries/findr-x86_64-apple-darwin
          cp /tmp/findr-bins/findr-windows-x86_64.exe src-tauri/binaries/findr-x86_64-pc-windows-msvc.exe
          cp /tmp/findr-bins/findr-linux-x86_64 src-tauri/binaries/findr-x86_64-unknown-linux-gnu
          cp /tmp/findr-bins/findr-ocr-macos-arm64 src-tauri/binaries/findr-ocr-aarch64-apple-darwin
          cp /tmp/findr-bins/findr-ocr-macos-x86_64 src-tauri/binaries/findr-ocr-x86_64-apple-darwin
          chmod +x src-tauri/binaries/*

      - uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          # Code signing deferred — add when certs acquired:
          # APPLE_CERTIFICATE, APPLE_CERTIFICATE_PASSWORD, APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
        with:
          tagName: v__VERSION__
          releaseName: "findr Desktop v__VERSION__"
          releaseBody: "See CHANGELOG.md. Bundles findr CLI ${{ steps.ver.outputs.version }}."
          releaseDraft: true
          includeUpdaterJson: true
```

---

## Dependencies to Install

### Rust (`src-tauri/Cargo.toml`)

```toml
tauri = "2"
tauri-plugin-shell = "2"
tauri-plugin-global-shortcut = "2"
tauri-plugin-autostart = "2"
tauri-plugin-updater = "2"
tauri-plugin-dialog = "2"
tauri-plugin-notification = "2"
tauri-plugin-fs = "2"
tauri-plugin-store = "2"
tokio = { version = "1", features = ["full"] }
ureq = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
dirs = "5"
sha2 = "0.10"
chrono = "0.4"
sentry = { version = "0.34", features = ["anyhow", "panic"] }
tauri-plugin-sentry = "0.4"  # confirm latest at install time
# tauri-plugin-aptabase  # deferred to v2 — see Telemetry section
```

### JS (in `findr-app/`)

```bash
~/.bun/bin/bun add @tauri-apps/api@^2
~/.bun/bin/bun add @tauri-apps/plugin-shell
~/.bun/bin/bun add @tauri-apps/plugin-global-shortcut
~/.bun/bin/bun add @tauri-apps/plugin-autostart
~/.bun/bin/bun add @tauri-apps/plugin-updater
~/.bun/bin/bun add @tauri-apps/plugin-dialog
~/.bun/bin/bun add @tauri-apps/plugin-store
~/.bun/bin/bun add @sentry/react
~/.bun/bin/bun add -d @sentry/vite-plugin
# ~/.bun/bin/bun add @aptabase/tauri  # deferred to v2 — see Telemetry section
~/.bun/bin/bun add lucide-react
~/.bun/bin/bun add -d tailwindcss @tailwindcss/vite
~/.bun/bin/bun add -d typescript @types/react @types/node
```

---

## Risk Areas and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Subprocess spawn latency adds up | Medium | Per-spawn ~5-15ms on macOS, ~30ms on Windows. Two-phase search = 2 spawns per query (acceptable). Batch via `--limit` to avoid pagination spawns. |
| HNSW reloads from disk per spawn | Low (v1) | Real cost at 500K+ files. v1 scales (<100K) = invisible. If becomes issue, build a persistent helper daemon process in v2. |
| findr JSON schema drifts | High | Pin findr version in `findr_version.txt`. Test desktop against pinned version. Bump version + re-test before each desktop release. |
| Global hotkey conflicts (e.g., macOS Option+Space) | High | Default `Cmd+Shift+F` / `Ctrl+Shift+F`. Configurable day 1. |
| Overlay transparency on Windows | Medium | Test on Windows early (Day 3). `transparent: true` works differently on Win10 vs Win11. |
| `set_activation_policy(Accessory)` breaks cmd-tab | Medium | Expected for tray-resident app. Document. Tray click focuses. |
| Background subprocess not killed on app exit (Windows) | Medium | Use `kill_on_drop` + Job Objects on Windows. Test app quit path explicitly. |
| Subprocess stdin/stdout buffering on Windows | Medium | Line-buffered reads can stall. Use `BufReader::read_until(b'\n')`. |
| Bundled findr binary fails to execute (perms, quarantine) | Medium | On macOS unsigned: instruct users to clear quarantine. After signing: notarize the bundled binary too. |
| Polar.sh API changes | Low | Open source, versioned API. |
| macOS notarization rejects bundled binaries | Medium (when signing) | Re-sign findr + findr-ocr inside `.app/Contents/MacOS/` during CI before notarizing. |
| Trial expiry / clock manipulation | Low | Server-validated activation date when license entered. Trial = local-only, easy to game (acceptable for v1). |

---

## Existing Code to Reuse Directly

- `raycast-extension/src/utils.ts` `getFileIcon` — port extension→icon mapping to `FileIcon.tsx`
- `raycast-extension/src/search.tsx` `ResultActions` — direct reference for action menu structure
- `raycast-extension/src/utils.ts` binary download/checksum logic — reference for sidecar bundling in CI
- findr's `--json` output schema (from `src/search.rs`) — port shapes to `findr_client.rs` Rust types

No code physically shared. Pattern + shape reuse only.

---

## Observability

### Crash reporting: Sentry

Selling a paid product without crash data = flying blind. Add Sentry from day 1.

**Use `tauri-plugin-sentry` by timfish** (community plugin, recommended by Sentry support). Unifies Rust panics + macOS/Windows native minidumps + JS errors into a single Sentry project. More mature than the official `sentry-tauri` crate for Tauri 2.

**DSN:** `https://1ef441fdd1202426505007899c0726c2@o4511455386599424.ingest.us.sentry.io/4511455416156160`

DSNs are write-only public keys — safe to commit in open-source repos. Or use env var for cleanliness.

**Features enabled:** Error Monitoring only. Tracing, Session Replay, Logging, Profiling all skipped:
- Tracing: desktop subprocess calls (not HTTP) — `browserTracingIntegration` provides no useful data. Wastes 5K event quota.
- Session Replay: **privacy disaster** for a filesystem search tool (would record user filenames + paths + content snippets).
- Logging: doubles event quota burn. Breadcrumbs already give us context.
- Profiling: experimental, requires cross-origin isolation headers (not relevant for Tauri).

#### Rust side

`src-tauri/Cargo.toml`:
```toml
sentry = { version = "0.34", features = ["anyhow", "panic"] }
tauri-plugin-sentry = "0.4"  # confirm latest at install time
```

`src-tauri/src/main.rs`:
```rust
const SENTRY_DSN: &str = "https://1ef441fdd1202426505007899c0726c2@o4511455386599424.ingest.us.sentry.io/4511455416156160";

fn main() {
    let client = sentry::init((
        SENTRY_DSN,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            traces_sample_rate: 0.0,  // errors only
            ..Default::default()
        },
    ));

    tauri::Builder::default()
        .plugin(tauri_plugin_sentry::init(&client))
        // ... rest of setup
        .run(tauri::generate_context!())
        .expect("error while running tauri");
}
```

#### React side (follows official Sentry React SDK pattern)

Per [Sentry React skill](https://github.com/getsentry/sentry-for-ai/blob/main/skills/sentry-react-sdk/SKILL.md): `Sentry.init()` lives in a **dedicated sidecar file** imported FIRST in entry, before any other code.

Install:
```bash
~/.bun/bin/bun add @sentry/react
~/.bun/bin/bun add -d @sentry/vite-plugin
```

`src/instrument.ts` (new file):
```typescript
import * as Sentry from "@sentry/react";

Sentry.init({
  dsn: import.meta.env.VITE_SENTRY_DSN,
  environment: import.meta.env.MODE,
  release: import.meta.env.VITE_APP_VERSION,

  sendDefaultPii: false,  // desktop app — never send PII (user filenames are sensitive)

  // Error monitoring only — no tracing, no replay, no logging
  tracesSampleRate: 0,
  enableLogs: false,
});
```

`src/main.tsx`:
```tsx
import "./instrument";  // MUST be first import

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { reactErrorHandler } from "@sentry/react";  // React 19+
import App from "./App";

createRoot(document.getElementById("root")!, {
  onUncaughtError: reactErrorHandler(),
  onCaughtError: reactErrorHandler(),
  onRecoverableError: reactErrorHandler(),
}).render(
  <StrictMode>
    <App />
  </StrictMode>
);
```

If Tauri scaffolds React 18 instead of 19, use `<Sentry.ErrorBoundary>` wrapping `<App />` instead of `reactErrorHandler()`.

`.env` (gitignored):
```
VITE_SENTRY_DSN=https://1ef441fdd1202426505007899c0726c2@o4511455386599424.ingest.us.sentry.io/4511455416156160
VITE_APP_VERSION=1.0.0
SENTRY_AUTH_TOKEN=sntrys_...    # for source map uploads, get from Sentry settings
SENTRY_ORG=findr                 # your Sentry org slug
SENTRY_PROJECT=findr-desktop     # the React project you created
```

`vite.config.ts` source map upload:
```typescript
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { sentryVitePlugin } from "@sentry/vite-plugin";

export default defineConfig({
  build: { sourcemap: "hidden" },
  plugins: [
    react(),
    sentryVitePlugin({
      org: process.env.SENTRY_ORG,
      project: process.env.SENTRY_PROJECT,
      authToken: process.env.SENTRY_AUTH_TOKEN,
    }),
  ],
});
```

Without source maps, stack traces from production builds show minified gibberish.

#### Verification (Day 1)

Drop a temporary test button anywhere in the app:
```tsx
import * as Sentry from "@sentry/react";

export function SentryTest() {
  return (
    <>
      <button onClick={() => { throw new Error("findr Sentry test"); }}>
        Trigger JS error
      </button>
      <button onClick={() => Sentry.captureMessage("findr test message", "info")}>
        Trigger test message
      </button>
    </>
  );
}
```

For Rust side, add a temp Tauri command that panics:
```rust
#[tauri::command]
fn trigger_panic() { panic!("findr Sentry Rust test"); }
```

Confirm both appear in Sentry dashboard within seconds. Delete test code before release.

#### Privacy

Explicit toggle in Settings ("Send crash reports") — default ON. `sendDefaultPii: false` keeps IP addresses out of events. Document in Privacy policy.

#### Escape hatch

If self-hosting needed (EU residency, cost, data sovereignty), GlitchTip is Sentry-API-compatible and self-hosts on a $6 Hetzner VPS (~2GB RAM). Same SDK code — just swap DSN. Zero lock-in.

**Free tier limits:** 5K errors/month, 30-day retention, 1 seat (solo dev OK). Plenty for v1 at <500 users.

### Telemetry: deferred to v2

**Skipped for v1.** Sentry breadcrumbs around errors give the most valuable signal (what was the user doing when it broke). Aggregate usage metrics (DAU/MAU/retention/version distribution) can wait until there are actual users to measure.

**Why deferred:**
- aptabase.com SaaS has been unreliable; self-hosting requires ~3-4GB RAM that the existing server doesn't have (currently 2.1GB free)
- At pre-launch / <50 users, analytics infrastructure investment is premature
- Sentry alone gives crash data + breadcrumb context (recent user actions before each error) — enough signal for v1

**Revisit when:**
- After v1 launch, when there's actual usage to measure
- Or when server is upgraded (Hetzner CX32 / 8GB tier, ~€10/mo) — easy unlock for self-host
- Or build trivial custom telemetry (Hono endpoint + SQLite, ~50 lines, fits current RAM)

**Top contenders when revisiting:**
- **Aptabase** (self-host or SaaS) — best fit for Tauri apps, anonymous-by-design
- **PostHog** — if feature flags / A/B testing / session replay become valuable
- **Custom Hono + SQLite** — minimum infra, full control, no fancy dashboard

See `[VISION_V3_DISTRIBUTED.md]` and v2 roadmap for context on when this becomes valuable.

### JSON schema drift detection

Risk: findr CLI changes `--json` output shape, desktop silently mis-parses, results disappear or app crashes.

Mitigation: CI test runs against the pinned findr binary and validates schema.

`findr-app/tests/schema.rs`:
```rust
#[tokio::test]
async fn search_response_matches_schema() {
    let output = Command::new("src-tauri/binaries/findr-aarch64-apple-darwin")
        .args(["search", "test", "--json", "--limit", "5"])
        .output().await.unwrap();
    let parsed: findr_desktop_lib::SearchResponse =
        serde_json::from_slice(&output.stdout).expect("schema drift");
    assert!(parsed.results.iter().all(|r| !r.path.is_empty()));
}

#[tokio::test]
async fn status_response_matches_schema() { /* same idea for index status */ }
```

CI runs these on every `findr_version.txt` bump. If they break, fix `findr_client.rs` types before merging.

---

## App Assets

### Icon

v1 ships with placeholder icon. Use [icon.kitchen](https://icon.kitchen) or generate via Tauri's icon tool from a single 1024×1024 PNG:
```bash
~/.bun/bin/bunx @tauri-apps/cli@latest icon path/to/source-icon.png
```
Generates all required sizes for macOS (.icns), Windows (.ico), Linux. Drops into `src-tauri/icons/`.

Pre-release: hire designer (Dribbble, $200-500 for full icon set). Don't ship to paying users with a placeholder.

### Tray icon

Separate, monochrome, 22×22 PNG. macOS uses template image mode (automatic light/dark adaptation) — requires PNG with transparent background + black pixels only.

### Marketing site

Defer to pre-release. Single-page Astro or Next.js site on `findr.beautiful-apps.com`. Sections: hero + demo gif, features, pricing ($150 one-time), download CTAs, FAQ. Polar.sh checkout link as primary CTA.

---

## Resolved Decisions

| Decision | Answer |
|----------|--------|
| Repo | `Roderick111/findr-app`, **open source** |
| Architecture | Subprocess (Tauri sidecar). findr binary bundled at CI build time. |
| Coupling to findr CLI | Zero. JSON contract only. |
| Binary distribution | Bundled in .app/.exe via Tauri `externalBin`. Pinned via `findr_version.txt`. |
| Pinned findr version | `v1.4.5` |
| Updates | Tauri auto-updater replaces entire app (bundled binary included). |
| Hotkey default | `Cmd+Shift+F` (macOS) / `Ctrl+Shift+F` (Windows/Linux), user-configurable |
| Payments | Polar.sh — org `findr`, product `499639ab-c131-4dc7-9fe7-a4cde74f56f4`, benefit `dd74d90b-d4a9-4545-849b-ae8adaba389e` |
| **Price** | **$150 USD, one-time** |
| Domain | `findr.beautiful-apps.com` |
| Bundle ID | `com.beautiful-apps.findr` |
| Code signing | Deferred for v1 (ship unsigned) |
| Settings persistence | Desktop's own `tauri-plugin-store` (NOT findr DB) |
| First-run UX | Settings window (not overlay) |
| Overlay click-outside | Hide on click-outside (Spotlight behavior) |
| Machine fingerprint | Stable hardware UUID (IOPlatformUUID/MachineGuid/machine-id), NOT MAC address |
| Trial | 14 days, locally gameable for v1 |
| macOS minimum | 12.0 (Monterey) — required for findr-ocr Apple Vision |
| Linux v1 | Deferred to v1.1. Focus macOS + Windows for v1. |
| Crash reporting | Sentry SaaS free tier via `tauri-plugin-sentry` (timfish). Error Monitoring only — skip Logs/Metrics/Replay/Tracing. GlitchTip self-host as escape hatch. |
| Telemetry | **Deferred to v2.** Sentry breadcrumbs cover the critical signal for v1. Revisit after launch when there are real users to measure + server has headroom. |
| Schema drift protection | CI tests validate JSON shape against pinned findr binary |
| App icon v1 | Placeholder via `tauri icon` generator. Designer-made before release. |

---

## Honest Note on Price

$150 one-time is **aggressive positioning** for a desktop search tool. Reference points:
- Spotlight: free
- Raycast: free + Pro at $8/mo (~$96/yr)
- Alfred Powerpack: ~$43
- HoudahSpot: $34
- Find Any File: $6

$150 puts findr in productivity-power-tool territory (Things 3 $50, iA Writer $50, Reflect $120/yr, CleanShot X $29-$69). Works if:
- Landing page communicates "this saves you hours per week"
- 14-day trial converts cold visitors
- Targets developers / power users / consultants with high hourly rates
- Demo video shows the "I found a file Spotlight couldn't" moment clearly

If conversion is weak post-launch, easier to lower price than raise it. Worth A/B testing pre-launch with a smaller cohort.

Not relitigating — your call. Just flagging for awareness.

---

## Remaining Questions

1. **Semantic search API key UX**: User enters OpenRouter key in Settings. Desktop writes to findr's config location (`~/.findr/openrouter_key` etc). This is the **only** place desktop touches findr's filesystem directly. Acceptable because the file path + format is stable, documented in README. Confirm OK?
2. **Analytics deferred to v2.** Sentry only for v1. Decide on analytics path post-launch based on actual user behavior + server headroom.
3. **Marketing site stack** — Astro / Next.js / plain HTML? Defer to pre-release.
4. **Designer for app icon** — hire pre-release or use placeholder for soft launch? Recommend placeholder OK for first 50 users, designer before public launch.
