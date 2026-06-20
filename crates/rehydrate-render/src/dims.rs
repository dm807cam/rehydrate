//! Shared page-and-canvas constants. Pulled out of the rendering modules
//! so the conversion factors live in one place.

pub(crate) const PAGE_W_MM: f32 = 210.0;
pub(crate) const PAGE_H_MM: f32 = 297.0;
pub(crate) const PT_PER_INCH: f32 = 72.0;
pub(crate) const MM_PER_INCH: f32 = 25.4;

pub(crate) fn mm_to_pt(mm: f32) -> f32 {
    mm * PT_PER_INCH / MM_PER_INCH
}

/// Panel density used to map `.rm` device-space coordinates to PDF points
/// in the annotation-overlay path. reMarkable lays ink down at the panel's
/// native DPI, so a device coordinate converts to points by the single
/// uniform factor `72 / DPI` — the same `SCALE = 72.0 / SCREEN_DPI` that
/// `rmc`/`rmscene` use. 226 is the RM2 / reMarkable Paper Pro density.
///
/// This replaces the old per-axis stretch (`page_w/1404` × `page_h/1872`),
/// which fit the *entire* device canvas onto the existing PDF page box. That
/// was wrong twice over: the two axes scaled by different factors (a ~2.4×
/// vertical squash on a 16:9 page) and ink drawn outside the page rectangle
/// — every margin note on an annotated PDF — was scaled past the page edge
/// and clipped. The uniform factor keeps ink undistorted and correctly
/// sized; the page is then grown vertically (see `overlay`) so nothing is
/// lost off the top or bottom.
pub(crate) const OVERLAY_SCREEN_DPI: f32 = 226.0;

/// PDF points per `.rm` device unit for the overlay path. See
/// [`OVERLAY_SCREEN_DPI`].
pub(crate) const OVERLAY_PT_PER_DEVICE: f32 = PT_PER_INCH / OVERLAY_SCREEN_DPI;

/// Padding (PDF points) left around the ink extent when the page is grown
/// to fit annotations that spill past the original page rectangle. Roughly
/// the reMarkable export's own breathing room (~100 device units).
pub(crate) const OVERLAY_MARGIN_PT: f32 = 14.0;
