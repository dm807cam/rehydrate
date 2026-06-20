# File-layout refactor — splitting the monolithic files

**Status:** complete. `cargo check --workspace --tests` clean,
`cargo test --workspace` 179 passed / 0 failed, `tsc --noEmit` clean,
`npm run build` produces a byte-identical CSS bundle and a same-size
JS bundle.

**Scope:** seven independent file-splitting commits. No public API
changes, no behaviour changes, no logic rewrites — every refactor here
moves code, it does not modify it. The goal was navigability: six
files had grown past 1000 lines and were hard to read, hard to review,
and hard to find anything in. After the split, the largest non-test
file is 689 lines and the median is around 230.

---

## 1. Summary

| # | Area | File(s) | Lines before | Files after | Largest after |
|---|---|---|---:|---:|---:|
| 1 | `rehydrate-app` Tauri IPC | `commands.rs` | 2268 | 10 in `commands/` | 486 |
| 2 | `rehydrate-app` OCR IPC  | `ocr_commands.rs` | 1259 | 5 in `ocr_commands/` | 532 |
| 3 | `rehydrate-device` SSH transport | `ssh.rs` | 1450 | 4 in `ssh/` | 689 |
| 4 | `rehydrate-render` PDF rendering | `lib.rs` | 1041 | 6 (`lib`, `dims`, `strokes`, `rm_to_pdf`, `image_to_pdf`, `overlay`) | 429 |
| 5 | `rehydrate-core` library facade | `library.rs` | 4594 | 10 in `library/` (incl. 1617-line test module) | 641 non-test, 1617 test |
| 6 | UI styles | `styles.css` | 2782 | 19 in `styles/` + thin entry | 494 |
| 7 | UI shell | `App.tsx` | 3685 | 2387 (App.tsx) + 1345 (`components/AppSubviews.tsx`) | 2387 |

`62 files changed, 15247 insertions(+), 14643 deletions(-)`. The
extra ~600 inserted lines are module-declaration boilerplate,
per-file `use` headers, and the new doc comments on each module
file — no production logic was added or removed.

---

## 2. What didn't change

- **No public API was touched.** Every Tauri command keeps the same
  name and signature. Every crate's `pub use` re-exports resolve at
  the same path. External callers (the UI, the integration tests, the
  shipped binary) are unmodified.
- **No `lib.rs` was edited at the registration level.** The Tauri
  `invoke_handler![…]` list still references commands as
  `commands::open_library`, etc. The split uses glob `pub use` so the
  short paths still resolve.
- **No tests were rewritten.** The big `library.rs` `#[cfg(test)] mod
  tests` block moved verbatim into `library/tests.rs`; the inner
  `mod tests { … }` wrapper was unwrapped and the body de-indented,
  which is the only edit it received.
- **The `Library` struct was not broken apart.** It still owns the
  single `Mutex<Db>` and `BlobStore`. The split uses Rust's
  multi-`impl Library { … }` block support across files; no methods
  were promoted to free functions, and no traits were introduced.

---

## 3. Why

Six files had grown past the point of comfortable navigation. The
practical symptoms:

- Finding the right `Library` method required scrolling through 4594
  lines of intertwined SQL, transaction guards, archive lifecycle, and
  GC. The two `impl Library` blocks (one for `open`, one for
  everything else) hinted at a long-deferred split.
- `App.tsx` mixed the App shell with eight sub-components and six
  pure helpers in one file. The sub-components were already
  self-contained (no closures over `App()` state) — the split was
  free.
- `commands.rs` and `ocr_commands.rs` had `// ---------- X ----------`
  banner comments that explicitly marked the seams.
- The crate-level architecture is genuinely good — clean layering,
  proper port/adapter seam for the device, public-API discipline. The
  pain was inside the files, not between them. Splitting the files
  without redesigning the modules is exactly the change that paid
  back the most for the least risk.

---

## 4. Commit walkthrough

### 4.1 `commands.rs` → `commands/` (commit `424d137`)

The 2268-line file mixed library lifecycle, import, document CRUD,
folder ops, export staging, device connection, sync orchestration,
and miscellaneous shims. The existing `// ---------- Library
----------` / `// ---------- Device ----------` / `// ---------- Sync
----------` banners defined the seams.

Split into ten files under `crates/rehydrate-app/src/commands/`:

```
commands/
├── mod.rs        — module decls + glob `pub use` re-exports
├── library.rs    — open/probe/pick/summary/gc/verify + allowlist tests
├── documents.rs  — list/rename/move/archive/history/open/thumbnail
├── folders.rs    — list/create/rename/reorder/delete/revert
├── import.rs     — file-picker + drop import + size-cap tests
├── export.rs     — drag-staging + version export + annotation pairs
├── device.rs     — state/connect/disconnect/host-key + reachability
├── sync.rs       — pull/push/two-way + progress forwarder
├── logs.rs       — get_recent_logs + reveal_log_dir
├── misc.rs       — ping/app_version/cancel/open_support_url
└── util.rs       — sanitize + thumbnail_fallback_pdf (shared)
```

**One subtlety worth noting**: `#[tauri::command]` generates
`__cmd__<name>` helper items that `tauri::generate_handler!` looks for
at the re-exported path. A selective `pub use commands::library::{a,
b};` brings the functions but loses the macro shim, breaking the
command registration. The fix is to use glob re-exports
(`pub use library::*;`). The `mod.rs` doc comment records this so the
next person to touch the file doesn't have to rediscover it.

`pdf_annotation_pairs` is `pub(crate)` and used by the top-level
`src/export.rs`; it gets a dedicated `pub(crate) use
export::pdf_annotation_pairs;` re-export in `mod.rs` because glob
re-exports can't carry `pub(crate)` visibility.

### 4.2 `ocr_commands.rs` → `ocr_commands/` (commit `83e5208`)

The 1259-line OCR file was already split out from `commands.rs` at
some prior point, but had itself grown into four banner-delimited
sections. Now lives in `crates/rehydrate-app/src/ocr_commands/`:

```
ocr_commands/
├── mod.rs        — module decls + shared TRANSCRIPT_PATH constant
├── ollama.rs     — config / ping / status / curated models /
│                   reachability cache / default model
│                   + ollama_url_tests + reachability_cache_tests
├── transcribe.rs — transcribe_document / get_transcript /
│                   list_documents_needing_ocr + frontmatter parsers
├── export_md.rs  — export_transcript (txt + md)
└── publish.rs    — Ghost / WordPress credentials + publish_transcript
                    + open_publish_url + ping_publish_target
```

`strip_frontmatter` is `pub(super)` from `transcribe.rs` and shared
with `export_md.rs` and `publish.rs` — kept there because it lives
naturally next to `parse_frontmatter`, the inverse operation.

### 4.3 `ssh.rs` → `ssh/` (commit `c34f62e`)

The 1450-line SSH file mixed the russh client handler (TOFU host-key
flow), SSH connection construction, SFTP timeout wrappers + error
mapping, the `SshDevice` struct + its non-trait helpers, the
`impl Device` trait body, and the fetch/reap subtree walkers.

```
crates/rehydrate-device/src/ssh/
├── mod.rs         — SshConfig, SshDevice + Inner, connect, exec,
│                    stage/commit/discard, purge_device_trash,
│                    is_reachable, public constants
├── handler.rs     — ClientHandler + HostKeyOutcome + impl Handler
├── io.rs          — timeout wrappers (with_timeout / with_body_timeout),
│                    MAX_* constants, staged_path / backup_path,
│                    open_sftp, sftp_err, read_path, read_capped,
│                    + the sftp_err classification tests
└── device_impl.rs — impl Device for SshDevice + fetch_subtree*,
                     reap_extras, reap_subtree
```

**Visibility detail:** `Inner.handle` is module-private because only
`SshDevice::exec` (in `mod.rs`) ever opens new SSH channels; every
cross-file operation goes through `Inner.sftp`. Keeping `handle`
private avoids leaking `ClientHandler` (which is `pub(super)`) at a
wider visibility than its type allows.

External call sites (`rehydrate-app::commands::device`,
`rehydrate-app::commands::sync`, `rehydrate-app::state`,
the integration tests) still resolve
`rehydrate_device::ssh::{SshDevice, SshConfig, is_reachable,
DEFAULT_HOST, DEFAULT_PORT, DEFAULT_USER, XOCHITL_DIR}` at the same
paths.

### 4.4 `rehydrate-render/lib.rs` → six files (commit `adc994d`)

The 1041-line file held three distinct PDF pipelines — fresh-PDF
generation from `.rm` strokes, PNG-thumbnail stitching, and
annotation overlay on an existing PDF — plus the shared
v6-scene-walk + per-stroke width / colour helpers both stroke
pipelines used.

```
crates/rehydrate-render/src/
├── lib.rs           — public API + PREVIEW_/EXPORT_LAYOUT_VERSION
│                      constants + cache-busting rationale
├── dims.rs          — A4 + RM canvas constants + mm_to_pt
├── strokes.rs       — v6 scene walk (collect_v6_renderables, visibility),
│                      width_pt_for, stroke_color_rgb, pen_color_rgb,
│                      mean — shared by rm_to_pdf and overlay
├── rm_to_pdf.rs     — build_pdf_from_rm_files + render_rm_to_ops +
│                      emit_stroke_segment + emit_chunked_strokes
├── image_to_pdf.rs  — build_pdf_from_pngs + build_pdf_from_image_bytes
└── overlay.rs       — overlay_annotations_on_pdf + lopdf helpers
```

The cache-busting constants (`PREVIEW_LAYOUT_VERSION`,
`EXPORT_LAYOUT_VERSION`) stay in `lib.rs` so the rule "if you change
the renderer's visual output, bump the version" remains a single-file
decision — the doc-comment rationale already in the original file
moves with them.

### 4.5 `library.rs` → `library/` (commit `d777db7`)

The big one. `Library` is the central facade exposing 49 methods
covering blob storage, version log, document CRUD, folder lifecycle,
archive lifecycle, the device-deletion queue, derived artefacts,
import, restore, garbage collection, verify, and reconstruct. The
original file held all of them — plus the public types they return,
plus 1617 lines of tests — at 4594 lines total.

Splitting `Library` into multiple structs was not on the table: every
method needs the same `Mutex<Db>` and `BlobStore`, and threading those
through free functions or helper types would have meant a real
redesign for no real win. Instead, the file is split using Rust's
ability to have **multiple `impl Library { … }` blocks across
files**:

```
crates/rehydrate-core/src/library/
├── mod.rs         — types, struct Library, open + validate + stamp
│                    helper, paths/blobs/probe_path accessors,
│                    `pub use maintenance::ReconstructOptions`
├── blobs.rs       — put_blob / put_blob_from_reader / has_blob /
│                    read_blob
├── versions.rs    — record_version + the transactional core
│                    `record_version_in_tx`, get_history, get_version,
│                    version_count, restore_version, set_version_note,
│                    previously_synced_ids
├── documents.rs   — list_documents, `record_metadata_change`
│                    (the pull-vs-local-edit conflict guard),
│                    rename / move, last_seen + mtime hint,
│                    is_archived, list_pushable_documents
├── folders.rs     — folder CRUD + push-queue helpers
├── archive.rs     — archive / unarchive / purge + list_archived +
│                    device-deletion queue
├── import.rs      — import_file + finalize_import rollback path
├── derived.rs     — record / read derived artefact (transcripts)
├── maintenance.rs — garbage_collect / verify / reconstruct /
│                    revert_unpushed_changes + walk_blobs +
│                    prune_empty_dirs + ReconstructOptions
└── tests.rs       — the 1617-line in-crate test suite, with the
                     outer `mod tests { … }` unwrapped (the file IS
                     the tests module via `mod tests;` in mod.rs)
```

**Visibility:** three internal helpers cross module boundaries and
needed promotion to `pub(super)`:
- `record_version_in_tx` (defined in `versions.rs`, called from
  `documents.rs`'s restore path)
- `record_metadata_change` (defined in `documents.rs`, called from
  `archive.rs`, `import.rs`, `derived.rs`)
- `finalize_import` (defined in `import.rs`, exercised by tests)

Everything else stays private to its file.

**`PostAction` enum** stays in `mod.rs` because it's the parameter
type for `record_metadata_change` and is used by callers in three
different sibling modules.

### 4.6 `styles.css` → `styles/` (commit `efef28c`)

A single 2782-line global stylesheet with 35 `/* === Section ===
*/` banner comments. Split into 19 per-concern files under
`ui/src/styles/`, with the original `styles.css` reduced to a thin
entry that `@import`s them in dependency order:

```
ui/src/styles/
├── tokens.css        — design tokens + dark mode + keyframes
├── shell.css         — base + app shell + toolbar + brand
├── controls.css      — buttons + pills + inputs
├── sidebar.css       — left source list + library card
├── content.css       — main content area + errors + document table
├── statusbar.css     — bottom statusbar
├── drawers.css       — modal + drawer (largest single concern, 494 lines)
├── history.css       — history list + logs
├── feedback.css      — badges + toaster + skeleton
├── popover.css       — popover + drag preview + row exit
├── row-search.css    — row pills + search bar
├── sync.css          — sync drawer hero + per-row progress
├── cheatsheet.css    — keyboard hint
├── thumbs.css        — thumbnails
├── select.css        — select mode + view-switch
├── grid.css          — grid view + quicklook
├── onboarding.css    — selection summary + onboarding pager
├── timeline.css      — version history timeline + cheatsheet overlay
└── palette.css       — command palette (⌘K)
```

Order in `styles.css` matters: `tokens.css` declares the CSS
variables every later file references and the keyframes
`feedback.css` / `drawers.css` / `sync.css` use. The entry comment
calls this out so a future refactor doesn't accidentally reorder them.

Vite resolves `@import` at build time, so the production bundle is
byte-identical to before (same 43.78 kB, same content hash). No
runtime cost.

The `import "./styles.css"` line in `main.tsx` is unchanged — the
file at that path now just delegates.

### 4.7 `App.tsx` → `App.tsx` + `components/AppSubviews.tsx` (commit `bf4b037`)

`App.tsx` was 3685 lines: a 2305-line `App()` function followed by
1295 lines of sub-components and pure helpers. None of the
sub-components depended on `App()`'s state — every input arrived via
props.

Moved everything from line 2390 onward into
`ui/src/components/AppSubviews.tsx` (1345 lines, of which ~50 are the
new file's imports + doc comment). Nine symbols are now exported and
imported back into `App.tsx`:

```
export { WelcomeEmpty, SidebarItem, FolderTree,
         DocumentList, DocumentGrid, ArchiveList,
         DocumentListSkeleton,
         prettyType, formatBytes }
```

File-private inside `AppSubviews.tsx`: `prettyDate`, `sortDocuments`,
`loadSortPref`, `buildFolderTree`, `FolderTreeNode`, `FolderReorder`,
`TypeIcon`, `FolderRow`. These are only consumed by other
sub-components in the same file.

`App.tsx` imports were trimmed to what the App shell actually uses
(several React types, several drag helpers, two `views` helpers, and
the `Skeleton`/`Thumbnail` components all moved with the
sub-components and were dropped from `App.tsx`).

**What was deliberately left for follow-up**: the `App()` function
body itself is still 2305 lines. Pulling feature hooks
(`useImport`, `useExport`, `useFolderOps`, `useShortcuts`,
`useCommandPalette`) out of `App()` reshuffles `useState`/`useEffect`
order and closure captures in ways that need browser-side
verification. The type-check / build pass is necessary but not
sufficient for that refactor; a stale-closure bug would type-check
and break at runtime. The work shape is the same as 4.7 (one new
hooks file per feature, props plumbed through), but its verification
step is "click around the actual app for ten minutes," which is the
gate left for the next pass.

---

## 5. Verification

All checks performed at the tip of each commit and again at HEAD:

| check | result |
|---|---|
| `cargo check --workspace --tests` | clean |
| `cargo test --workspace` | 179 passed, 0 failed, 2 ignored |
| `cd ui && npx tsc --noEmit` | clean |
| `cd ui && npm run build` (`tsc -b && vite build`) | clean; JS bundle 281.21 kB (unchanged), CSS bundle 43.78 kB (same content hash) |

`npm run lint` reports two pre-existing `role="checkbox"` a11y errors
in the document-list code that moved to `AppSubviews.tsx`. These were
present in `App.tsx` before this work — confirmed by stashing the
refactor and re-running lint — and are not introduced by it.

---

## 6. What was deliberately not done

- **No crate-boundary changes.** The nine-crate workspace split is
  load-bearing and not in scope.
- **No method-level redesign of `Library`.** The god-object concern
  is real, but breaking it apart would require threading the
  `Mutex<Db>` through helper structs or a service layer — a real
  redesign that would invalidate the test suite. Splitting the file
  while keeping the struct preserves the API and the tests.
- **No removal of duplication.** A `sanitize` function exists in
  `commands/util.rs` (filename-sanitiser for the dragged-PDF staging
  path) AND in `ocr_commands/export_md.rs` (filename-sanitiser for
  the exported transcript). They have different rules. Consolidating
  would conflate two separate contracts and is out of scope.
- **No error-mapping cleanup.** Each crate's `error` module was left
  untouched. The way `String` errors bubble up from
  `crate::util::err` is preserved everywhere.
- **No new tests.** Existing tests still cover the moved code at the
  same call-site shape, which is the only post-refactor invariant the
  splits could plausibly break.

---

## 7. Follow-ups worth doing

Listed in rough ROI order if the next pass picks any up:

1. **Extract feature hooks from `App()`** (the deferred half of 4.7).
   Goal: `App.tsx` shrinks from 2387 to ~300 lines, with
   `useImport`, `useExport`, `useFolderOps`, `useShortcuts`,
   `useCommandPalette` as siblings to the existing `useLibrary` /
   `useDeviceSync` / `useOcr` / `useSelection` hooks. Requires manual
   browser-side verification per hook extraction.
2. **`crates/rehydrate-sync/src/push.rs` (947 lines)** and
   **`crates/rehydrate-sync/src/execute.rs` (679 lines)**. Below the
   pain threshold today but the next candidates if the file-size
   guideline tightens.
3. **Public-surface snapshot tests.** With the splits in place, one
   test per crate that asserts the `pub` re-export list (a `cargo
   expand`-derived snapshot, or a hand-maintained list) would catch a
   future refactor accidentally moving an item to a different path.
4. **Fix the two `role="checkbox"` a11y warnings in
   `AppSubviews.tsx`**. Pre-existing; not introduced here but worth
   clearing so `npm run lint` becomes a meaningful gate again.
