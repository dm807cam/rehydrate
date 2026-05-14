# reHydrate

[![CI](https://github.com/dm807cam/rehydrate/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/dm807cam/rehydrate/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/dm807cam/rehydrate?display_name=tag&sort=semver)](https://github.com/dm807cam/rehydrate/releases/latest)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#licence)
[![macOS: Apple Silicon](https://img.shields.io/badge/macOS-Apple%20Silicon-black?logo=apple)](#compatibility)

A privacy-respecting desktop app for managing documents on a reMarkable 2 over USB. No cloud, no telemetry; the app talks only to a tablet plugged into your computer.

The library lives in a single self-contained directory you control: every distinct file is stored once by content hash, every change to every document is captured as a new version, and any past version can be restored to the device or exported to disk.

## Status

`v1.0.0` — first stable release. macOS (Apple Silicon) only. Bundles
ship **unsigned**; see [Installing](#installing) below for the
Gatekeeper right-click dance on first launch. Working features:

- Two-way sync (pull + push) over USB-SSH, with progress streamed live.
- Content-addressed blob store, full version history, restore-any-version.
- Archive with restore (soft-delete; the tablet only sees the deletion
  on the next sync).
- Import PDFs and EPUBs from disk; they upload to the tablet on the
  next sync.
- Folder organisation, drag-and-drop moves, multi-select with
  Cmd-click + Shift-click + a selection toolbar.
- Library health (`Verify`) and orphan-blob cleanup (`Clean up unused
  files`) with confirmation.
- Quick Look (Space), command palette (⌘K), keyboard navigation,
  search.
- Multi-library support: switch between per-tablet libraries from the
  toolbar without restarting.
- **Notebook OCR via Ollama.** Convert a handwritten notebook to text
  with a vision-language model running on your own machine (or a
  GPU box on your LAN). No cloud OCR; no transcripts leave your
  network unless you explicitly publish them.
- **Publish transcripts to Ghost or WordPress** as drafts. The
  credential entry, host-pinned HTTP client, and explicit user
  trigger mean transcripts never reach anywhere you didn't
  configure.

Architecture notes are in [`docs/architecture.md`](docs/architecture.md);
release notes are in `CHANGELOG.md`; threat model and security
posture in `SECURITY.md`.

## Installing

reHydrate ships pre-built bundles on the [GitHub Releases][releases]
page for **macOS (Apple Silicon, 11.0+)**. Intel Macs, Windows, and
Linux are not currently shipped as binaries — building from source
works on all three; see [Building](#building) below.

v1.0 bundles are **unsigned** — code-signing is on the roadmap for
v1.1. Until then, macOS warns on first launch:

1. Download `reHydrate_<version>_aarch64.dmg` from the latest release.
2. (Optional but recommended) Verify the download against
   `SHA256SUMS`, which is also attached to the release:
   `shasum -a 256 reHydrate_*.dmg` should match the corresponding
   line in `SHA256SUMS`.
3. **Right-click the `.dmg`** (or Control-click) and choose **Open**.
   Don't double-click — Gatekeeper will reject the unsigned bundle.
4. Drag reHydrate to `/Applications`.
5. The first time you launch the app, **right-click reHydrate in
   `/Applications`** and choose **Open**. Confirm the "unidentified
   developer" warning. After this one-time bypass, normal
   double-click works.

## Compatibility

- **Host:** macOS 11.0+ on Apple Silicon (M1 / M2 / M3 / M4).
  Intel Macs are not shipped as binaries; building from source
  on Intel works but is not part of CI. Linux / Windows builds
  from source — no bundles.
- **Tablet:** the reMarkable 2 on stock firmware (xochitl) is the
  only tablet reHydrate has been directly tested against — v1.0
  against firmware 3.11 → 3.20. The reMarkable Paper Pro and
  reMarkable 1 are untested: the Paper Pro's tablet-side schema
  introduced new files reHydrate doesn't yet parse, and the rM 1
  has differing protocol semantics, so both will likely need work
  before they function end-to-end.
- **OCR (optional):** [Ollama][ollama] 0.6+ on the same machine or
  a reachable LAN host, with at least one vision-language model
  pulled (default `qwen3.5:4b`, ~3.4 GB on disk). Without Ollama
  every other feature still works; only "Convert to text…" is
  disabled.

If you're on a model / firmware combination not in the list above,
sync will most likely work but is not part of the test matrix —
please file an issue from the in-app About dialog if you hit a
problem so the matrix can grow.

## Known limitations

- No auto-update. See [Updates](#updates) below.
- Unsigned, un-notarized macOS bundle — first launch needs a
  right-click → Open (see [Installing](#installing)).
- OCR partial-failure handling: if individual pages fail (Ollama
  returns 5xx for those pages) the transcript is committed with
  inline `*[Page N: transcription failed — re-run OCR to retry]*`
  placeholders and a header note. If *every* page fails the
  transcript is not committed and the run surfaces the failure to
  the user instead.

## Updates

reHydrate does **not** auto-update. Subscribe to the GitHub repo's
release feed, or check the [Releases][releases] page from the
in-app About dialog (Help → About reHydrate → Check for updates).
Security fixes will be called out in the release notes.

[releases]: https://github.com/dm807cam/rehydrate/releases

## Optical character recognition

reHydrate doesn't ship an OCR model — it talks to an [Ollama][ollama]
daemon you run yourself. That keeps the bundle small, lets you swap
models without an app update, and means every byte of your handwriting
stays on hardware you control.

[ollama]: https://ollama.com/download

**Setup, in a fresh terminal:**

```sh
# Install Ollama (or grab the installer from ollama.com/download).
brew install ollama

# Pull a vision-language model. Pick one:
ollama pull qwen3.5:4b       # default — ~3.4 GB, runs on 8 GB GPUs / M-series
ollama pull qwen3.5:9b       # sharper at cursive + math — ~6.6 GB

# Start the daemon (background service on macOS).
```

Then open reHydrate, click the gear icon → **Settings → Ollama**,
hit **Test connection**, and Save. From there, "Convert to text…"
in any document's three-dot menu starts a transcription. Progress is
shown in a floating chip; results land in the Transcript drawer with
Save-as-`.txt` / Save-as-`.md` and **Publish** actions.

Pointing reHydrate at a different host is supported — useful if you
keep a small machine on your LAN just for inference. The host must
be reachable by name (give the box an mDNS / DNS entry, e.g.
`https://ollama.lan:11434`, and terminate TLS in front of Ollama)
because reHydrate refuses plain HTTP to anything other than
`localhost` and refuses HTTPS to literal private IPs — both close
SSRF-shaped paths. Every request is host-pinned and refuses
redirects (see `SECURITY.md`).

## Publishing transcripts

Transcripts can be POSTed directly into Ghost or WordPress as drafts:

- **Ghost**: Settings → Integrations → Custom integration; copy the
  Admin API key (the `<id>:<hex>` form) and the site URL into
  reHydrate's *Settings → Publishing* tab.
- **WordPress**: Users → Profile → Application Passwords (WP 5.6+);
  copy the password, paste alongside the site URL and your
  username.

Credentials live in your OS keychain. reHydrate only contacts the
host you entered — never anywhere else.

## Stack

- **Core:** Rust workspace (`crates/rehydrate-core`, `rehydrate-device`, `rehydrate-sync`, `rehydrate-app`, `rehydrate-ocr`, `rehydrate-publish`, `rm-parser`)
- **UI:** Tauri 2 + Vite + React + TypeScript (`ui/`)
- **Transport:** SSH/SFTP over USB-ethernet (`10.11.99.1`)

## Building

Prerequisites: Rust stable (≥ 1.82), Node.js 20.19+ or 22.12+, npm,
and (for bundle builds) the Tauri CLI:

```sh
cargo install tauri-cli --version "^2.0.0"
```

`./build.sh` is the one-liner. By default it produces a release
`.dmg` + `.app` under `target/aarch64-apple-darwin/release/bundle/`
— the same artefact the GitHub release workflow attaches to a tag.

```sh
./build.sh             # build the .dmg + .app bundle (release)
./build.sh --open      # ... and reveal the bundle in Finder
./build.sh --install   # ... and copy the .app to /Applications
./build.sh --dev       # quick iteration: cargo run -p rehydrate-app --release
```

For day-to-day dev with UI hot reload, prefer `cargo tauri dev`
(Vite serves the UI directly, no static bundle needed).

The bundle path requires an Apple Silicon host and passes
`--no-default-features` to strip the in-app webview inspector —
the release workflow does the same. See `PACKAGING.md` for the
full rationale and the manual `cargo tauri build` invocation.

## Testing against a real reMarkable

Two integration tests run against a tablet plugged in over USB. Both
are read-only.

```sh
# Capture password: tablet → Settings → Help → Copyrights and licenses
MARGINALIA_RM_PASSWORD='...' \
    cargo test -p rehydrate-device --features ssh \
    --test ssh_smoke -- --ignored --nocapture

MARGINALIA_RM_PASSWORD='...' \
    cargo test -p rehydrate-sync \
    --test full_pull_smoke -- --ignored --nocapture
```

Packaging notes (signing, notarization, release workflow) are in
`PACKAGING.md`.

## Layout

```
crates/
  rm-parser/         reMarkable v6 binary file parser
  rehydrate-core/    blob store, manifests, version log
  rehydrate-device/  device transport: trait + fake + ssh
  rehydrate-sync/    sync engine
  rehydrate-ocr/     Ollama client + page rendering
  rehydrate-publish/ Ghost / WordPress upload
  rehydrate-app/     Tauri binary
ui/                  React frontend
docs/                architecture notes + landing page
```

## Contributing

Bug reports, feature ideas, and pull requests are all welcome. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) for dev environment setup, the
test/lint commands CI runs, commit conventions, and how to file a
good bug. The project follows the
[Contributor Covenant](CODE_OF_CONDUCT.md) — be kind, assume good
faith. Security issues go through the
[private advisory form](https://github.com/dm807cam/rehydrate/security/advisories/new),
not public issues; see [`SECURITY.md`](SECURITY.md) for details.

## Licence

reHydrate is dual-licensed under either of:

- [MIT licence](LICENSE-MIT) ([summary][mit])
- [Apache License, Version 2.0](LICENSE-APACHE) ([summary][apache])

at your option. Pick whichever fits your downstream use; you don't
need to do anything special either way.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in reHydrate by you, as defined in the
Apache-2.0 licence, shall be dual-licensed as above, without any
additional terms or conditions.

The name **reHydrate** is reserved as a project identifier. You may
fork the code under the licences above, but please rename the fork
if you redistribute it so users don't confuse forks with the upstream
project.

[mit]: https://choosealicense.com/licenses/mit/
[apache]: https://choosealicense.com/licenses/apache-2.0/
