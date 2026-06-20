use crate::ParseError;

/// Data representation of an exported color in a reMarkable document line.
/// `Unknown` exists because newer firmware adds colour codes the parser
/// doesn't recognise; we keep the raw value so callers can decide how to
/// render rather than failing the whole file.
#[derive(Debug, Clone)]
pub enum PenColor {
    Black,
    Grey,
    White,
    Yellow,
    Green,
    Pink,
    Blue,
    Red,
    GreyOverlap,
    /// Highlighter sentinel (code 9). The actual highlight tint is carried
    /// separately in the line's optional `color_rgba`; renderers that don't
    /// read that field treat this as a generic highlighter colour.
    Highlight,
    /// Second green (code 10) — a reMarkable Paper Pro palette colour absent
    /// from the original RM2 set. It is a common pen, and before this entry
    /// it fell through to `Unknown(10)` and rendered black.
    Green2,
    Cyan,
    Magenta,
    /// Second yellow (code 13), Paper Pro palette.
    Yellow2,
    Unknown(u32),
}

impl TryFrom<u32> for PenColor {
    type Error = ParseError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        // Codes 0..=8 are the original RM2 palette; 9..=13 were added with
        // the colour-capable reMarkable Paper Pro. Values match rmscene's
        // `PenColor` enum.
        Ok(match value {
            0x00 => PenColor::Black,
            0x01 => PenColor::Grey,
            0x02 => PenColor::White,
            0x03 => PenColor::Yellow,
            0x04 => PenColor::Green,
            0x05 => PenColor::Pink,
            0x06 => PenColor::Blue,
            0x07 => PenColor::Red,
            0x08 => PenColor::GreyOverlap,
            0x09 => PenColor::Highlight,
            0x0A => PenColor::Green2,
            0x0B => PenColor::Cyan,
            0x0C => PenColor::Magenta,
            0x0D => PenColor::Yellow2,
            other => PenColor::Unknown(other),
        })
    }
}
