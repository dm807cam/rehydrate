//! Render a notebook into a multi-page A4 PDF.
//!
//! Two paths exist:
//! - `build_pdf_from_rm_files`: parse the device's `.rm` ink files (v6
//!   format) and render each stroke with PDF's native stroking pipeline.
//!   Sharp at any zoom, real pressure variation.
//! - `build_pdf_from_pngs`: stitch the device's per-page thumbnail PNGs
//!   into a PDF. Used as a fallback when a page has no `.rm` data.
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

use std::collections::{HashMap, HashSet};

use image::ImageReader;
use printpdf::{
    BuiltinFont, Color, ExtendedGraphicsState, ExtendedGraphicsStateId, Line as PdfLine,
    LineCapStyle, LineDashPattern, LineJoinStyle, LinePoint, Mm, Op, PdfDocument, PdfFontHandle,
    PdfPage, PdfSaveOptions, Point, Pt, RawImage, Rgb, TextItem as PdfTextItem, XObjectTransform,
};
use rm_parser::shared::{pen_color::PenColor, tool::Tool};
use rm_parser::v6::block::Block;
use rm_parser::v6::crdt::CrdtId;
use rm_parser::v6::scene_item::line::Line;
use rm_parser::v6::scene_item::point::Point as RmPoint;
use rm_parser::v6::scene_item::text::{Text, TextItem as RmTextItem};
use rm_parser::RemarkableFile;

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
///
/// The constant is the single source of truth; do not redeclare it
/// elsewhere in the workspace.
pub const PREVIEW_LAYOUT_VERSION: &str = "ink-v17";

/// Version suffix included in the per-document drag-out export cache
/// key. Independent of [`PREVIEW_LAYOUT_VERSION`] so a future change
/// to the export pipeline (e.g. embedding an invisible OCR text layer
/// for searchable PDFs) doesn't invalidate the Preview cache, and
/// vice-versa.
///
/// Today the export pipeline produces byte-identical PDFs to the
/// Preview pipeline — the cache is separated so callers can drop a
/// file with a clean human filename into a per-document staging
/// directory without colliding with the Preview cache's hash-laden
/// filenames.
pub const EXPORT_LAYOUT_VERSION: &str = "export-v1";

const PAGE_W_MM: f32 = 210.0;
const PAGE_H_MM: f32 = 297.0;
const PT_PER_INCH: f32 = 72.0;
const MM_PER_INCH: f32 = 25.4;

fn mm_to_pt(mm: f32) -> f32 {
    mm * PT_PER_INCH / MM_PER_INCH
}

/// Build a multi-page A4 PDF from `.rm` v6 byte buffers, in order.
pub fn build_pdf_from_rm_files(title: &str, pages: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    if pages.is_empty() {
        return Err("notebook has no pages".to_string());
    }

    let mut doc = PdfDocument::new(title);
    let mut warnings = Vec::new();
    let mut pdf_pages = Vec::with_capacity(pages.len());

    // Register the two ExtGStates the renderer toggles between: opaque
    // ink for normal pens, ~0.39 alpha for highlighter strokes (matches
    // rmrl). Returned IDs are reused on every page.
    let opaque_gs = doc.add_graphics_state(
        ExtendedGraphicsState::default()
            .with_current_stroke_alpha(1.0)
            .with_current_fill_alpha(1.0),
    );
    let highlighter_gs = doc.add_graphics_state(
        ExtendedGraphicsState::default()
            .with_current_stroke_alpha(0.39)
            .with_current_fill_alpha(0.39),
    );

    for (i, bytes) in pages.iter().enumerate() {
        let rm =
            RemarkableFile::read(bytes.as_slice()).map_err(|e| format!("page {i} parse: {e}"))?;
        let ops = render_rm_to_ops(&rm, &opaque_gs, &highlighter_gs)?;
        pdf_pages.push(PdfPage::new(Mm(PAGE_W_MM), Mm(PAGE_H_MM), ops));
    }

    Ok(doc
        .with_pages(pdf_pages)
        .save(&PdfSaveOptions::default(), &mut warnings))
}

/// Collect the renderable items from a parsed v6 file.
///
/// Filtering applied here, before any geometry math:
///
/// * `CrdtSequenceItem::deleted_length > 0` items are skipped — those
///   are CRDT tombstones for erased strokes / deleted text. Without
///   this filter, an eraser stroke on the tablet still shows up in
///   the rendered PDF.
/// * Items whose parent `TreeNode` (i.e. layer / group) has
///   `visible.value == false` are skipped — hiding a layer on the
///   device should hide it in the export too.
fn collect_v6_renderables(blocks: &[Block]) -> (Vec<&Line>, Vec<&Text>) {
    // First pass: build a `parent_id -> is_visible` map from
    // `TreeNode` blocks. A missing entry means we never saw a
    // visibility declaration for that group, so we err on the side
    // of rendering (matches the device default).
    let mut visibility: HashMap<CrdtId, bool> = HashMap::new();
    for b in blocks {
        if let Block::TreeNode(tn) = b {
            visibility.insert(tn.group.node_id, tn.group.visible.value);
        }
    }
    // Transitive visibility: a child group inherits "hidden" from
    // any ancestor group. SceneGroupItem blocks place a group under
    // a parent (the parent_id); walk those edges and mark a group
    // hidden if any ancestor is hidden.
    let mut group_parent: HashMap<CrdtId, CrdtId> = HashMap::new();
    for b in blocks {
        if let Block::SceneGroupItem(sgi) = b {
            if let Some(child) = sgi.item.value {
                group_parent.insert(child, sgi.parent_id);
            }
        }
    }
    let is_visible = |mut id: CrdtId| -> bool {
        // Cap the walk at the total number of groups so a malformed
        // file with a cycle can't trap us here.
        let mut seen: HashSet<CrdtId> = HashSet::new();
        for _ in 0..visibility.len().max(1) {
            if !seen.insert(id) {
                return true;
            }
            if visibility.get(&id) == Some(&false) {
                return false;
            }
            match group_parent.get(&id) {
                Some(p) => id = *p,
                None => return true,
            }
        }
        true
    };

    let mut lines: Vec<&Line> = Vec::new();
    let mut texts: Vec<&Text> = Vec::new();
    for b in blocks {
        match b {
            Block::SceneLineItem(item) => {
                if item.item.deleted_length != 0 {
                    continue;
                }
                if !is_visible(item.parent_id) {
                    continue;
                }
                if let Some(line) = item.item.value.as_ref() {
                    lines.push(line);
                }
            }
            Block::SceneTextItem(item) => {
                if item.item.deleted_length != 0 {
                    continue;
                }
                if !is_visible(item.parent_id) {
                    continue;
                }
                if let Some(text) = item.item.value.as_ref() {
                    texts.push(text);
                }
            }
            _ => {}
        }
    }
    (lines, texts)
}

fn render_rm_to_ops(
    rm: &RemarkableFile,
    opaque_gs: &ExtendedGraphicsStateId,
    highlighter_gs: &ExtendedGraphicsStateId,
) -> Result<Vec<Op>, String> {
    let (lines, texts) = match rm {
        RemarkableFile::V6 { blocks, .. } => collect_v6_renderables(blocks),
        RemarkableFile::Other { .. } => {
            return Err("unsupported .rm version (only v6 strokes are rendered today)".into());
        }
    };

    if lines.is_empty() && texts.is_empty() {
        return Ok(vec![]);
    }

    // Fit the actual stroke bbox to A4 (with a small breathing margin),
    // preserving aspect ratio. Avoids reproducing the empty canvas the
    // user didn't write on — a notebook page where the user only wrote
    // in the bottom third would otherwise leave 2/3 of A4 blank.
    const PAGE_MARGIN_MM: f32 = 8.0;

    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for line in &lines {
        if !is_visible_tool(line.tool()) {
            continue;
        }
        for p in line.points() {
            min_x = min_x.min(p.x());
            max_x = max_x.max(p.x());
            min_y = min_y.min(p.y());
            max_y = max_y.max(p.y());
        }
    }
    // Also fold typed-text blocks into the bbox so a notebook that
    // contains only text doesn't collapse to a zero-area canvas
    // (and a mixed-content page widens the bbox correctly so the
    // text isn't clipped at the right edge).
    for text in &texts {
        let tx = text.x as f32;
        let ty = text.y as f32;
        let tw = text.width.max(0.0);
        min_x = min_x.min(tx);
        max_x = max_x.max(tx + tw);
        min_y = min_y.min(ty);
        // Reserve ~60 tablet-px of height per text block as a rough
        // line-height estimate. Better than collapsing the bbox to
        // a single point and then clipping the visible text.
        max_y = max_y.max(ty + 60.0);
    }
    if !min_x.is_finite() {
        return Ok(vec![]);
    }
    // Pad bbox by a fraction so strokes don't kiss the page edge.
    let pad = ((max_x - min_x).max(max_y - min_y)) * 0.02;
    min_x -= pad;
    max_x += pad;
    min_y -= pad;
    max_y += pad;
    let canvas_w = (max_x - min_x).max(1.0);
    let canvas_h = (max_y - min_y).max(1.0);

    // Aspect-preserving fit into the printable area.
    let printable_w_mm = PAGE_W_MM - 2.0 * PAGE_MARGIN_MM;
    let printable_h_mm = PAGE_H_MM - 2.0 * PAGE_MARGIN_MM;
    let scale_mm_per_px_w = printable_w_mm / canvas_w;
    let scale_mm_per_px_h = printable_h_mm / canvas_h;
    let scale_mm_per_px = scale_mm_per_px_w.min(scale_mm_per_px_h);
    let drawn_w_mm = canvas_w * scale_mm_per_px;

    // Horizontal: centre the bbox on the page (looks balanced for
    // notebook-style content that doesn't fill the width).
    // Vertical: top-anchor — the user's first written line should land
    // near the top of the A4, not floating in the middle. The flip
    // below means small canvas_y maps to high pdf_y_mm; we want
    // canvas_y=0 to land at PAGE_H_MM - PAGE_MARGIN_MM (near top).
    let offset_x_mm = (PAGE_W_MM - drawn_w_mm) / 2.0;
    let offset_y_mm = PAGE_MARGIN_MM;
    let _ = printable_h_mm; // referenced above for the height-bound branch

    // PDF y axis points up; ink y axis points down — flip on map.
    let map_xy = |x: f32, y: f32| -> Point {
        let canvas_x = x - min_x;
        let canvas_y = y - min_y;
        let pdf_x_mm = offset_x_mm + canvas_x * scale_mm_per_px;
        let pdf_y_mm = PAGE_H_MM - offset_y_mm - canvas_y * scale_mm_per_px;
        Point {
            x: Pt(mm_to_pt(pdf_x_mm)),
            y: Pt(mm_to_pt(pdf_y_mm)),
        }
    };

    // Default state for pen tools: round caps + round joins. The round
    // cap is a half-disc at each endpoint, so a stroked polyline looks
    // exactly like the bead-of-discs approach was trying to fake — but
    // composited correctly by PDF, with no winding artefacts.
    let mut ops: Vec<Op> = vec![
        Op::SetLineDashPattern {
            dash: LineDashPattern::default(),
        },
        Op::SetLineCapStyle {
            cap: LineCapStyle::Round,
        },
        Op::SetLineJoinStyle {
            join: LineJoinStyle::Round,
        },
        Op::LoadGraphicsState {
            gs: opaque_gs.clone(),
        },
    ];

    // Render highlighters first so they sit underneath ink.
    let mut ordered: Vec<&Line> = lines.clone();
    ordered.sort_by_key(|l| match l.tool() {
        Tool::Highlighter | Tool::Shader => 0,
        _ => 1,
    });

    // Track the current PDF graphics-state values so we only emit a
    // setter when something actually changes — keeps the content stream
    // small and readable.
    let mut last_rgb: Option<(f32, f32, f32)> = None;
    let mut last_width_pt: Option<f32> = None;
    let mut last_cap = LineCapStyle::Round;
    let mut last_gs_alpha: bool = false; // false = opaque, true = highlighter

    for line in &ordered {
        if !is_visible_tool(line.tool()) {
            continue;
        }
        let pts = line.points();
        if pts.is_empty() {
            continue;
        }

        let tool = line.tool();
        let is_highlighter = matches!(tool, Tool::Highlighter | Tool::Shader);

        // Switch to the highlighter alpha state only for highlighters,
        // and only when not already there.
        if is_highlighter != last_gs_alpha {
            ops.push(Op::LoadGraphicsState {
                gs: if is_highlighter {
                    highlighter_gs.clone()
                } else {
                    opaque_gs.clone()
                },
            });
            last_gs_alpha = is_highlighter;
        }

        // Highlighter uses square (butt) caps on the device; pens use
        // round caps so endpoints look like the brush tip.
        let want_cap = if is_highlighter {
            LineCapStyle::Butt
        } else {
            LineCapStyle::Round
        };
        if want_cap != last_cap {
            ops.push(Op::SetLineCapStyle { cap: want_cap });
            last_cap = want_cap;
        }

        let rgb = stroke_color_rgb(tool, line.color());
        if last_rgb != Some(rgb) {
            // Strokes use the *outline* colour, not fill.
            ops.push(Op::SetOutlineColor {
                col: Color::Rgb(Rgb {
                    r: rgb.0,
                    g: rgb.1,
                    b: rgb.2,
                    icc_profile: None,
                }),
            });
            last_rgb = Some(rgb);
        }

        let thickness = (line.thickness_scale() as f32).clamp(0.3, 3.0);

        // Pre-compute per-point widths (in pt) for reuse.
        let widths_pt: Vec<f32> = pts
            .iter()
            .map(|p| width_pt_for(tool, p, thickness))
            .collect();

        // Map all points into PDF space once.
        let mapped: Vec<Point> = pts.iter().map(|p| map_xy(p.x(), p.y())).collect();

        // For a single-sample stroke (just a tap), emit a zero-length
        // polyline so the round cap renders as a dot.
        if mapped.len() == 1 {
            emit_stroke_segment(
                &mut ops,
                std::slice::from_ref(&mapped[0]),
                widths_pt[0],
                &mut last_width_pt,
            );
            continue;
        }

        // Tools where width is essentially constant along the stroke can
        // be drawn as one polyline with a single SetOutlineThickness.
        // Variable-width tools are chunked into ~5-sample groups, each
        // stroked at the chunk's mean width — same approach as rmc/rmrl.
        let chunk_size = match tool {
            Tool::FineLiner | Tool::Marker | Tool::Highlighter | Tool::Shader => usize::MAX,
            _ => 5,
        };

        emit_chunked_strokes(
            &mut ops,
            &mapped,
            &widths_pt,
            chunk_size,
            &mut last_width_pt,
        );
    }

    // Typed-text pass. Runs after strokes so it sits on top of ink
    // (matches the tablet's z-order). We use the built-in Helvetica
    // — it ships embedded in every PDF reader, so no font subsetting
    // / licensing footwork. Style information from the device (BOLD,
    // HEADING, BULLET) is not yet honoured; for v1.0 we emit the
    // text content at the recorded position so it isn't silently
    // dropped, and call richer styling a follow-up.
    if !texts.is_empty() {
        let font = PdfFontHandle::Builtin(BuiltinFont::Helvetica);
        // Tablet "default" typed text is ~32 device-px tall. Scaled
        // through the bbox fit (same scale_mm_per_px the strokes
        // use) it maps to a reasonable on-page size.
        let font_size_pt = (32.0 * scale_mm_per_px * PT_PER_INCH / MM_PER_INCH).clamp(6.0, 36.0);
        ops.push(Op::StartTextSection);
        ops.push(Op::SetFont {
            font: font.clone(),
            size: Pt(font_size_pt),
        });
        // Default to black ink for typed text; reMarkable typed
        // text doesn't carry a per-block colour the way strokes do.
        ops.push(Op::SetFillColor {
            col: Color::Rgb(Rgb {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                icc_profile: None,
            }),
        });
        for text in &texts {
            // Reconstruct the visible text in CRDT insertion order.
            // `item_id` advances monotonically as the user types, so
            // sorting by it is a "good enough" reading order for
            // linearly-authored text; CRDT linked-list traversal
            // is overkill for v1.0.
            let mut items: Vec<_> = text.items.iter().collect();
            items.sort_by_key(|i| (i.item_id.part1, i.item_id.part2));
            let mut buf = String::new();
            for item in items {
                if item.deleted_length != 0 {
                    continue;
                }
                if let RmTextItem::Text(s) = &item.value {
                    buf.push_str(s);
                }
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Position the text at its recorded anchor (top-left of
            // the text block). PDF baseline anchors text from the
            // bottom of the glyph, so add the font cap-height back
            // so the visual top aligns with the recorded y.
            let anchor = map_xy(text.x as f32, text.y as f32);
            let baseline = Point {
                x: anchor.x,
                y: Pt(anchor.y.0 - font_size_pt * 0.8),
            };
            ops.push(Op::SetTextCursor { pos: baseline });
            // Split on newline-equivalents so multi-line typed text
            // visually wraps. printpdf treats `\n` as part of the
            // string verbatim, so we emit explicit line breaks.
            let lines_of_text: Vec<&str> = trimmed.split('\n').collect();
            for (i, line_str) in lines_of_text.iter().enumerate() {
                if i > 0 {
                    ops.push(Op::AddLineBreak);
                }
                ops.push(Op::ShowText {
                    items: vec![PdfTextItem::Text(line_str.to_string())],
                });
            }
        }
        ops.push(Op::EndTextSection);
    }

    Ok(ops)
}

/// Emit one stroked polyline at a single width.
fn emit_stroke_segment(
    ops: &mut Vec<Op>,
    pts: &[Point],
    width_pt: f32,
    last_width_pt: &mut Option<f32>,
) {
    if pts.is_empty() {
        return;
    }
    let w = width_pt.max(0.04);
    let needs_set = match last_width_pt {
        Some(prev) => (*prev - w).abs() > 0.005,
        None => true,
    };
    if needs_set {
        ops.push(Op::SetOutlineThickness { pt: Pt(w) });
        *last_width_pt = Some(w);
    }
    let line_pts: Vec<LinePoint> = pts
        .iter()
        .map(|p| LinePoint {
            p: *p,
            bezier: false,
        })
        .collect();
    // For a single-point stroke, duplicate it: PDF round caps draw a
    // disc at zero-length stroke endpoints, so the dot still appears.
    let line_pts = if line_pts.len() == 1 {
        vec![line_pts[0].clone(), line_pts[0].clone()]
    } else {
        line_pts
    };
    ops.push(Op::DrawLine {
        line: PdfLine {
            points: line_pts,
            is_closed: false,
        },
    });
}

/// Stroke a polyline as several overlapping sub-polylines, each at
/// the mean width of its samples. Adjacent chunks share an endpoint so
/// their round caps coincide and produce no visible seam.
fn emit_chunked_strokes(
    ops: &mut Vec<Op>,
    mapped: &[Point],
    widths: &[f32],
    chunk_size: usize,
    last_width_pt: &mut Option<f32>,
) {
    let n = mapped.len();
    if n < 2 {
        if n == 1 {
            emit_stroke_segment(ops, mapped, widths[0], last_width_pt);
        }
        return;
    }

    // chunk_size = MAX or larger than the polyline ⇒ one stroke at the
    // average of the whole polyline (constant-width tools).
    if chunk_size >= n {
        let avg = mean(widths);
        emit_stroke_segment(ops, mapped, avg, last_width_pt);
        return;
    }

    let mut start = 0usize;
    while start < n - 1 {
        let end = (start + chunk_size).min(n - 1);
        let avg = mean(&widths[start..=end]);
        emit_stroke_segment(ops, &mapped[start..=end], avg, last_width_pt);
        start = end;
    }
}

fn mean(xs: &[f32]) -> f32 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f32>() / (xs.len() as f32)
    }
}

fn is_visible_tool(tool: &Tool) -> bool {
    !matches!(
        tool,
        Tool::Eraser | Tool::EraseArea | Tool::EraseAll | Tool::SelectionBrush
    )
}

/// Per-point rendered width, in PDF points (1 pt = 1/72 inch).
///
/// reMarkable point fields are roughly:
/// - `pressure`: 0..255 (u8 in v2 format, float×255 in v1 format)
/// - `speed`: pixel-distance per sample, typically 0..200
/// - `width`: tool-specific multiplier, u16 in v2 (typical 0..2000-ish)
///
/// Width formulas adapted from open-source rm renderers (rm2svg.py,
/// Nemoworld) and hand-tuned for the look of the reMarkable 2.
fn width_pt_for(tool: &Tool, p: &RmPoint, thickness: f32) -> f32 {
    let pressure_norm = (p.pressure() / 255.0).clamp(0.0, 1.0);
    let speed = p.speed().max(0.0);
    // The raw `width` field is a tool-internal multiplier. v1 stores it
    // as f32×4, v2 stores it as a u16. Empirically values cluster around
    // a few hundred; normalise toward 1.0 so the per-tool base width does
    // most of the work and the multiplier just adds variation.
    let raw_width = p.width();
    let width_mult = if raw_width > 0.0 {
        // log-soft normalisation: maps 30→0.5, 300→1.0, 3000→2.0 roughly.
        (raw_width / 300.0).clamp(0.3, 3.0)
    } else {
        1.0
    };

    let base_mm = match tool {
        Tool::BallPoint => 0.40,
        Tool::FineLiner => 0.35,
        Tool::Marker => 1.00,
        Tool::Brush => 0.85,
        Tool::Pencil => 0.32,
        Tool::MechanicalPencil => 0.22,
        Tool::Calligraphy => 1.00,
        Tool::Highlighter | Tool::Shader => 4.50,
        _ => 0.40,
    };

    let pressure_curve = match tool {
        Tool::BallPoint => pressure_norm.powf(1.4) * 0.7 + 0.3, // 30%..100%
        Tool::Brush => pressure_norm.powf(1.5) * 0.8 + 0.2,     // 20%..100%
        Tool::Pencil => pressure_norm.sqrt() * 0.6 + 0.4,       // 40%..100%, gentle
        Tool::MechanicalPencil => pressure_norm * 0.3 + 0.7,    // mostly fixed
        Tool::Calligraphy => pressure_norm * 0.6 + 0.4,
        Tool::FineLiner | Tool::Marker | Tool::Highlighter | Tool::Shader => 1.0,
        _ => 1.0,
    };

    // Speed thinning: ballpoints and brushes get noticeably narrower when
    // drawn fast (high speed). Other pens are speed-insensitive.
    let speed_factor = match tool {
        Tool::BallPoint => 1.0 / (1.0 + speed * 0.005),
        Tool::Brush => 1.0 / (1.0 + speed * 0.003),
        _ => 1.0,
    };

    let mm = base_mm * thickness * pressure_curve * speed_factor * width_mult;
    mm_to_pt(mm.max(0.04))
}

fn stroke_color_rgb(tool: &Tool, color: &PenColor) -> (f32, f32, f32) {
    match tool {
        // Highlighter colours are the saturated source RGB; the actual
        // translucency is applied via the highlighter ExtGState (alpha
        // ≈ 0.39, matching rmrl). The classic device yellow is rgb
        // (1.0, 0.914, 0.290) — anything paler reads as washed-out
        // when laid over white through the alpha blend.
        Tool::Highlighter | Tool::Shader => match color {
            PenColor::Yellow => (1.000, 0.914, 0.290),
            PenColor::Green => (0.482, 0.871, 0.420),
            PenColor::Pink => (0.969, 0.408, 0.671),
            PenColor::Blue => (0.341, 0.612, 0.953),
            PenColor::Red => (0.949, 0.341, 0.341),
            _ => (1.000, 0.914, 0.290),
        },
        Tool::Pencil | Tool::MechanicalPencil => {
            // Bias hard toward graphite-grey regardless of nominal colour.
            let base = pen_color_rgb(color);
            let r = base.0 * 0.30 + 0.55;
            let g = base.1 * 0.30 + 0.55;
            let b = base.2 * 0.30 + 0.55;
            (r.min(1.0), g.min(1.0), b.min(1.0))
        }
        _ => pen_color_rgb(color),
    }
}

fn pen_color_rgb(color: &PenColor) -> (f32, f32, f32) {
    match color {
        PenColor::Black => (0.00, 0.00, 0.00),
        PenColor::Grey => (0.50, 0.50, 0.50),
        PenColor::GreyOverlap => (0.55, 0.55, 0.55),
        PenColor::White => (1.00, 1.00, 1.00),
        PenColor::Yellow => (0.95, 0.85, 0.00),
        PenColor::Green => (0.00, 0.65, 0.31),
        PenColor::Pink => (0.94, 0.42, 0.65),
        PenColor::Blue => (0.00, 0.40, 0.85),
        PenColor::Red => (0.85, 0.15, 0.15),
        PenColor::Unknown(_) => (0.00, 0.00, 0.00),
    }
}

/// Build a multi-page PDF from PNG byte buffers. Used as a fallback when
/// a notebook has no parseable `.rm` ink files. Pages are A4 portrait;
/// thumbnails are scaled to fit while preserving aspect ratio.
pub fn build_pdf_from_pngs(title: &str, pages: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    if pages.is_empty() {
        return Err("notebook has no thumbnail pages to render".to_string());
    }

    let mut doc = PdfDocument::new(title);
    let mut warnings = Vec::new();
    let mut pdf_pages = Vec::with_capacity(pages.len());

    let page_w_in = PAGE_W_MM / MM_PER_INCH;
    let page_h_in = PAGE_H_MM / MM_PER_INCH;
    let page_w_pt = mm_to_pt(PAGE_W_MM);
    let page_h_pt = mm_to_pt(PAGE_H_MM);

    for (i, png_bytes) in pages.iter().enumerate() {
        let dims = ImageReader::new(std::io::Cursor::new(png_bytes))
            .with_guessed_format()
            .map_err(|e| format!("page {i}: {e}"))?
            .into_dimensions()
            .map_err(|e| format!("page {i} dimensions: {e}"))?;

        let raw = RawImage::decode_from_bytes(png_bytes, &mut warnings)
            .map_err(|e| format!("page {i} decode: {e}"))?;
        let xobject_id = doc.add_image(&raw);

        let dpi_for_width = dims.0 as f32 / page_w_in;
        let dpi_for_height = dims.1 as f32 / page_h_in;
        let dpi = dpi_for_width.max(dpi_for_height);

        let drawn_w_in = dims.0 as f32 / dpi;
        let drawn_h_in = dims.1 as f32 / dpi;
        let drawn_w_pt = drawn_w_in * PT_PER_INCH;
        let drawn_h_pt = drawn_h_in * PT_PER_INCH;
        let translate_x_pt = (page_w_pt - drawn_w_pt) / 2.0;
        let translate_y_pt = (page_h_pt - drawn_h_pt) / 2.0;

        let page = PdfPage::new(
            Mm(PAGE_W_MM),
            Mm(PAGE_H_MM),
            vec![Op::UseXobject {
                id: xobject_id,
                transform: XObjectTransform {
                    translate_x: Some(Pt(translate_x_pt)),
                    translate_y: Some(Pt(translate_y_pt)),
                    scale_x: None,
                    scale_y: None,
                    rotate: None,
                    dpi: Some(dpi),
                },
            }],
        );
        pdf_pages.push(page);
    }

    Ok(doc
        .with_pages(pdf_pages)
        .save(&PdfSaveOptions::default(), &mut warnings))
}
