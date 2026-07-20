# findr v3 Vision — Distributed Search

## One-liner

Search everything you own — every device, every drive, every cloud — from one hotkey.

---

## Core Idea

findr's search index syncs across devices. Your laptop searches your desktop's files. Your desktop searches your NAS. Cloud files appear alongside local results. All from one unified search overlay. No server, no cloud storage — P2P index sync + on-demand file fetch.

---

## Architecture

```
┌──────────────┐     P2P (Iroh/QUIC)     ┌──────────────┐
│   MacBook    │◄────────────────────────►│   Desktop    │
│              │     index sync +         │              │
│  findr DB    │     file streaming       │  findr DB    │
│  (SQLite +   │                          │  (SQLite +   │
│   Tantivy)   │                          │   Tantivy)   │
└──────┬───────┘                          └──────┬───────┘
       │                                         │
       │  Cloud API (OAuth)                      │  USB / Network
       ▼                                         ▼
┌──────────────┐                          ┌──────────────┐
│ Google Drive │                          │ External SSD │
│ Dropbox      │                          │ NAS          │
│ OneDrive     │                          │ USB Drive    │
└──────────────┘                          └──────────────┘
```

**Search query flows:**
1. User types query on MacBook
2. findr searches local index (instant, <100ms)
3. findr searches synced remote indexes (instant — already local copy)
4. Results merge + rank together across all sources
5. Click local file → opens
6. Click remote device file → P2P fetch → opens (or "connect to network" prompt)
7. Click cloud file → opens in browser

---

## Universal File Addressing

Every file gets a source-prefixed path (inspired by Spacedrive's SdPath):

```
local://macbook/Users/daniel/Documents/invoice.pdf
local://desktop/C:/Users/daniel/projects/report.docx
local://nas/shared/photos/vacation.jpg
gdrive://My Drive/invoices/q4-report.pdf
dropbox://Work/contracts/nda.pdf
onedrive://Documents/budget.xlsx
```

Same SQLite schema, same Tantivy content index, same ranking. Source is just a column.

---

## Three Tiers of Distribution

### Tier 1: Multi-device index sync (LAN)
- **What**: Sync findr's search index between machines on same network
- **How**: Iroh crate (QUIC-based P2P). mDNS discovery on LAN. Share SQLite metadata + Tantivy index
- **Index size**: ~5-50 MB per device (metadata only, not files)
- **File access**: On click, fetch file via P2P stream (64KB chunks, encrypted)
- **Offline**: Search results persist from last sync. Files unavailable until reconnected
- **Effort**: 3-4 weeks
- **Key crates**: `iroh`, `blake3`, `chacha20poly1305`

### Tier 2: Multi-device over internet
- **What**: Same as Tier 1 but works across networks (home + office, travel)
- **How**: Iroh NAT hole-punching for direct connection. Iroh relay servers as fallback (encrypted, relay can't read data)
- **Extra complexity**: Device pairing/auth, relay infrastructure
- **Effort**: +2-3 weeks on top of Tier 1

### Tier 3: Cloud storage search
- **What**: Index Google Drive, Dropbox, OneDrive files alongside local results
- **How**: OAuth + provider API. Index metadata (name, path, size, modified). Optionally download + extract content for full-text search
- **File access**: Click opens browser URL (or downloads then opens)
- **Change detection**: Webhooks (Google Drive push notifications) or polling
- **Per-provider effort**: 2-3 weeks each (OAuth paperwork is the bottleneck)
- **Providers by priority**: Google Drive > Dropbox > OneDrive > iCloud (no API, skip)

---

## P2P Networking (Iroh)

Iroh handles the hard parts:
- **Discovery**: mDNS on LAN, relay-assisted on internet
- **NAT traversal**: UDP hole-punching, no port forwarding needed
- **Encryption**: TLS over QUIC, all traffic encrypted
- **Relay fallback**: if direct connection fails, routes through Iroh relay (can't read content)
- **Protocol**: QUIC — multiplexed, low-latency, handles unstable connections

**What we send over Iroh:**
- Index sync: SQLite diff (new/modified/deleted file metadata)
- File fetch: chunked streaming (64KB blocks), integrity verified with BLAKE3

---

## Sync Model

Leaderless — no "master" device. Every device is equal.

**Sync strategy (simple version for findr):**
- Each device tracks a `sync_version` counter (increments on any DB change)
- On connect: exchange versions. Higher version sends diff since other's version
- Conflict resolution: last-write-wins by timestamp (HLC if needed later)
- Merge: remote files get `device_slug` prefix in path, stored in same DB

**What syncs:**
- File metadata (path, filename, size, modified, file_type, content_hash)
- Content index entries (Tantivy docs)
- OCR text
- Embedding vectors (HNSW)

**What does NOT sync:**
- Actual file bytes (fetched on-demand via P2P)
- Interaction history (per-device)
- Settings/preferences (per-device)

---

## Cloud Integration Details

### Google Drive
- **API**: Google Drive API v3
- **Auth**: OAuth 2.0 (requires Google Cloud project + app verification for sensitive scopes)
- **File listing**: `files.list` with `pageToken` pagination
- **Content**: `files.get?alt=media` to download for content extraction
- **Change detection**: `changes.list` with `startPageToken` (push notifications available)
- **Rate limits**: 12,000 queries per 100 seconds
- **Gotcha**: App verification required for production (takes weeks). Can use "testing" mode with <100 users initially

### Dropbox
- **API**: Dropbox API v2
- **Auth**: OAuth 2.0 (simpler app review than Google)
- **File listing**: `files/list_folder` + `files/list_folder/continue`
- **Content**: `files/download`
- **Change detection**: `files/list_folder/longpoll` (long polling)
- **Rate limits**: Generous for individual users

### OneDrive
- **API**: Microsoft Graph API
- **Auth**: OAuth 2.0 via Microsoft identity platform
- **File listing**: `/me/drive/root/children` recursive
- **Change detection**: Delta queries (`/me/drive/root/delta`)
- **Rate limits**: Per-app throttling

---

## Data Model Changes for findr DB

```sql
-- New: device registry
CREATE TABLE devices (
    id TEXT PRIMARY KEY,          -- UUID
    slug TEXT UNIQUE NOT NULL,    -- "macbook", "desktop-pc"
    name TEXT NOT NULL,           -- "Daniel's MacBook Pro"
    iroh_node_id TEXT,            -- for P2P connection
    last_seen INTEGER,            -- unix timestamp
    sync_version INTEGER DEFAULT 0
);

-- Modified: files table gets source info
ALTER TABLE files ADD COLUMN source TEXT DEFAULT 'local';     -- local, gdrive, dropbox, onedrive
ALTER TABLE files ADD COLUMN device_id TEXT;                   -- which device owns this file
ALTER TABLE files ADD COLUMN remote_url TEXT;                  -- cloud: direct URL to open
ALTER TABLE files ADD COLUMN content_hash TEXT;                -- BLAKE3 for dedup detection
```

---

## UX Considerations

### Search results from remote devices
```
📄 invoice.pdf                    ← local file (instant open)
📄 q4-report.pdf    📱 Desktop   ← remote device (P2P fetch on click)
📄 budget.xlsx      ☁️ GDrive    ← cloud (opens in browser)
📄 photo.jpg        ⚠️ Offline   ← device not connected
```

### Device pairing flow
1. Both devices on same network
2. Device A shows 6-digit code
3. Device B enters code
4. Iroh connection established, keys exchanged
5. Initial index sync begins (progress bar)

### Privacy messaging
- "Your files never leave your devices"
- "Only file names and metadata sync — not file contents"
- "All connections are end-to-end encrypted"
- "No cloud servers — direct device-to-device"

---

## What NOT to Build (Leave for Spacedrive)

- Full virtual distributed filesystem (VDFS)
- Content-addressed dedup across devices
- File versioning / history
- Distributed file operations (move/rename across devices)
- Mobile app (phone as a source)
- Conflict resolution for file edits
- Collaborative features

findr's moat is **search quality**, not filesystem abstraction. Index everything, search everything, open from source. Don't try to replace Finder/Explorer for file management across devices.

---

## Roadmap Context

- **v1** (current): Desktop search app (Tauri + hotkey overlay)
- **v2**: File browser UI, polish, cloud settings
- **v3**: This document — distributed search
  - v3.0: LAN device sync (Tier 1)
  - v3.1: Internet device sync (Tier 2)
  - v3.2: Google Drive integration (Tier 3)
  - v3.3: Dropbox + OneDrive

---

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `iroh` | P2P networking (QUIC, NAT traversal, relay) |
| `blake3` | Content hashing for dedup detection |
| `chacha20poly1305` | File transfer encryption |
| `oauth2` | OAuth flows for cloud providers |
| `opendal` | Unified cloud storage abstraction (optional — or use provider SDKs directly) |
