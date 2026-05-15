//! Integration tests against the public `RemarkableFile::read` API.
//!
//! These exist as a regression guard against the bitreader EOF
//! contract — the parser's loop in `lib.rs::read_impl` calls
//! `Bitreader::eof()` between blocks, and `eof()` decides
//! "no more bytes" by checking for `ParseErrorKind::Io` on a
//! 1-byte read. A previous audit broke that contract by returning
//! `ParseErrorKind::InvalidInput` from the bounds guard in
//! `read_bytes`; the symptom was every notebook OCR failing with
//! "Failed to read eof of bitreader when trying to add context."
//!
//! The crate-internal unit tests in `bitreader.rs` cover the
//! `read_bytes`/`skip_bytes` error-kind contract directly. This
//! file exists so the *end-to-end* `read()` API — which is what
//! the OCR + render pipelines call — has its own line of defence.
//! If both layers ever drift, both have to be re-broken before a
//! regression ships.

use rm_parser::RemarkableFile;

/// The minimum well-formed v6 `.rm` file: the 43-byte header
/// followed by zero blocks. The parser walks the header, enters the
/// v6 branch, calls `eof()`, gets `Ok(true)`, exits the loop, and
/// returns `RemarkableFile::V6 { tree, blocks: [] }`.
///
/// If `eof()` ever stops recognising end-of-stream — as it did
/// during the phase-4 regression — `read()` returns
/// `Err("Error while parsing remarkable file Failed to read eof
/// of bitreader …")` instead, and this test goes red.
#[test]
fn header_only_v6_file_parses_to_empty_blocks() {
    // Header layout: `read_bytes(43)` then `trim_end()` looks for
    // the literal `"reMarkable .lines file, version=6"`.
    let mut bytes = Vec::with_capacity(43);
    bytes.extend_from_slice(b"reMarkable .lines file, version=6");
    let pad = 43 - bytes.len();
    bytes.resize(bytes.len() + pad, b' ');
    assert_eq!(bytes.len(), 43);

    let rm = RemarkableFile::read(&bytes[..])
        .expect("header-only v6 file must parse cleanly — see file doc");
    match rm {
        RemarkableFile::V6 { blocks, .. } => {
            assert!(
                blocks.is_empty(),
                "header-only file should yield zero blocks, got {}",
                blocks.len()
            );
        }
        RemarkableFile::Other { version, .. } => {
            panic!("expected V6 variant, got Other(version={version})");
        }
    }
}

/// Truncated header (less than 43 bytes) is a real error — the
/// parser can't even read the version string. This locks the
/// "Io kind on overshoot" contract from the other direction: a
/// real truncation should surface as a parse error (which then
/// propagates up to the user-facing toast), not as silent EOF.
#[test]
fn truncated_header_errors() {
    let bytes = b"reMarkable .lines"; // 17 bytes, well short of 43.
    let r = RemarkableFile::read(&bytes[..]);
    assert!(r.is_err(), "truncated header must not parse");
}

/// Header announcing an unsupported version yields a typed
/// `Unsupported` error rather than an `Io` or `InvalidInput`
/// one. The `RemarkableFile::read` Display chain folds the
/// version number into the message, so a quick substring check
/// catches accidental message-shape drift.
#[test]
fn unknown_version_errors_as_unsupported() {
    let header = b"reMarkable .lines file, version=99         ";
    assert_eq!(header.len(), 43);
    let err = RemarkableFile::read(&header[..]).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("99"),
        "error message should cite the unsupported version, got: {msg}"
    );
}

/// Build the 43-byte version header for a v3-v5 `.rm` file.
fn v3_header() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(43);
    bytes.extend_from_slice(b"reMarkable .lines file, version=3");
    let pad = 43 - bytes.len();
    bytes.resize(bytes.len() + pad, b' ');
    assert_eq!(bytes.len(), 43);
    bytes
}

/// Issue #34 regression: a malicious `amount_points = u32::MAX` in the
/// v3-v5 parser used to drive `(0..N).map(...).collect::<Vec<Point>>()`,
/// which calls `Vec::with_capacity(u32::MAX as usize)` via
/// `Range<u32>`'s exact size_hint. That's a ~100 GB allocation
/// attempt *before* the inner `read_f32` notices EOF — on macOS
/// without overcommit the allocator returns null and the process
/// aborts; on Linux with overcommit the kernel OOM-kills it. Either
/// way a single bad file from a tablet kills the user's sync.
///
/// With the cap in `line.rs`, `Vec::with_capacity` is bounded by
/// `remaining() / 24` (the size of a Point), so a header-truncated
/// file fails fast on the first `read_f32`. If the cap regresses,
/// this test wedges or OOM-kills the runner — exactly the production
/// shape we're guarding against.
#[test]
fn malicious_amount_points_does_not_oom() {
    let mut bytes = v3_header();
    bytes.extend_from_slice(&1u32.to_le_bytes()); // amount_layers
    bytes.extend_from_slice(&1u32.to_le_bytes()); // amount_lines
    bytes.extend_from_slice(&0u32.to_le_bytes()); // tool (Brush)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // color
    bytes.extend_from_slice(&0u32.to_le_bytes()); // skipped unknown
    bytes.extend_from_slice(&0f32.to_le_bytes()); // brush_size
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // amount_points: ATTACK
    let r = RemarkableFile::read(&bytes[..]);
    assert!(
        r.is_err(),
        "u32::MAX amount_points must error out, not allocate ~100 GB"
    );
}

/// Issue #34 regression: matching guard for `amount_lines` in
/// `layer.rs`. Same DoS shape — without the cap, `Vec::with_capacity`
/// for u32::MAX Lines (≥ 20 bytes each) attempts ~80 GB before the
/// first child Line fails on EOF.
#[test]
fn malicious_amount_lines_does_not_oom() {
    let mut bytes = v3_header();
    bytes.extend_from_slice(&1u32.to_le_bytes()); // amount_layers
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // amount_lines: ATTACK
    let r = RemarkableFile::read(&bytes[..]);
    assert!(
        r.is_err(),
        "u32::MAX amount_lines must error out, not allocate ~80 GB"
    );
}

/// Issue #34 regression: matching guard for `amount_layers` in
/// `page.rs`. A Layer is at least 4 bytes (its own amount_lines
/// u32), so u32::MAX of them would be ~16 GB of `Vec<Layer>` capacity.
#[test]
fn malicious_amount_layers_does_not_oom() {
    let mut bytes = v3_header();
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // amount_layers: ATTACK
    let r = RemarkableFile::read(&bytes[..]);
    assert!(
        r.is_err(),
        "u32::MAX amount_layers must error out, not allocate ~16 GB"
    );
}
