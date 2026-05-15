# Changelog

All notable changes to reHydrate are recorded here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project follows [Semantic Versioning](https://semver.org/) once it
hits `1.0.0`. Pre-1.0 releases may break compatibility freely; the
library on-disk format is forward-stable from `0.9.0`.

## [Unreleased]

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
