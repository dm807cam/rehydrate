//! Render a notebook into a multi-page A4 PDF.
//!
//! Three pipelines live in this crate:
//! - [`build_pdf_from_rm_files`]: parse the device's `.rm` ink files
//!   (v6 format) and render each stroke with PDF's native stroking
//!   pipeline. Sharp at any zoom, real pressure variation.
//! - [`build_pdf_from_pngs`] / [`build_pdf_from_image_bytes`]: stitch
//!   the device's per-page thumbnail PNGs into a PDF (used as a
//!   fallback when a page has no `.rm` data), and turn an arbitrary
//!   raster image into a single-page PDF for the drag-import path.
//! - [`overlay_annotations_on_pdf`]: take an existing PDF (a synced
//!   PDF document on the tablet) and stroke each annotation `.rm`
//!   layer on top of the corresponding page.
//!
//! Rendering approach (matches rmrl, rmc, lines-are-rusty):
//! Each stroke is emitted as one or more stroked polylines using
//! `Op::DrawLine` (PDF `S` operator) with round caps + round joins. The
//! round cap *is* a perfect half-disc at the line's endpoint, so we get
//! authentic rounded ends without stamping geometry. For tools that vary
//! width along the stroke (ballpoint, brush, pencil, mech pencil,
//! calligraphy) we chunk the polyline into ~5-sample groups and stroke
//! each group at its average width — adjacent chunks share an endpoint
//! so the caps blend invisibly.
//!
//! Highlighter is one wide constant-width polyline with butt caps and a
//! ~0.39 alpha applied through an ExtGState; rendered before pens so it
//! sits underneath the ink, matching the device.
//!
//! Submodules:
//!   - `dims`         shared page + RM canvas constants and `mm_to_pt`
//!   - `strokes`      v6 scene walk + per-point width / colour tables,
//!                    shared by `rm_to_pdf` and `overlay`
//!   - `rm_to_pdf`    `.rm` → fresh PDF page via printpdf's Op model
//!   - `image_to_pdf` PNG-stitch fallback + single-image conversion
//!   - `overlay`      annotation layer on top of an existing PDF (lopdf)
//!
//! ## Cache-busting contract
//!
//! [`PREVIEW_LAYOUT_VERSION`] is part of the per-document preview cache
//! key in `rehydrate-app::commands::open_document`. The constant lives
//! here, next to the renderer, so the same edit that changes the
//! renderer's visual output is the edit that bumps the version. The
//! field is co-located *on purpose* — a previous regression cached
//! blurry thumbnail-fallback PDFs while the parser was broken, and
//! recovering required a manual bump from the app crate. With the
//! constant next door, "I changed the renderer, I should bump this"
//! is a single-file decision.

mod dims;
mod image_to_pdf;
mod overlay;
mod rm_to_pdf;
mod strokes;

pub use image_to_pdf::{build_pdf_from_image_bytes, build_pdf_from_pngs};
pub use overlay::{assemble_annotated_pdf, overlay_annotations_on_pdf, PageSpec};
pub use rm_to_pdf::build_pdf_from_rm_files;

/// Version suffix included in the per-document preview cache key.
///
/// **Bump this whenever any change to this crate alters the visual
/// output of `build_pdf_from_rm_files` / `build_pdf_from_pngs`.**
/// The app's `open_document` command keys cached preview PDFs as
/// `<safe_name>-<document_id>-<manifest_hash>-<PREVIEW_LAYOUT_VERSION>.pdf`,
/// so a change to the renderer that doesn't bump this constant
/// will silently serve stale PDFs forever.
///
/// Version history (each entry: short reason, commit prefix):
/// - `ink-v16` — original public version after the audit refactor.
/// - `ink-v17` — invalidate fallback PDFs cached during the
///   phase-4 bitreader regression (`0d78c9a`).
/// - `ink-v18` — reMarkable Paper Pro palette colours (green-2, cyan,
///   magenta, …) now render in colour instead of black, and the
///   annotation overlay uses a uniform device-to-point scale.
///
/// The constant is the single source of truth; do not redeclare it
/// elsewhere in the workspace.
pub const PREVIEW_LAYOUT_VERSION: &str = "ink-v18";

/// Version suffix included in the per-document drag-out export cache
/// key. Independent of [`PREVIEW_LAYOUT_VERSION`] so a future change
/// to the export pipeline (e.g. embedding an invisible OCR text layer
/// for searchable PDFs) doesn't invalidate the Preview cache, and
/// vice-versa.
///
/// The export cache is separated so callers can drop a file with a clean
/// human filename into a per-document staging directory without colliding
/// with the Preview cache's hash-laden filenames.
///
/// Bump on any change that alters exported bytes:
/// - `export-v1` — initial.
/// - `export-v2` — drag-out now strokes `.rm` annotation layers onto PDF
///   bodies (previously copied verbatim), with the uniform-scale overlay
///   and Paper Pro palette colours. Invalidates the old body-hash-keyed
///   entries that staged un-annotated PDFs.
/// - `export-v3` — inserted notebook pages (reMarkable `redir == null`) are
///   now spliced in as their own pages instead of being stamped on top of
///   the adjacent PDF page.
pub const EXPORT_LAYOUT_VERSION: &str = "export-v3";
