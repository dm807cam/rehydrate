# Security & threat model

reHydrate is a desktop app that mirrors a reMarkable tablet over USB-ethernet
into a local, content-addressed library on the user's machine. This document
records the threat model the v1.0 codebase was built against and the
mitigations in place — so contributors and security-minded users can audit
the trade-offs without reading every commit message.

## Trust boundaries

| Boundary | Who's trusted | Who's not |
| --- | --- | --- |
| Local machine | The OS, the user account running reHydrate | Other local users; processes running as another UID |
| Device over USB-ethernet | The tablet physically plugged in by the user | Arbitrary servers reachable at `10.11.99.1` |
| Tablet's xochitl filesystem | The reMarkable firmware as shipped | Files placed in the tree by other apps or by an attacker with prior device access |
| Local library on disk | The user account's home directory | Anyone with read access to that directory |
| Imported PDFs / EPUBs | Files the user dropped onto the window or picked via the file dialog | The renderers that parse them (printpdf, image, lopdf) — treated as untrusted parsers |

## Network surface

reHydrate has **three** sanctioned network paths, each initiated by an
explicit user action:

1. **SSH/SFTP to the tablet** via `russh` + `russh-sftp`. Default
   endpoint `10.11.99.1:22` (the tablet's USB-ethernet address).
   Triggered by **Connect** / **Sync**.
2. **HTTP to a user-provided Ollama daemon** for OCR. Default
   endpoint `http://localhost:11434`; the user can point this at a
   different host in the Settings modal. Triggered by **Convert to
   text…** and by the Settings tab's **Test connection** button.
3. **HTTPS to a user-provided Ghost or WordPress site** for transcript
   publishing. Triggered by **Publish to Ghost / WordPress** in the
   Transcript drawer.

Every business-logic crate that isn't on this list (`rehydrate-core`,
`rehydrate-device`, `rehydrate-sync`) is asserted free of HTTP
clients by `tests-integration/tests/no_egress.rs` — failing CI is
the wall against accidental egress. The two sanctioned-egress crates
(`rehydrate-ocr`, `rehydrate-publish`) each route their requests
through a host-pinned `RestrictedAgent` (assertion:
`sanctioned_egress_crates_pin_to_one_host`) that:

- pins to the host of the configured base URL,
- refuses redirects (no `Location:` rewrite to a third party), and
- enforces a hard timeout per request.

The Tauri webview's CSP keeps `connect-src` to `'self'` and Tauri's
own IPC bridge. The renderer never loads remote URLs — images come
from local blobs (`data:`/`blob:` URLs) only.

## SSH host-key verification

**v1.0 uses persistent TOFU (Trust On First Use).** The first
successful connection to `10.11.99.1:22` records the tablet's host
key fingerprint in a per-app `known_hosts.json` (mode `0o600`).
Every subsequent connect compares the live key against the stored
fingerprint:

- Match → connection proceeds normally.
- Mismatch → reHydrate refuses to connect and surfaces a
  `HostKeyChanged` error naming the expected and observed
  fingerprints. The user has to manually delete the stored entry
  (or factory-reset the tablet so its key actually changed) before
  another connect attempt can succeed.

The fingerprint is committed *after* successful authentication, so
an attacker who can intercept the very first connect (before any
key has been recorded) can still pin themselves as the trusted
host — that's the classic TOFU compromise window. The threat model
assumes the first connect happens over a trusted USB cable to a
tablet the user physically owns; anyone running reHydrate against
an SSH endpoint they did NOT just unbox should verify the recorded
fingerprint out-of-band before relying on the persistence guarantee.

A future release may add explicit fingerprint pinning at config
time so users can avoid the TOFU window entirely.

## Ollama (OCR backend)

reHydrate does not bundle an OCR model — it sends rendered notebook
pages to an Ollama daemon the user runs themselves (default
`http://localhost:11434`). What you should know:

- **Local-loopback by default.** The default URL never leaves the
  machine. If you change it to a LAN or internet address, you are
  explicitly opting into **both transcripts *and* page image
  bytes** (PNG renders of each notebook page) traversing your
  network to that host. This is not metadata-only — it's the
  literal content of your handwriting.
- **Scheme is validated.** `save_ollama_config` and `ping_ollama`
  reject `file://`, `gopher://`, etc., and refuse `http://` for
  any non-loopback host. Pointing OCR at a LAN box requires
  `https://` so PNG bytes don't cross the network in plaintext.
- **No authentication.** Ollama's `/api/generate` is unauthenticated;
  anyone with network access to the daemon can use it. Keep the
  daemon firewalled to your trusted network if you expose it past
  localhost. The "Test connection" probe also returns the full
  list of models the daemon has pulled (via `/api/tags`) — if you
  point at a *shared* Ollama, you can see what other users on
  that daemon have downloaded.
- **Host-pin + no redirects.** `crates/rehydrate-ocr/src/http.rs`
  wraps `ureq` in a `RestrictedAgent` that refuses any request to
  a host other than the one parsed from the configured base URL,
  and disables HTTP redirects entirely — so a malicious DNS reply
  or compromised proxy can't quietly send your transcripts elsewhere.
  The `no_egress.rs` integration test asserts that no `.rs` file
  in `rehydrate-ocr` or `rehydrate-publish` (other than `http.rs`
  itself) constructs a `ureq::Agent` directly — preventing a future
  refactor from accidentally routing around the host pin.
- **No assumed trust in Ollama itself.** A compromised local Ollama
  could return crafted output that's later rendered as Markdown
  inside the app. Transcript Markdown is rendered as plain text in
  the drawer (no HTML interpretation) and converted to HTML *only*
  when the user clicks Publish; even there it goes to the
  user-configured CMS endpoint, not back into reHydrate's UI.
- **Auto-OCR-at-startup is opt-in.** The Settings → Ollama tab
  exposes a toggle that, when enabled, transcribes every notebook
  without an existing transcript when the app launches. This
  *does* mean outbound traffic happens without further per-doc
  consent — but only after the user explicitly checked the box.
  The toggle defaults to off; an unreachable Ollama silently
  skips the sweep (no auto-modal nag at launch). If you have
  configured a remote Ollama and turn this on, expect every app
  launch to ship page images of any new notebooks to that host
  until the sweep completes.

## Publishing (Ghost / WordPress)

The publish path uses the same host-pinned `RestrictedAgent` pattern
(see `crates/rehydrate-publish/src/http.rs`):

- Ghost: Admin API with a JWT signed locally per request.
- WordPress: REST API with Basic auth using an Application Password
  (WP 5.6+).

Credentials are JSON-encoded into the OS keyring; reHydrate never
writes them to a plaintext config file. The publish action only
fires when the user clicks the Publish button in the Transcript
drawer — there is no background-publish.

## Credentials

- The SSH password the tablet shows under *Settings → Help → Copyrights and
  licenses → "GPLv3 Compliance"* is stored in the OS keyring via the
  `keyring` crate. Shipped binaries are macOS Apple-Silicon only and
  use the system Keychain (Login). Source builds on other platforms
  fall back to keyring's in-memory mock store unless the workspace is
  rebuilt with the corresponding backend feature (`linux-native` /
  `sync-secret-service` for Linux, `windows-native` for Windows);
  those configurations are not part of CI and are not security-
  reviewed.
- The password is held in memory as `secrecy::SecretString`, which
  zero-fills on drop. The only places it's materialised as a `String`
  are the keyring write (`set_password`) and the russh `authenticate_password`
  call; both are scoped to the connect path.
- If `keyring::Entry::new` or `set_password` returns an error (e.g.
  Keychain access denied), the app surfaces a `keyring:warning` toast
  — the password is **not** silently written to plaintext.
- `forget_device_password` deletes the keyring entry.

## Untrusted device data

The tablet is treated as **untrusted** even though most users own theirs:
firmware bugs, third-party hacks (e.g. KOReader), and prior device
compromise are realistic. Every device-sourced byte is filtered:

- **File sizes** are capped at 512 MiB per file (`MAX_REMOTE_FILE_BYTES`).
  A malicious tablet can't OOM the client by serving an enormous file.
- **Recursion** during `fetch_subtree` is bounded at 16 levels and
  symlinks are skipped, so a directory loop on the device can't trap us.
- **Manifest paths** are validated against `Manifest::validate_paths`:
  no `..`, no absolute / `~` / `\\` / `:`, no control characters, no
  trailing dots / spaces, no Windows reserved names (`CON`, `PRN`, …).
  This applies to every path before the library reconstructs the document
  on disk or opens a cache file.
- **Export filenames** are sanitised with the same Windows-safe rules
  (`sanitize()` in `commands.rs`).
- **PDF / EPUB body extensions** are allow-listed to `{pdf, epub}` so a
  manifest can't trick `open_document` into materialising an executable
  type for the OS opener.
- **Magic-byte sniff** on imported PDFs / EPUBs (`%PDF-` / `PK\x03\x04`)
  guards against a renamed file confusing the library.

## Renderer trust boundary

reHydrate ships the Tauri webview as a **trusted renderer** — the
frontend is allowed to call any registered Tauri command. This means a
compromised renderer (XSS in third-party deps, etc.) can read the entire
library through the existing commands. v1.0 mitigations:

- **`devtools` is disabled by default and absent from release builds.**
  The feature flag is opt-in (`default = []` on `rehydrate-app`).
  Contributors enable it explicitly with `--features devtools` (or
  via `./build.sh --dev`, which adds the flag for you). The release
  workflow builds with `--no-default-features` as belt-and-suspenders
  so end users can never open the inspector and arbitrary-JS-evaluate.
- **CSP** locks `script-src` to `'self'` — no inline scripts, no remote
  scripts. `style-src 'unsafe-inline'` remains as a v1.0 carve-out for
  React inline styles; it'll be tightened in a follow-up.
- **No HTTP clients in the renderer**, so a compromised renderer can't
  exfiltrate to a remote endpoint without help from the Rust side.
- **`tauri-plugin-opener`** is the only OS-handler bridge. It's invoked
  only with paths inside reHydrate's cache directory and only for
  `{pdf, epub}` extensions.

## Imported files

- `import_file` (picker) and `import_dropped_file` (drag-drop) both apply
  the magic-byte sniff and a 512 MiB cap before staging bytes to a
  `tempfile::NamedTempFile` (auto-deletes on panic / error).
- The parsers (`printpdf`, `image`, `lopdf`) are treated as **untrusted
  code paths** — they parse arbitrary bytes the user supplied. Past
  versions of `image` have had CVEs in their JPEG / PNG decoders.
  reHydrate keeps these crates on pinned versions and updates them in
  release notes when CVEs land.

## Logs

The tracing log writes to a daily-rotated file under the platform's data
directory. The log retention is capped at 30 files (`LOG_RETENTION_FILES`)
and `read_tail` streams newest-first instead of loading every file into
memory.

Logged content is **trace-level for routine operations and warn / error
for failures**. We make a best effort to avoid logging:

- The SSH password (never logged; `SecretString::expose_secret` is only
  ever passed to `set_password` / `authenticate_password`).
- Full document contents.

We **do** log document UUIDs and visible names, file paths inside the
library, and the tablet's exec-output strings. Treat log files as
sensitive data on multi-user systems.

## What's deliberately out of scope for v1.0

- **Auto-update**: there is none. Users get new versions only by
  downloading them. We didn't want to ship an updater without a
  code-signing infrastructure to back it.
- **Sandboxing of imported PDFs**: a malicious PDF that exploits a
  decoder bug would run with the app's privileges. v1.0 relies on
  upstream decoders being CVE-clean.
- **Explicit SSH host-key pinning at config time**: today's flow is
  persistent TOFU (see above), which still has a first-connect
  window. A pinned-fingerprint config option would close it.
- **Telemetry**: there is none, by design.

## Reporting

Found a security issue? Please report it privately through GitHub's
[Report a Vulnerability](https://github.com/dm807cam/rehydrate/security/advisories/new)
flow on this repository. That channel is monitored and routes
directly to the maintainers without a public issue ever being
created. Please don't open a public issue for security-sensitive
matters.

If you can't use the GitHub flow, open a minimally-detailed public
issue saying you have a security report to share and asking for a
private channel; a maintainer will follow up.

### Supported versions

Only the latest released `v1.x.y` receives security fixes. v0.x
releases are unsupported.
