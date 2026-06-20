//! Compose `.rm` v6 annotations with an existing PDF. Uses `lopdf` directly
//! because `printpdf` can't modify an existing document. Two cases:
//!
//! * **Annotated source page** — the `.rm` layer is stroked on top of the
//!   matching PDF page (a content stream is appended and the page box grown
//!   to fit ink drawn past the edges).
//! * **Inserted note page** — a notebook page the user added *between* PDF
//!   pages (reMarkable `.content` `redir == null`). It has no backing PDF
//!   page, so a fresh page is created and spliced into the page order.
//!
//! Both share two ExtGState resources (opaque ink + highlighter alpha).

use rm_parser::shared::tool::Tool;
use rm_parser::v6::scene_item::line::Line;
use rm_parser::RemarkableFile;

use crate::dims::{OVERLAY_MARGIN_PT, OVERLAY_PT_PER_DEVICE};
use crate::strokes::{
    collect_v6_renderables, is_visible_tool, mean, stroke_color_rgb, width_pt_for,
};

/// One output page in an assembled annotated PDF.
pub struct PageSpec {
    /// `Some(i)` keeps source PDF page `i` (0-based), stroking `rm` on top if
    /// present. `None` inserts a brand-new page rendered from `rm` — a
    /// notebook page the user added between PDF pages.
    pub source_index: Option<usize>,
    /// The `.rm` annotation layer for this page, if any.
    pub rm: Option<Vec<u8>>,
}

/// Overlay `.rm` v6 annotation strokes onto an existing PDF document.
///
/// `annotations` is a list of `(page_index, rm_bytes)` pairs where
/// `page_index` is 0-based. Convenience wrapper over [`assemble_annotated_pdf`]
/// for the common all-overlay, no-inserted-pages case.
pub fn overlay_annotations_on_pdf(
    pdf_bytes: &[u8],
    annotations: &[(usize, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let plan: Vec<PageSpec> = annotations
        .iter()
        .map(|(idx, rm)| PageSpec {
            source_index: Some(*idx),
            rm: Some(rm.clone()),
        })
        .collect();
    assemble_annotated_pdf(pdf_bytes, &plan)
}

/// Build the output PDF from `plan`: the ordered list of pages, each either a
/// kept (optionally annotated) source page or a freshly inserted note page.
///
/// When the plan contains no inserted pages this only appends content streams
/// to the matching pages and leaves the page tree untouched (the low-risk
/// path). When it does, the page tree is flattened and rebuilt in plan order
/// so inserted notes land between the right PDF pages instead of being drawn
/// on top of them.
pub fn assemble_annotated_pdf(pdf_bytes: &[u8], plan: &[PageSpec]) -> Result<Vec<u8>, String> {
    if plan.is_empty() {
        return Ok(pdf_bytes.to_vec());
    }

    let mut doc = lopdf::Document::load_mem(pdf_bytes).map_err(|e| format!("PDF parse: {e}"))?;
    let pages = doc.get_pages();

    // Two named ExtGState objects shared across every annotated page: opaque
    // for normal ink, 0.39 alpha + Multiply blend for highlighter.
    let opaque_gs_id = doc.add_object(annotation_gs_dict(1.0, None));
    let hl_gs_id = doc.add_object(annotation_gs_dict(0.39, Some("Multiply")));

    let has_inserts = plan.iter().any(|s| s.source_index.is_none());

    if !has_inserts {
        // Pure overlay: touch only the pages that carry ink.
        for spec in plan {
            let (Some(idx), Some(rm_bytes)) = (spec.source_index, spec.rm.as_ref()) else {
                continue;
            };
            if let Some(&page_id) = pages.get(&((idx + 1) as u32)) {
                overlay_on_page(&mut doc, page_id, rm_bytes, opaque_gs_id, hl_gs_id)?;
            }
        }
        return save(doc);
    }

    // Inserted pages need the page tree rebuilt. Every kept page here is
    // self-contained (reMarkable PDFs carry MediaBox + Resources + Contents
    // on each page), so flattening to a single Pages node is safe.
    let pages_root = pages_root_id(&doc)?;
    let mut kids: Vec<lopdf::Object> = Vec::with_capacity(plan.len());
    let mut used: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for spec in plan {
        match spec.source_index {
            Some(idx) => {
                let Some(&page_id) = pages.get(&((idx + 1) as u32)) else {
                    continue;
                };
                if let Some(rm_bytes) = spec.rm.as_ref() {
                    overlay_on_page(&mut doc, page_id, rm_bytes, opaque_gs_id, hl_gs_id)?;
                }
                reparent(&mut doc, page_id, pages_root);
                kids.push(lopdf::Object::Reference(page_id));
                used.insert(idx);
            }
            None => {
                if let Some(rm_bytes) = spec.rm.as_ref() {
                    if let Some(page_id) =
                        make_inserted_page(&mut doc, rm_bytes, pages_root, opaque_gs_id, hl_gs_id)
                    {
                        kids.push(lopdf::Object::Reference(page_id));
                    }
                }
            }
        }
    }

    // Safety net: never silently drop a source page the plan failed to
    // mention (e.g. a `.content` that lists fewer pages than the PDF has).
    for (&num, &page_id) in &pages {
        let idx = (num - 1) as usize;
        if !used.contains(&idx) {
            reparent(&mut doc, page_id, pages_root);
            kids.push(lopdf::Object::Reference(page_id));
        }
    }

    let count = kids.len() as i64;
    if let Ok(root) = doc.get_dictionary_mut(pages_root) {
        root.set("Kids", lopdf::Object::Array(kids));
        root.set("Count", count);
    }

    save(doc)
}

fn save(mut doc: lopdf::Document) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    doc.save_to(&mut out).map_err(|e| format!("PDF save: {e}"))?;
    Ok(out)
}

/// The document's root `Pages` tree node (`/Root /Pages`).
fn pages_root_id(doc: &lopdf::Document) -> Result<lopdf::ObjectId, String> {
    let root = doc
        .trailer
        .get(b"Root")
        .and_then(lopdf::Object::as_reference)
        .map_err(|e| format!("PDF /Root: {e}"))?;
    doc.get_dictionary(root)
        .and_then(|d| d.get(b"Pages"))
        .and_then(lopdf::Object::as_reference)
        .map_err(|e| format!("PDF /Pages: {e}"))
}

fn reparent(doc: &mut lopdf::Document, page_id: lopdf::ObjectId, parent: lopdf::ObjectId) {
    if let Ok(d) = doc.get_dictionary_mut(page_id) {
        d.set("Parent", lopdf::Object::Reference(parent));
    }
}

/// Stroke one `.rm` layer onto an existing PDF page: append the ink content
/// stream, register the shared ExtGStates, and grow the page box vertically
/// to fit ink drawn past the top/bottom edges.
fn overlay_on_page(
    doc: &mut lopdf::Document,
    page_id: lopdf::ObjectId,
    rm_bytes: &[u8],
    opaque_gs_id: lopdf::ObjectId,
    hl_gs_id: lopdf::ObjectId,
) -> Result<(), String> {
    let media_box = annotation_media_box(doc, page_id);

    let Ok(rm) = RemarkableFile::read(rm_bytes) else {
        return Ok(());
    };
    let Some(rendered) = annotation_content_stream(&rm, &media_box) else {
        return Ok(());
    };

    doc.add_graphics_state(page_id, "RhGsOp", opaque_gs_id)
        .map_err(|e| format!("page ExtGState: {e}"))?;
    doc.add_graphics_state(page_id, "RhGsHl", hl_gs_id)
        .map_err(|e| format!("page ExtGState: {e}"))?;
    doc.add_page_contents(page_id, rendered.stream)
        .map_err(|e| format!("page contents: {e}"))?;
    set_page_media_box(doc, page_id, &media_box.grown_to(&rendered.ink_box));
    Ok(())
}

/// Create a standalone PDF page rendered from a `.rm` notebook page (no
/// backing PDF page). Sized to the ink it contains plus a margin. Returns the
/// new page's object id, or `None` if the layer has nothing to draw.
fn make_inserted_page(
    doc: &mut lopdf::Document,
    rm_bytes: &[u8],
    parent: lopdf::ObjectId,
    opaque_gs_id: lopdf::ObjectId,
    hl_gs_id: lopdf::ObjectId,
) -> Option<lopdf::ObjectId> {
    let rm = RemarkableFile::read(rm_bytes).ok()?;
    let (stream, media) = inserted_page_content(&rm)?;

    let content_id = doc.add_object(lopdf::Stream::new(lopdf::Dictionary::new(), stream));

    let mut ext_gstate = lopdf::Dictionary::new();
    ext_gstate.set("RhGsOp", lopdf::Object::Reference(opaque_gs_id));
    ext_gstate.set("RhGsHl", lopdf::Object::Reference(hl_gs_id));
    let mut resources = lopdf::Dictionary::new();
    resources.set("ExtGState", lopdf::Object::Dictionary(ext_gstate));

    let mut page = lopdf::Dictionary::new();
    page.set("Type", lopdf::Object::Name(b"Page".to_vec()));
    page.set("Parent", lopdf::Object::Reference(parent));
    page.set("MediaBox", media.to_object());
    page.set("Resources", lopdf::Object::Dictionary(resources));
    page.set("Contents", lopdf::Object::Reference(content_id));
    Some(doc.add_object(lopdf::Object::Dictionary(page)))
}

fn annotation_gs_dict(alpha: f32, blend_mode: Option<&str>) -> lopdf::Object {
    let mut d = lopdf::Dictionary::new();
    d.set("Type", lopdf::Object::Name(b"ExtGState".to_vec()));
    d.set("CA", lopdf::Object::Real(alpha));
    d.set("ca", lopdf::Object::Real(alpha));
    if let Some(bm) = blend_mode {
        d.set("BM", lopdf::Object::Name(bm.as_bytes().to_vec()));
    }
    lopdf::Object::Dictionary(d)
}

/// A page's rectangle in PDF user space: `[x0, y0]` lower-left,
/// `[x1, y1]` upper-right (PDF y points up).
#[derive(Clone, Copy)]
struct MediaBox {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl MediaBox {
    fn width(&self) -> f32 {
        self.x1 - self.x0
    }

    /// Return a box with the same horizontal extent but grown vertically so
    /// it also contains `ink` (plus a small margin). Width is deliberately
    /// left fixed: horizontal overflow is clipped, matching the reMarkable
    /// export, while vertical overflow expands the page.
    fn grown_to(&self, ink: &InkBox) -> MediaBox {
        MediaBox {
            x0: self.x0,
            x1: self.x1,
            y0: self.y0.min(ink.min_y - OVERLAY_MARGIN_PT),
            y1: self.y1.max(ink.max_y + OVERLAY_MARGIN_PT),
        }
    }

    /// A box tightly enclosing `ink` plus a margin on every side. Used for
    /// inserted note pages, which have no backing page to anchor to — the
    /// page simply becomes the size of what was written.
    fn around_ink(ink: &InkBox) -> MediaBox {
        MediaBox {
            x0: ink.min_x - OVERLAY_MARGIN_PT,
            y0: ink.min_y - OVERLAY_MARGIN_PT,
            x1: ink.max_x + OVERLAY_MARGIN_PT,
            y1: ink.max_y + OVERLAY_MARGIN_PT,
        }
    }

    fn to_object(self) -> lopdf::Object {
        lopdf::Object::Array(vec![
            lopdf::Object::Real(self.x0),
            lopdf::Object::Real(self.y0),
            lopdf::Object::Real(self.x1),
            lopdf::Object::Real(self.y1),
        ])
    }
}

/// Extent (PDF points) actually covered by drawn ink, including half the
/// stroke width so fat strokes at the edge aren't shaved.
struct InkBox {
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

struct Rendered {
    stream: Vec<u8>,
    ink_box: InkBox,
}

fn annotation_media_box(doc: &lopdf::Document, page_id: lopdf::ObjectId) -> MediaBox {
    let parse_box = |dict: &lopdf::Dictionary| -> Option<MediaBox> {
        let arr = dict.get(b"MediaBox").ok()?.as_array().ok()?;
        if arr.len() < 4 {
            return None;
        }
        let f = |o: &lopdf::Object| -> f32 {
            match o {
                lopdf::Object::Integer(i) => *i as f32,
                lopdf::Object::Real(r) => *r,
                _ => 0.0,
            }
        };
        // Normalise so x0<x1, y0<y1 regardless of how the array was written.
        let (x0, x1) = (f(&arr[0]).min(f(&arr[2])), f(&arr[0]).max(f(&arr[2])));
        let (y0, y1) = (f(&arr[1]).min(f(&arr[3])), f(&arr[1]).max(f(&arr[3])));
        if x1 > x0 && y1 > y0 {
            Some(MediaBox { x0, y0, x1, y1 })
        } else {
            None
        }
    };

    if let Ok(d) = doc.get_dictionary(page_id) {
        if let Some(b) = parse_box(d) {
            return b;
        }
        // MediaBox may be inherited from the parent Pages node.
        if let Ok(parent_id) = d.get(b"Parent").and_then(lopdf::Object::as_reference) {
            if let Ok(parent) = doc.get_dictionary(parent_id) {
                if let Some(b) = parse_box(parent) {
                    return b;
                }
            }
        }
    }
    MediaBox { x0: 0.0, y0: 0.0, x1: 595.0, y1: 842.0 } // A4 fallback
}

/// Write the expanded `MediaBox` directly onto the page object, overriding
/// any inherited value so the vertical growth actually takes effect.
fn set_page_media_box(doc: &mut lopdf::Document, page_id: lopdf::ObjectId, b: &MediaBox) {
    if let Ok(dict) = doc.get_dictionary_mut(page_id) {
        dict.set(
            "MediaBox",
            lopdf::Object::Array(vec![
                lopdf::Object::Real(b.x0),
                lopdf::Object::Real(b.y0),
                lopdf::Object::Real(b.x1),
                lopdf::Object::Real(b.y1),
            ]),
        );
    }
}

/// Render a `.rm` layer for *overlay* onto an existing page: x is centred on
/// the page width and y is flipped/anchored to the page top, so on-page ink
/// lands where the user drew it. The caller grows the page box to keep ink
/// drawn past the edges.
fn annotation_content_stream(rm: &RemarkableFile, media: &MediaBox) -> Option<Rendered> {
    let scale = OVERLAY_PT_PER_DEVICE;
    let page_cx = media.x0 + media.width() / 2.0;
    let page_top = media.y1;
    render_strokes(rm, |x, y| (page_cx + x * scale, page_top - y * scale))
}

/// Render a `.rm` notebook page as a standalone page: x/y are scaled and
/// y-flipped about the origin (no page to anchor to). Returns the ink content
/// stream and a page box sized to enclose it.
fn inserted_page_content(rm: &RemarkableFile) -> Option<(Vec<u8>, MediaBox)> {
    let scale = OVERLAY_PT_PER_DEVICE;
    let rendered = render_strokes(rm, |x, y| (x * scale, -y * scale))?;
    Some((rendered.stream, MediaBox::around_ink(&rendered.ink_box)))
}

/// Emit a PDF content stream stroking every visible `.rm` line, mapping each
/// device-space point through `map_xy`. Highlighters are drawn first (under
/// the ink) with butt caps; pens use round caps. Returns the stream and the
/// covered ink extent (incl. half stroke width).
fn render_strokes(
    rm: &RemarkableFile,
    map_xy: impl Fn(f32, f32) -> (f32, f32),
) -> Option<Rendered> {
    use std::fmt::Write;

    let (lines, _) = match rm {
        RemarkableFile::V6 { blocks, .. } => collect_v6_renderables(blocks),
        RemarkableFile::Other { .. } => return None,
    };
    if lines.is_empty() {
        return None;
    }

    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    let mut s = String::new();
    s.push_str("q\n1 J\n1 j\n/RhGsOp gs\n");

    let mut ordered: Vec<&Line> = lines.clone();
    ordered.sort_by_key(|l| if matches!(l.tool(), Tool::Highlighter | Tool::Shader) { 0_u8 } else { 1 });

    let mut last_rgb: Option<(f32, f32, f32)> = None;
    let mut last_w: Option<f32> = None;
    let mut last_is_hl = false;

    for line in &ordered {
        if !is_visible_tool(line.tool()) {
            continue;
        }
        let pts = line.points();
        if pts.is_empty() {
            continue;
        }

        let tool = line.tool();
        let is_hl = matches!(tool, Tool::Highlighter | Tool::Shader);

        if is_hl != last_is_hl {
            s.push_str(if is_hl { "/RhGsHl gs\n0 J\n" } else { "/RhGsOp gs\n1 J\n" });
            last_is_hl = is_hl;
        }

        let rgb = stroke_color_rgb(tool, line.color());
        if last_rgb != Some(rgb) {
            let _ = write!(s, "{:.4} {:.4} {:.4} RG\n", rgb.0, rgb.1, rgb.2);
            last_rgb = Some(rgb);
        }

        let thickness = (line.thickness_scale() as f32).clamp(0.3, 3.0);
        let widths: Vec<f32> = pts.iter().map(|p| width_pt_for(tool, p, thickness)).collect();
        let mapped: Vec<(f32, f32)> = pts.iter().map(|p| map_xy(p.x(), p.y())).collect();

        for (&(x, y), w) in mapped.iter().zip(&widths) {
            min_x = min_x.min(x - w / 2.0);
            max_x = max_x.max(x + w / 2.0);
            min_y = min_y.min(y - w / 2.0);
            max_y = max_y.max(y + w / 2.0);
        }

        let chunk_size = match tool {
            Tool::FineLiner | Tool::Marker | Tool::Highlighter | Tool::Shader => usize::MAX,
            _ => 5,
        };
        annot_chunks(&mut s, &mapped, &widths, chunk_size, &mut last_w);
    }

    s.push_str("Q\n");

    if !min_x.is_finite() {
        return None;
    }
    Some(Rendered {
        stream: s.into_bytes(),
        ink_box: InkBox {
            min_x,
            max_x,
            min_y,
            max_y,
        },
    })
}

fn annot_chunks(
    s: &mut String,
    pts: &[(f32, f32)],
    widths: &[f32],
    chunk_size: usize,
    last_w: &mut Option<f32>,
) {
    let n = pts.len();
    if n == 0 {
        return;
    }
    if chunk_size >= n || n < 2 {
        annot_segment(s, pts, mean(widths), last_w);
        return;
    }
    let mut start = 0;
    while start < n - 1 {
        let end = (start + chunk_size).min(n - 1);
        annot_segment(s, &pts[start..=end], mean(&widths[start..=end]), last_w);
        start = end;
    }
}

fn annot_segment(s: &mut String, pts: &[(f32, f32)], width_pt: f32, last_w: &mut Option<f32>) {
    use std::fmt::Write;
    if pts.is_empty() {
        return;
    }
    let w = width_pt.max(0.04);
    if last_w.map_or(true, |lw| (lw - w).abs() > 0.005) {
        let _ = write!(s, "{:.3} w\n", w);
        *last_w = Some(w);
    }
    let (x0, y0) = pts[0];
    if pts.len() == 1 {
        // Duplicate the point so the round cap renders a dot.
        let _ = write!(s, "{:.3} {:.3} m\n{:.3} {:.3} l\nS\n", x0, y0, x0, y0);
    } else {
        let _ = write!(s, "{:.3} {:.3} m\n", x0, y0);
        for (x, y) in &pts[1..] {
            let _ = write!(s, "{:.3} {:.3} l\n", x, y);
        }
        s.push_str("S\n");
    }
}
