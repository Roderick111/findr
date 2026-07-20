# Quick Look Crash — Debug Log

> Cmd+Y (Quick Look preview) intermittently crashes Raycast when used in the Findr extension.

## Status: OPEN — intermittent, not fully resolved

## Symptoms

- User presses Cmd+Y to preview a file in the search results list
- Raycast crashes (full process crash, not just extension error)
- Does NOT happen every time — intermittent, hard to reproduce deterministically
- More likely on **first preview** after results load
- Once Quick Look succeeds once in a session, subsequent previews tend to work

## Root Cause Analysis

### Confirmed fix (v2): dynamic `isShowingDetail` flip

**Problem:** `isShowingDetail` was toggling from `false` to `true` when search results arrived. This caused Raycast's native renderer to tear down and rebuild the `List` layout. If `Cmd+Y` fired during this transition, the `quickLook` prop wasn't registered with the native host yet — crash.

**Fix applied:** Set `isShowingDetail={true}` as a static value (always on). This eliminated the layout rebuild race condition.

```tsx
// Before (crashed)
<List isShowingDetail={hasResults}>

// After (current)
<List isShowingDetail={true}>
```

**Result:** Crash frequency reduced significantly but NOT eliminated.

### Remaining hypothesis: `quickLook` + `detail` on same `List.Item`

The current code uses both `quickLook` and `detail` props on the same `List.Item`:

```tsx
// raycast-extension/src/search.tsx:292-300
<List.Item
  icon={getResultIcon(result)}
  title={result.filename}
  accessories={result.is_dir ? [{ tag: "Folder" }] : []}
  quickLook={result.is_dir ? undefined : { path: result.path, name: result.filename }}
  detail={<ResultDetail result={result} />}
  actions={<ResultActions result={result} />}
/>
```

**Concern:** The Raycast API docs never show `quickLook` and `detail` used together on the same `List.Item`. They document them as separate features but don't address co-existence. The crash may be a framework bug where the native renderer conflicts when both a detail panel and a Quick Look overlay are active simultaneously.

### Additional factors

1. **File path validity** — If the file at `result.path` has been moved/deleted since indexing, Quick Look may crash instead of showing an error. No pre-check exists.
2. **Large files** — Quick Look rendering of very large PDFs or images could cause memory pressure in the Raycast process.
3. **Timing / race condition** — Quick Look fires via `Action.ToggleQuickLook` which is a Raycast-native action. If the selected `List.Item` is still mounting (React render not committed to native host), the path reference may be undefined.

## Current Implementation

### Quick Look prop (on `List.Item`)
```tsx
quickLook={result.is_dir ? undefined : { path: result.path, name: result.filename }}
```
- Directories: no Quick Look (undefined)
- Files: path + filename passed to native Quick Look

### Toggle action (in `ActionPanel`)
```tsx
<Action.ToggleQuickLook
  shortcut={Keyboard.Shortcut.Common.ToggleQuickLook}
/>
```
- Uses Raycast's built-in Cmd+Y shortcut constant

### Detail panel (always visible)
```tsx
<List isShowingDetail={true}>
  ...
  <List.Item detail={<ResultDetail result={result} />} />
```

## Things Tried

| # | Attempt | Result | Insight |
|---|---------|--------|---------|
| 1 | Removed `quickLook` prop entirely from `List.Item` | Quick Look stopped working (expected). No crashes. | Confirms the crash originates from the `quickLook` prop, not from `Action.ToggleQuickLook` or other code. |
| 2 | Restored `quickLook` prop, changed `isShowingDetail` from dynamic to static `{true}` | Crash frequency reduced from "almost always on first try" to "intermittent". | The dynamic `false→true` layout transition was the primary trigger. But not the only one — crashes still happen with static layout. |
| 3 | Conditional `quickLook` (undefined for dirs, set for files) | Correct behavior, no impact on crash for file results. | Crash is not related to dir entries. Happens on regular files. |
| 4 | Added 300ms debounce to search input | May indirectly help by reducing how often the result list re-renders before Cmd+Y. | Hard to measure direct impact on this bug. Stabilizes the list, which means `quickLook` props are more likely to be registered before user presses Cmd+Y. |
| 5 | Tested with `keepPreviousData: true` (stale results shown during new query) | No observable impact on crash. | The crash happens even on the first search when there are no previous results to keep. |
| 6 | **Minimal repro extension (30 lines, static data, no CLI)** | **Crashed on 2nd Cmd+Y attempt.** | **Definitive proof this is a Raycast framework bug.** No subprocess, no dynamic data, no async loading — just a static `List` with `isShowingDetail={true}` + `quickLook` + `detail` on same `List.Item`. Eliminates ALL findr-specific code as a factor. |

## Things NOT Yet Tried

| Idea | Rationale | Risk |
|------|-----------|------|
| Remove `detail` prop, keep only `quickLook` | Eliminates the dual-rendering theory. If crash stops, confirms framework conflict. | Loses the detail panel (core UX feature). Only viable as a diagnostic test, not a fix. |
| Remove `quickLook` prop, keep `Action.ToggleQuickLook` only | Some extensions use the action without the prop. Unclear if Raycast infers path from selected item. | Likely breaks Quick Look entirely — the action needs the prop to know what to preview. |
| Guard `quickLook` with file existence check | `existsSync(result.path)` before setting prop. Prevents crash from stale paths. | Adds sync I/O per list item render. Could lag on large result sets. Better: check only on selected item. |
| Delay Quick Look registration | Set `quickLook` prop only after a short delay or after `detail` has rendered. Use state to stage it. | Hacky. Might not help if the race is in the native layer, not React. |
| File a Raycast framework bug | Provide minimal repro: `List` with `isShowingDetail={true}` + `List.Item` with both `quickLook` and `detail`. | Depends on Raycast team responsiveness. |
| Move Quick Look to a separate `Action.Open` fallback | Replace Cmd+Y with opening the file in macOS Quick Look via `qlmanage -p` shell command. | Different UX (separate window, not inline). But guaranteed stable. |
| Use `Action.Push` to a detail view instead | Navigate to a full-screen detail view with file preview rendered as markdown/image. | Completely different UX pattern. No native Quick Look. |

## Key Insights

1. **The crash is in Raycast's native layer, not in our extension JS.** A React extension should never be able to crash the host process. The fact that setting a `quickLook` prop crashes Raycast means the bug is in their Swift/AppKit Quick Look integration, not in our code.

2. **`quickLook` + `detail` is an undocumented combination.** Every official Raycast example and docs page shows `quickLook` on a plain `List` (no detail panel) or `detail` without `quickLook`. We're the edge case. The framework likely doesn't test this combination.

3. **The crash is timing-dependent, not data-dependent.** Same file, same path, same data — crashes on first try, works on second. This rules out bad data (corrupt paths, missing files, encoding issues). It's a race between React committing props to the native host and the native Quick Look handler reading them.

4. **`isShowingDetail` transition was the biggest factor.** Going from static to dynamic eliminated most crashes. The remaining crashes are a smaller, harder-to-trigger race — possibly the initial mount of `List.Item` itself, before any re-render.

5. **Removing `quickLook` entirely = zero crashes.** This is the nuclear option and confirms the prop is the sole trigger. The `Action.ToggleQuickLook` action alone does nothing without the prop.

## Raycast API Notes

- **`List.Item.quickLook`**: `{ name?: string; path: PathLike }` — optional prop
- **`Action.ToggleQuickLook`**: toggles the Quick Look preview. Has NO `path` or `file` prop — reads from `List.Item.quickLook` on the selected item
- **`isShowingDetail`**: boolean on `List` component. Shows detail area on right side.
- **No documented interaction** between `quickLook` and `detail` / `isShowingDetail`. The docs show them independently but never together. This is the gap.
- **Changelog v1.30.0**: "Fixed a crash when attempting to switch Quick Look to full-screen mode" — confirms Quick Look has had native crashes before
- **Changelog v1.30.2**: "Updating the list isShowingDetail property has been fixed" — confirms `isShowingDetail` transitions had bugs
- **Changelog v1.67.0**: "Action.ToggleQuickLook now expands paths starting with ~" — confirms path handling bugs existed

## Relevant Files

- `raycast-extension/src/search.tsx` — Main search component, `ResultItem` (quickLook + detail), `List` (isShowingDetail)
- `PRD.md` — Bug documented in v2 Bugs Fixed section

## Recommendation

**Short term:** File a bug with Raycast. Include minimal repro:
```tsx
// Minimal repro: List with isShowingDetail + quickLook → intermittent crash on Cmd+Y
<List isShowingDetail={true}>
  <List.Item
    title="test.pdf"
    quickLook={{ path: "/Users/me/test.pdf" }}
    detail={<List.Item.Detail markdown="hello" />}
    actions={<ActionPanel><Action.ToggleQuickLook /></ActionPanel>}
  />
</List>
```

**Medium term:** Add file existence guard on selected item only (via `onSelectionChange` + `existsSync`). Won't fix the framework race but prevents crashes from stale index entries.

**If Raycast won't fix:** Replace `quickLook` + `Action.ToggleQuickLook` with a custom action that runs `qlmanage -p <path>`. Opens macOS Quick Look in a separate window instead of inline. Different UX but guaranteed stable. Implementation:
```tsx
<Action
  title="Quick Look"
  shortcut={Keyboard.Shortcut.Common.ToggleQuickLook}
  onAction={() => exec(`qlmanage -p "${result.path}" &>/dev/null &`)}
/>
```
