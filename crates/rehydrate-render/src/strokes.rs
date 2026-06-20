//! v6 scene-walk + per-stroke geometry helpers shared between the
//! `rm_to_pdf` (printpdf vector path) and `overlay` (raw content-stream
//! path) modules. Both consume the same parsed `.rm` model, so the
//! visibility walk, the colour table, and the per-point width formulae
//! belong in exactly one place.

use std::collections::{HashMap, HashSet};

use rm_parser::shared::{pen_color::PenColor, tool::Tool};
use rm_parser::v6::block::Block;
use rm_parser::v6::crdt::CrdtId;
use rm_parser::v6::scene_item::line::Line;
use rm_parser::v6::scene_item::point::Point as RmPoint;
use rm_parser::v6::scene_item::text::Text;

use crate::dims::mm_to_pt;

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
pub(crate) fn collect_v6_renderables(blocks: &[Block]) -> (Vec<&Line>, Vec<&Text>) {
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

pub(crate) fn mean(xs: &[f32]) -> f32 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f32>() / (xs.len() as f32)
    }
}

pub(crate) fn is_visible_tool(tool: &Tool) -> bool {
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
pub(crate) fn width_pt_for(tool: &Tool, p: &RmPoint, thickness: f32) -> f32 {
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

pub(crate) fn stroke_color_rgb(tool: &Tool, color: &PenColor) -> (f32, f32, f32) {
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

pub(crate) fn pen_color_rgb(color: &PenColor) -> (f32, f32, f32) {
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
        // reMarkable Paper Pro palette additions (codes 9..=13). Saturated
        // export tints in the same spirit as the RM2 colours above — the
        // device's on-eink renderings are far more muted, but the official
        // PDF export uses vivid ink, so we match that.
        PenColor::Green2 => (0.13, 0.60, 0.27),
        PenColor::Cyan => (0.00, 0.62, 0.80),
        PenColor::Magenta => (0.80, 0.15, 0.55),
        PenColor::Yellow2 => (0.95, 0.82, 0.00),
        // A bare `Highlight` colour outside the highlighter tool is rare;
        // fall back to the classic highlighter yellow.
        PenColor::Highlight => (1.00, 0.85, 0.00),
        PenColor::Unknown(_) => (0.00, 0.00, 0.00),
    }
}
