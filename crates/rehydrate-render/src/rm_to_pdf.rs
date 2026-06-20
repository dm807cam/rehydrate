//! `.rm` v6 → A4 PDF rendering. Re-emits each stroke as one or more
//! stroked polylines using PDF's native stroking pipeline (round caps
//! and joins, alpha via ExtGState for highlighters). Typed text is
//! drawn on top of strokes with the built-in Helvetica font.

use printpdf::{
    BuiltinFont, Color, ExtendedGraphicsState, ExtendedGraphicsStateId, Line as PdfLine,
    LineCapStyle, LineDashPattern, LineJoinStyle, LinePoint, Mm, Op, PdfDocument, PdfFontHandle,
    PdfPage, PdfSaveOptions, Point, Pt, Rgb, TextItem as PdfTextItem,
};
use rm_parser::shared::tool::Tool;
use rm_parser::v6::scene_item::text::TextItem as RmTextItem;
use rm_parser::RemarkableFile;

use crate::dims::{mm_to_pt, MM_PER_INCH, PAGE_H_MM, PAGE_W_MM, PT_PER_INCH};
use crate::strokes::{
    collect_v6_renderables, is_visible_tool, mean, stroke_color_rgb, width_pt_for,
};

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
    let mut ordered: Vec<_> = lines.clone();
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
