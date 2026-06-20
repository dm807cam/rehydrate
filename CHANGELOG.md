# Changelog

All notable changes to reHydrate are recorded here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project follows [Semantic Versioning](https://semver.org/) once it
hits `1.0.0`. Pre-1.0 releases may break compatibility freely; the
library on-disk format is forward-stable from `0.9.0`.

## [Unreleased]

## [1.1.1] — 2026-05-21

Feature-rich release on the v1 line. Adds bulk PDF export, image
and Word-document drag-import, a "My Files" view, column sorting,
a Tablet Trash view with a one-click purge, archive multi-select,
and a string of UX fixes (sticky selection bar, persistent folder
expand state, visible chevrons, refresh buttons). Bug-fix
highlights include the rMPP "No such file" sync skip, the
purged-doc resurrection on next pull, and the sub-folder drop
target that always landed in root. No on-disk format changes; the
existing library and version log are forward-compatible. The
macOS bundle is still Apple-Silicon-only, still unsigned, and
still requires the right-click → Open dance on first launch.

### Added

- **Bulk export to PDFs.** New "Export All" button in the
  toolbar, plus per-folder export from the folder kebab menu and
  per-selection export from the multi-select bar. Three Tauri
  commands back it (`export_as_pdfs`, `export_selected_as_pdfs`,
  `pick_export_directory`); a new `ExportOptionsDialog` lets the
  user toggle annotation overlay and the "keep local copies of
  deleted notes" backup mode. Progress streams live via
  `export:progress`.
- **Folder-mirroring output.** The exported tree mirrors the
  reHydrate folder hierarchy on disk — a notebook at
  `Research/Papers/foo` lands at
  `<target>/Research/Papers/foo.pdf`.
- **Incremental re-export.** A `.rehydrate-export.json` state
  file at the target root remembers
  `(version_id, path, include_annotations)` per document.
  Subsequent runs skip unchanged documents and clean up files
  for documents that were trashed, moved out of scope, or
  renamed — with an opt-out via the "keep deleted" checkbox
  (defaults to on, so the user's exported library is treated as
  a backup that doesn't surprise-delete files).
- **Annotation overlay on exported PDFs.** When
  `include_annotations` is on, `.rm` v6 strokes are composited
  onto the underlying PDF via a new `overlay_annotations_on_pdf`
  pass backed by `lopdf`.
- **"My Files" sidebar view.** A root-level view that shows only
  documents at the top of the library
  (`parent === null`), alongside the existing "All Documents" /
  kind filters.
- **Column sorting in the document list.** Click a column header
  to sort by Title / Type / Pages / Modified / Status; sort
  preference persists across launches.
- **Multi-select bulk actions.** Selecting multiple documents now
  exposes Export, Move, and Archive in the selection bar.
- **Empty Tablet Trash.** A new command-palette entry
  hard-deletes every document the tablet has soft-deleted
  (those with `deleted: true` in their `.metadata`) over SFTP,
  reclaiming device storage without touching the tablet.
- **Tablet Trash sidebar view.** A `"device_trash"` entry filters
  the document list to `parent === "trash"`, so the user can
  inspect what the tablet has soft-deleted before purging. An
  inline "Empty Tablet Trash…" button appears in the view header
  when the list is non-empty.
- **Image drag-import.** Drop a PNG / JPG / GIF / BMP / TIFF /
  WEBP onto the document list and reHydrate transparently
  converts it to a single-page PDF (via the existing
  `build_pdf_from_image_bytes` path) and imports it. Small
  images are upsampled with Lanczos3 so they don't look blocky
  on the tablet.
- **Word-document drag-import.** Drop a `.docx`, `.doc`, `.odt`,
  or `.rtf` and reHydrate calls a local LibreOffice
  (`/Applications/LibreOffice.app`, Homebrew, or PATH) to convert
  to PDF before importing. A clear error toast fires if
  LibreOffice isn't installed.
- **Archive multi-select.** The Archive page now supports
  checkbox selection with shift+click range, a select-all header
  checkbox with an indeterminate state, and a toolbar showing
  "N of M selected" plus **Restore (N)** and
  **Delete forever (N)** buttons. Per-row single-item actions
  moved to the kebab menu.
- **Sticky selection bar.** The multi-select action bar
  (Clear / Move / Export / Archive) now pins to the top of the
  content pane while the user scrolls a long document list.
  Column headers reposition themselves automatically via a
  `ResizeObserver`-driven CSS variable so they don't collide
  with the pinned bar.
- **Select-all in document list.** In select mode, the Title
  column header gains a checkbox (empty / dash / checked
  depending on selection state). Clicking it selects every
  filtered document. The selection bar also exposes a "Select
  all" button.
- **Refresh buttons for Archive and Tablet Trash.** A ↻ icon in
  the content header pulls a fresh list without forcing a full
  sync.
- **Persistent folder expand state.** The sidebar folder tree
  now remembers which folders are open across app restarts
  (stored in `localStorage` under `rh.expanded`).
- **Visible folder expand chevron.** Replaced the small `▸` / `•`
  glyphs with a 10×10 SVG chevron that rotates 90° when open.
  Leaf folders render no indicator at all. 150 ms ease
  transition for continuous feedback.
- **Sync activity in the log.** Both pull and push phases now
  emit `tracing::info!` events (started, per-document,
  complete). The log was previously silent during sync.
- **`Tool::Shader` recognised.** A new `.rm` v6 tool variant
  (firmware code `0x17`) is parsed and rendered identically to
  the Highlighter.

### Fixed

- **Sync (rMPP)**: stop skipping whole documents when the
  optional per-document directory `xochitl/<uuid>/` is absent on
  the device. The SFTP error classifier was matching the Debug
  spelling `"NoSuchFile"` against an error whose Display is
  `"No such file"`, so a benign "directory does not exist" was
  demoted to `DeviceError::Other` and propagated up to the pull
  loop as a document-level failure. The classifier now matches
  the typed `russh_sftp::protocol::StatusCode::NoSuchFile`
  variant, with a case-insensitive substring fallback, and the
  optional-directory probe in `fetch_document_tree` routes
  through the same classifier so the two sites can't drift again
  (#63).
- **Sync (rMPP)**: the sibling probe for the optional
  `xochitl/<uuid>.thumbnails` directory used to swallow *every*
  error — a transient SFTP failure would silently drop the
  thumbnails subtree and the document was still recorded as a
  successful sync. It now routes through the same classifier as
  the `xochitl/<uuid>/` probe: `NotFound` is benign, anything
  else propagates.
- **Purged documents re-appearing on the next pull.** If the
  user purged a document while the tablet was unreachable, the
  immediate SFTP delete failed and the local `sync_state` row
  was deleted; the next pull saw the document still on the
  device, classified it as `New`, and re-downloaded what the
  user had just permanently deleted. A new
  `device_deletion_queue` SQLite tombstone table (migration
  `0008_device_deletion_queue.sql`) persists purge intent across
  sessions, and the pull engine consumes it before classifying
  each device entry.
- **Dropped files always importing to root** regardless of which
  folder row was the drop target. Two independent bugs: the
  `FolderRow.onDrop` early-`return`ed without
  `e.preventDefault()`, so the event bubbled to the root
  handler; and `import_file` hardcoded `"parent": ""` in the
  metadata JSON, ignoring any folder id passed by the caller.
  Both fixed — the import now lands in the dropped-on folder.
- **Sync button disabled when only folder operations were
  pending.** `PushPlan.items` only tracked document pushes;
  folder-only changes (create, rename, delete) made the "Start
  sync" button look like there was nothing to do. `PushPlan`
  gained a `pending_folders: usize` field, populated from
  `Library::list_pending_folder_pushes`, and the UI gates on
  `totalActive > 0 || pending_folders > 0`.
- **Empty Tablet Trash didn't refresh the UI.** The IPC command
  succeeded but the renderer never called `refreshLibrary()`,
  so the Tablet Trash view kept showing the entries that had
  just been deleted.
- **Double extension on exported PDFs.** Documents whose
  `visible_name` already ended in `.pdf` or `.epub` produced
  `name.pdf.pdf` filenames on export. Fixed.
- **Shift+click range selection** used the unsorted
  `filteredDocs` array, so the selected range didn't match the
  user's displayed sort order. Fixed by ranging over the sorted
  view.
- **Shift+clicking a row highlighted surrounding text** because
  the browser's default text-selection kicked in.
  `e.preventDefault()` in the `<tr>` onClick and the
  selection-bar buttons; `user-select: none` on
  `.selection-bar`.
- **Log filename extension.** The rolling log appender produced
  `rehydrate.log.YYYY-MM-DD`, which macOS treated as having
  extension `.YYYY-MM-DD` and refused to open as text. Now
  produces `rehydrate.YYYY-MM-DD.log`.
- **`RUST_LOG=rehydrate=debug` was a no-op** because no crate is
  named `rehydrate` — they are all `rehydrate_sync`,
  `rehydrate_app`, etc. The default filter now enumerates every
  crate explicitly so debug events actually reach the log file.

### Changed (internal)

Six oversized files were split into focused per-concern modules.
Public APIs, Tauri command names, and on-disk artefacts are
unchanged. Each split is its own commit so individual pieces can
be reviewed in isolation. Full rationale and file-layout tables
live in [`docs/refactor-notes.md`](docs/refactor-notes.md).

- `rehydrate-app::commands` — 2268-line `commands.rs` split into
  ten files under `commands/` along the existing
  `// ---------- X ----------` banner seams (library, documents,
  folders, import, export, device, sync, logs, misc, util).
  Largest after: 486 lines.
- `rehydrate-app::ocr_commands` — 1259-line `ocr_commands.rs`
  split into `ollama.rs` (config + reachability),
  `transcribe.rs` (transcribe + transcript reads + OCR-candidates
  listing), `export_md.rs` (txt / md export), `publish.rs`
  (Ghost / WordPress).
- `rehydrate-device::ssh` — 1450-line `ssh.rs` split into
  `mod.rs` (struct + lifecycle + `is_reachable`), `handler.rs`
  (TOFU host-key flow), `io.rs` (timeout / SFTP helpers +
  classifier tests), `device_impl.rs` (`impl Device for
  SshDevice` + subtree walkers).
- `rehydrate-render` — 1041-line `lib.rs` split into `lib.rs`
  (public API + cache-busting constants), `dims.rs`,
  `strokes.rs` (v6 scene walk shared between PDF generation and
  overlay), `rm_to_pdf.rs`, `image_to_pdf.rs`, `overlay.rs`.
- `rehydrate-core::library` — 4594-line `library.rs` split into
  `library/` using nine multi-`impl Library` block files
  (`blobs`, `versions`, `documents`, `folders`, `archive`,
  `import`, `derived`, `maintenance`, plus `mod.rs` and
  `tests.rs`). The `Library` struct itself is unchanged.
- `ui/src/styles.css` — 2782-line stylesheet split into 19
  per-concern files under `styles/`, with the original
  `styles.css` reduced to an `@import` entry. Vite resolves at
  build time so the production CSS bundle is byte-identical.
- `ui/src/App.tsx` — 3685 lines reduced to 2387 by moving the
  eight stateless sub-components (`WelcomeEmpty`, `SidebarItem`,
  `FolderTree`, `FolderRow`, `DocumentList`, `DocumentGrid`,
  `ArchiveList`, `DocumentListSkeleton`, `TypeIcon`) and the
  pure helpers (`sortDocuments`, `loadSortPref`,
  `buildFolderTree`, `prettyType`, `prettyDate`, `formatBytes`)
  into `components/AppSubviews.tsx`. Hook extraction inside
  `App()` itself is deliberately left for a future pass — see
  the follow-ups section in `docs/refactor-notes.md`.

## [1.0.2] — 2026-05-17

Patch release on the v1.0 line. Fixes first-run macOS networking,
real Keychain persistence, and the new drag-out export path. No
on-disk format changes. The macOS bundle is still Apple-Silicon-only,
still unsigned, and still requires the right-click → Open dance on
first launch.

### Added

- **Export**: hold Option while dragging a document row or tile to
  drag a real file out of reHydrate into Finder, Desktop, Mail, or
  another native drop target. PDF and EPUB documents are copied out
  verbatim; notebooks render to a multi-page PDF via the same ink
  renderer used by Preview (#57).

### Fixed

- **macOS bundle**: ship `NSLocalNetworkUsageDescription` in
  `Info.plist`, so the OS prompts for Local Network access on
  first connection to the tablet at `10.11.99.1` instead of
  blocking the SSH socket silently. Affects macOS 15+ users
  (rM2 and Paper Pro alike) (#60, #61).
- **App (keychain)**: "Remember password" now actually persists the
  reMarkable SSH password across app restarts on macOS. The `keyring`
  v3 dependency was missing its `apple-native` backend feature, so the
  crate was silently compiling to an in-memory mock store: writes
  returned `Ok(())`, in-session reads worked, and the password
  disappeared on next launch with no warning surfaced. Enabling the
  feature routes credential I/O to the real macOS Keychain (#58, #59).
- **Export cache**: drag-out export staging now checks the content-
  keyed cache path before reading large PDF/EPUB blobs or rendering
  notebooks. Hover prefetch therefore stays cheap on cache hits, and
  the actual Option-drag can call into macOS' drag API inside the
  short user-gesture window.

### Credits

- Thanks to
  [u/shuusaku](https://www.reddit.com/r/RemarkableTablet/comments/1tccfh3/comment/om7ez6o/)
  for the Paper Pro report and follow-up root-cause notes that led
  directly to the macOS Local Network permission fix and the Keychain
  backend fix.

[1.0.2]: https://github.com/dm807cam/rehydrate/releases/tag/v1.0.2

## [1.0.1] — 2026-05-15

Patch release on the v1.0 line. Bug fixes, hardening, and dependency
bumps that landed against `main` since `v1.0.0`. No on-disk format
changes; no user-facing behavioural changes outside the fixes below.
The macOS bundle is still Apple-Silicon-only, still unsigned, and
still requires the right-click → Open dance on first launch.

### Fixed

- **Sync (push side)**: `delete_document_tree` failures now propagate
  to the sync result so a failed folder delete stays queued and is
  retried on the next push, rather than being silently swallowed
  (#23).
- **Sync (pull side)**: `record_version` on an archived document is
  rejected at the boundary, closing a race where a pull mid-archive
  could resurrect a soft-deleted doc (#31).
- **Device layer**: `put_document_tree` is now a true replace —
  stale tablet-side entries are removed when the library-side tree
  shrinks, so a folder rename + child-removal in one push no longer
  leaves orphan tablet entries (#22).
- **Device layer**: SFTP entry names containing path separators or
  parent references are rejected at the boundary instead of being
  treated as leaf names (#33).
- **Core (blob store)**: `import_file` failures now unlink the
  staged blob, so an interrupted import does not leak a partial
  file under the blob fanout (#39).
- **Core (blob store)**: blob-fanout ancestor directories are
  fsynced up to the blob root after a write, so a crash between
  rename and parent-dir fsync cannot leave a blob that exists on
  disk but is invisible after reboot (#30).
- **Core (library)**: `library.json` is written atomically via a
  tempfile + rename rather than truncated-in-place, so a crash mid-
  write cannot leave a zero-byte stamp file (#32).
- **rm-parser**: v3–v5 length-prefixed buffers cap their
  preallocation, closing a parser-DoS where a hostile `.rm` file
  could request gigabyte allocations from a 32-bit length field
  (#34).
- **rm-parser**: unknown `SceneItemType` subtypes are tolerated as
  forward-compat skips rather than hard parse errors, so newer
  tablet firmwares do not brick the import path (#35).
- **App (IPC)**: `open_library` is allowlisted against the picker,
  the recents list, and the default-library path, so a renderer
  compromise cannot ask the host to open an attacker-controlled
  directory as a library (#36).
- **App (import)**: dropped-file size is preflighted before the
  bytes are read into memory, so a 5 GiB drag-and-drop is rejected
  cheaply rather than after an OOM (#24).
- **UI**: a `refreshLibrary` request that arrives while another is
  in flight is now queued (one pending slot) instead of being
  dropped, so a sync-completion event during a manual refresh no
  longer leaves the library view stale (#38).
- **UI**: Tauri event listeners unbind cleanly when their owning
  component unmounts before the `listen()` promise resolves,
  closing a leak where a long-running listener kept a reference to
  a discarded component (#37).
- **UI**: three v1.0.0 UX dead-ends are unblocked (move-to-folder
  affordance, empty-state guidance, and one settings-tab focus bug)
  (PR #16).

### Changed

- **CI**: the supply-chain audit job (`cargo audit` + `cargo deny
  check`) is now a hard gate on `main`. A new advisory landing on a
  transitive dep fails the build instead of producing a yellow warn
  in the log (#19). The triage process is documented in
  `.cargo/audit.toml` and `deny.toml`.
- **Tests**: pull-side and push-side reconciliation now have
  coverage at the sync layer (#17). No production-code changes;
  these tests pin behaviour that was previously asserted only
  end-to-end.

### Dependencies

- `thiserror` 1.0.69 → 2.0.18 (#29)
- `sha2` 0.10.9 → 0.11.0 (#28)
- `imageproc` 0.25.1 → 0.26.2 (#27)
- `fs4` 0.9.1 → 1.1.0 (#26)
- `tokio` 1.52.2 → 1.52.3 (#15)
- `rusqlite` 0.32.1 → 0.39.0 (#14)
- `directories` 5.0.1 → 6.0.0 (#13)
- `actions/checkout` 4 → 6 (#10)
- `actions/setup-node` 4 → 6 (#9)
- `actions/upload-artifact` 4 → 7 (#7)
- `actions/attest-build-provenance` 2 → 4 (#8)
- `tauri-apps/tauri-action` 0.5.20 → 0.6.2 (#6)

### Release engineering

- `build.sh` now removes stale `rw.*.dmg` interstitials from
  `bundle/macos` before invoking the Tauri bundler, and asserts the
  expected `.dmg` exists and passes `hdiutil verify` before
  reporting success. Previously a `bundle_dmg.sh` failure could
  leave its temporary image inside the source folder, which then
  caused the next bundle to try copying its own growing tempfile
  into itself (#54).
- The release workflow now runs the same gates as CI
  (`cargo fmt --check`, `clippy -D warnings`, workspace tests,
  `cargo audit --deny warnings`, `cargo deny check`, UI typecheck
  + lint + build) as a preflight job before the Tauri build, and
  asserts that the tag version matches `Cargo.toml`,
  `tauri.conf.json`, `ui/package.json`, `ui/package-lock.json`,
  and a matching `CHANGELOG.md` section. The release fails
  loudly if any of those drift, instead of falling back to a
  generic body (#55).

[1.0.1]: https://github.com/dm807cam/rehydrate/releases/tag/v1.0.1

## [1.0.0] — 2026-05-11

First stable release. The goal of v1.0 was to ship the audit cleanups
the v0.9.x line accumulated and to land enough of the
"organise-your-library" UI for daily use against a reMarkable 2.

### Added

- **Folders, end-to-end.** Create folders and subfolders from the
  sidebar; drag-reorder rows in the sidebar (local-only ordering, not
  pushed to the device); move documents into folders by drag-and-drop,
  by a new "Move to folder…" item in the three-dot menus, or in bulk
  from the selection bar. The "Move to folder…" picker also has an
  inline "+ New folder here…" affordance so common workflows don't
  dead-end in cancel-create-retry.
- **Drag PDFs and EPUBs from Finder / Explorer onto the window** to
  import. A full-window overlay shows the drop zone; bytes travel
  over IPC into a tempfile and through the existing import path. Cap
  is 512 MiB per file.
- **Render typed text** in v6 notebook PDFs. Until now the renderer
  only emitted ink strokes; typed text on a page was silently
  dropped from the export. Now it lands as Helvetica at the recorded
  position.
- **Respect erased strokes and hidden layers** when rendering
  notebooks. A stroke the user erased on the tablet no longer
  reappears in the exported PDF; layers toggled off on the tablet
  are skipped.
- **Retry button on a failed sync**, plus a warning card for non-fatal
  sync issues (e.g. `xochitl restart` failed — files landed, the
  tablet's UI just needs a reboot to pick them up).
- **Toast warnings** for keyring write failures (Linux without
  secret-service) and legacy-format notebook fallbacks (v3 / v5
  `.rm` files that fall back to thumbnail previews).
- **OCR via a user-provided Ollama daemon.** "Convert to text…" on
  any document renders every `.rm` page to PNG, ships it to
  Ollama's `/api/generate`, and stores the transcript as a derived
  artefact attached to the document's current version. The
  Settings modal's *Ollama* tab lets the user point at a local or
  remote Ollama (default `http://localhost:11434`) and pick from
  the curated `qwen3.5:4b` (default, fast) / `qwen3.5:9b` (sharper
  at cursive + math) options, or supply a custom model tag.
  Qwen 3.5 is Ollama's current unified vision-language family
  (released ~one month before v1.0.0) and outperforms the older
  Qwen3-VL / Qwen2.5-VL lines on OCRBench (93.1%) and
  OmniDocBench1.5 (90.8%) — both directly relevant to the
  handwritten-notebook workload. Test Connection probes
  `/api/tags` and reports which models are pulled. Background
  progress is shown in a floating chip; the result lands in the
  Transcript drawer with Save-as-`.txt` / Save-as-`.md` actions.
- **Auto-OCR at startup.** Optional toggle in Settings → Ollama.
  When enabled, every notebook without an existing transcript is
  transcribed sequentially after the app opens. Silently skips
  when Ollama is unreachable (no nagging at launch); per-doc
  failures don't abort the sweep; the progress chip shows
  "(N of M)" batch progress and × cancels the whole queue. Off
  by default — opt-in keeps first-run users from unexpected
  network traffic.
- **Publish transcripts as drafts to Ghost or WordPress.** The
  Transcript drawer's "Publish to Ghost" / "Publish to WordPress"
  buttons convert the Markdown transcript to HTML and POST it
  to the configured CMS as a draft. Credentials live in the OS
  keychain; the *Publishing* tab in Settings handles entry, test
  connection, and forget. Every request routes through a
  host-pinned `RestrictedAgent` with redirects disabled, so a
  hijacked CMS endpoint can't redirect transcript content
  anywhere else.
- **Tabbed Settings modal.** New gear icon in the toolbar (and
  "Settings…" entry in the menu) opens a single modal with two
  tabs: Ollama (OCR) and Publishing. Failed OCR / publish actions
  auto-open the relevant tab with an explanatory banner instead
  of dead-ending the user on a raw error toast.
- `LICENSE-MIT` and `LICENSE-APACHE` at the repo root and a new
  `SECURITY.md` documenting the v1.0 threat model.

### Changed

- **Tauri webview `devtools` is disabled in release builds.** Dev
  builds still expose F12 / Cmd+Opt+I for local debugging; the
  release workflow builds with `--no-default-features` so end users
  can't open the inspector.
- **Manifest path validation** now rejects control characters,
  trailing dots/spaces, Windows reserved names (`CON`, `PRN`, etc.),
  and NTFS alternate-data-stream colons. The export-filename
  sanitiser applies the same rules so a doc titled `CON` exports as
  `doc-CON-…` instead of failing silently on Windows.
- **Document cache keys** now include the full content hash and the
  document UUID. The previous 12-char hash prefix was inside
  birthday-collision range; a malicious device could craft two
  documents that collide in the cache.
- **`update_last_seen_manifest` retries on transient SQLite errors**
  with backoff before propagating, so a momentary busy-lock doesn't
  leave the next push silently overwriting tablet-side edits.
- **Single xochitl restart per push session** instead of one per
  document. A multi-doc push no longer blanks the tablet UI N times.
  If the restart fails, the sync completes successfully but a
  warning surfaces in the UI.
- **GC clock-skew guard**: blobs whose filesystem mtime appears to
  be in the future relative to wall-clock now are kept rather than
  deleted, so an NTP backwards-jump can't sweep recent blobs.

### Fixed

- `import_dropped_file` is now size-capped at 512 MiB, matching the
  cap that already protected SFTP-side reads.
- `Library::reorder_folder` rejects cycles in a single transaction
  and the UI short-circuits descendant drops in the folder picker.

### Security

See `SECURITY.md` for the full v1.0 threat model. Summary: SSH host-
key verification remains TOFU-without-pinning (USB-cabled threat
model); credentials live in the OS keyring; logs are size-capped and
rotated; CSP locks `script-src` to `'self'`; no HTTP egress in any
business crate, enforced by an integration test.

## [0.9.1] — 2026-05-10

Hotfix release. The macOS bundles in `v0.9.0` are unlaunchable on
Apple Silicon: ship `v0.9.1` instead.

### Fixed

- **macOS bundle was structurally inconsistent and would not
  launch on Apple Silicon** (`reHydrate_0.9.0_aarch64.dmg`,
  `reHydrate_0.9.0_x64.dmg`). The Mach-O executable was
  linker-signed (mandatory on Apple Silicon — the linker stamps an
  ad-hoc signature on the binary) and that signature claimed the
  bundle had sealed resources. But Tauri's bundler with
  `signingIdentity: null` did not run a follow-up `codesign` on
  the bundle as a whole, so `Contents/_CodeSignature/CodeResources`
  was missing and `codesign -dv` reported `Sealed Resources=none`.
  The kernel rejected the signature mismatch at load time and
  macOS surfaced this as a misleading **"App is damaged and can't
  be opened"** error. Right-click → Open and `xattr -d
  com.apple.quarantine` did not help — the binary genuinely
  refused to load.

  Fix: set `signingIdentity: "-"` in `tauri.conf.json`. The `"-"`
  value tells Tauri (via `codesign`) to apply a real ad-hoc
  signature to the whole bundle, not just the binary. The result
  is a self-consistent ad-hoc bundle that passes the kernel's
  signature check at launch. Gatekeeper still warns on first run
  (no Developer ID), and the right-click → Open dance is still
  the documented workaround for that — but the app actually
  launches.

[0.9.1]: https://github.com/dm807cam/rehydrate/releases/tag/v0.9.1

## [0.9.0] — 2026-05-10

> **⚠️ Do not use the v0.9.0 macOS bundles.** They will not launch
> on Apple Silicon (and may misbehave on Intel). See `v0.9.1`
> above for the fix. Linux and Windows builds are unaffected.


First public release. Feature-complete for the sync + library use case
the project set out to solve; bundles ship unsigned, full code-signing
is deferred to `1.0.0`.

### Sync (USB)

- Pull from a reMarkable 2 over USB-SSH, into a content-addressed blob
  store with one stored copy per distinct file content. The same notebook
  template across 200 documents costs ~80 KB on disk.
- Push library-side edits back to the tablet: rename, move-between-folders,
  archive (soft-delete). Each push diffs against the device manifest so
  unchanged files transfer zero bytes.
- Two-way sync that runs pull then push under one transaction, with a
  pre-flight plan that shows the user exactly what will move before
  they hit `Start`.
- Live per-document progress in the sync drawer; the toolbar `StatusPill`
  shows a `Syncing…` chip while a sync is in flight, so closing the
  drawer doesn't lose visibility.

### Library

- Content-addressed blob store: every distinct file is stored once by
  SHA-256, so duplicates across documents and versions are free.
- Full version history: every change to every document is captured as
  a new version with a parent pointer, optional human note, byte-level
  diff against the parent, and one-click restore.
- Archive with restore: archived documents disappear from the library
  view and from the tablet on next sync, but every prior version is
  retained; restore brings them back.
- Permanent purge from archive: drops every saved version of a document
  (gated by an explicit confirm).
- Folder tree mirroring the tablet's hierarchy. Drag-and-drop moves
  documents between folders or into the archive; spring-loaded folders
  expand under a hovering drag.
- Multi-library support: switch between per-tablet libraries from a
  toolbar dropdown without restarting the app. The recents list stays
  sticky across launches.

### Import + export

- Import any PDF or EPUB via a server-side native picker (renderer
  cannot supply arbitrary paths; audit fix H6). The imported file
  uploads to the tablet on the next sync.
- Export any version of any document back to disk via the history
  drawer, again through a server-side picker.

### Health

- Library health check (`Verify`) reports manifest integrity, missing
  blobs, and orphan blobs at a glance.
- Garbage-collect orphan blobs (`Clean up unused files`) with a
  confirmation dialog explaining the operation.

### UX

- Onboarding (plug in tablet → open library) on first launch; auto-
  advances when the tablet is detected.
- Quick Look popover (Space) with thumbnail and metadata. Follows
  ↑/↓ navigation while open, mirroring macOS Finder.
- Command palette (⌘K) with fuzzy match across actions, views, folders,
  and documents.
- Search-this-view (⌘F) with live filter.
- Drag-and-drop with multi-selection batch support; custom drag-image
  showing the count.
- Keyboard navigation: arrow keys for the focus cursor, Enter to open,
  F2 to rename, ⌘⌫ to bulk-archive, Esc to close any dialog/drawer.
- View persistence: the last view (`All Documents` / `Pending Sync` /
  `Notebooks` / etc.) and viewMode (list vs. grid) survive across
  launches.
- Per-document menu in both list and grid views (Open in viewer,
  Rename…, Show history, Move to Archive).
- Toast system with optional `Undo` action on archive + move.
- Cheatsheet (`?`) listing every keyboard shortcut.

### Privacy

- No telemetry, no analytics, no auto-update check on launch — the app
  makes zero network calls until the user takes an explicit action.
- The only network destination the app contacts is the tablet at
  `10.11.99.1` over USB-SSH (russh + russh-sftp).
- Tablet password persists in the OS keychain (Keychain on macOS,
  Secret Service on Linux, Credential Manager on Windows).
- Locked Tauri capability surface: no `tauri-plugin-http`, no
  `tauri-plugin-shell` for arbitrary commands, content-security-policy
  set to `default-src 'self'; img-src 'self' data: blob:; …`.
- An automated CI check (`tests-integration/tests/no_egress.rs`)
  asserts that no Rust crate in the workspace transitively depends on
  any general-purpose HTTP client (`reqwest`, `ureq`, `isahc`, `surf`,
  `hyper-tls`, `hyper-rustls`, `awc`). The privacy invariant is
  enforced in code, not just documented.

### Engineering

- 38 workspace tests (29 in `rehydrate-core`, 3 in `rehydrate-sync`,
  2 in `rehydrate-device`, 4 integration including the egress
  invariants).
- CI runs `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, the workspace test suite, and a
  cross-platform UI build on every push.
- Release workflow fans out to four runners (macOS Intel, macOS Apple
  Silicon, Linux, Windows) and attaches `.dmg` / `.AppImage` / `.deb`
  / `.msi` bundles to a draft GitHub Release.

### Known gaps

- **Bundles are unsigned.** macOS users see a Gatekeeper warning on
  first launch (right-click → Open clears it); Windows users see
  SmartScreen ("Run anyway"). Code-signing config in `tauri.conf.json`
  is documented in `PACKAGING.md` and wired up to take effect once
  signing secrets are added to the release workflow. Targeted for
  `v1.0.0`.
- **No auto-update.** The design says auto-update should be opt-in;
  not implemented for this release.
- **`rehydrate-app` IPC layer has no test suite.** Other crates are
  well-covered; the IPC commands are exercised by manual smoke runs.
  Adding an IPC test harness is on the `1.0.0` punch list.

[0.9.0]: https://github.com/dm807cam/rehydrate/releases/tag/v0.9.0
