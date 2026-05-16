//! End-to-end pipeline regression guard.
//!
//! Exercises the same call chain the OCR + notebook-preview Tauri
//! commands take: raw `.rm` v6 bytes → `RemarkableFile::read` →
//! `build_pdf_from_rm_files` → produced PDF bytes.
//!
//! The phase-4 audit regression took notebook OCR out for every
//! document because `Bitreader::eof()` started erroring on every
//! valid stream; the cached fallback path then masked the failure
//! with blurry thumbnails. The crate-internal unit tests in
//! `rm-parser` cover the bitreader contract; this file covers the
//! *combined* contract — if the parser-renderer integration ever
//! regresses again, this test catches it before the user does.

use rehydrate_render::build_pdf_from_rm_files;

/// The smallest well-formed v6 `.rm` file: 43-byte header, zero
/// blocks. Renders to a single blank A4 page. The shape of the
/// produced PDF doesn't matter — what matters is that the call
/// returns `Ok(_)` rather than the "Failed to read eof of
/// bitreader" parse error the regression produced.
fn minimal_v6_page() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(43);
    bytes.extend_from_slice(b"reMarkable .lines file, version=6");
    let pad = 43 - bytes.len();
    bytes.resize(bytes.len() + pad, b' ');
    assert_eq!(bytes.len(), 43);
    bytes
}

#[test]
fn renders_minimal_v6_notebook_without_parse_error() {
    let page = minimal_v6_page();
    let pdf = build_pdf_from_rm_files("regression-fixture", &[page], None)
        .expect("minimal v6 notebook must render — see file doc");
    // PDF files begin with `%PDF-` — confirm we produced something
    // that at least passes the file-magic check rather than a
    // truncated buffer.
    assert!(
        pdf.starts_with(b"%PDF-"),
        "output should be a valid PDF; got {} bytes starting with {:?}",
        pdf.len(),
        &pdf[..pdf.len().min(8)]
    );
}

#[test]
fn renders_multi_page_v6_notebook() {
    // Two header-only pages — exercises the per-page parse loop in
    // `build_pdf_from_rm_files`, which was where the bigger
    // notebooks failed during the regression (one bad parse = the
    // whole call errors and the cached fallback wins).
    let pages = vec![minimal_v6_page(), minimal_v6_page(), minimal_v6_page()];
    let pdf = build_pdf_from_rm_files("multi", &pages, None)
        .expect("three-page header-only notebook must render");
    assert!(pdf.starts_with(b"%PDF-"));
}

#[test]
fn empty_pages_slice_is_typed_error_not_panic() {
    // The function returns Err("notebook has no pages") when the
    // slice is empty. Locking this so a future refactor doesn't
    // change the shape of the empty-input failure (the commands
    // layer matches on `Err(_)` and falls back to thumbnails).
    let r = build_pdf_from_rm_files("nope", &[], None);
    assert!(r.is_err(), "empty pages slice should error, got Ok");
}
