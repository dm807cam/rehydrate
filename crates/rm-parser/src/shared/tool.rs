use crate::ParseError;

#[derive(Debug, Clone)]
pub enum Tool {
    Brush,
    Pencil,
    BallPoint,
    Marker,
    FineLiner,
    Highlighter,
    Eraser,
    MechanicalPencil,
    EraseArea,
    EraseAll,
    SelectionBrush,
    Calligraphy,
    /// reMarkable Paper Pro "shader" — a wide, translucent shading tool.
    /// Rendered like the highlighter (constant width, ~0.39 alpha, drawn
    /// under the ink).
    Shader,
    /// Unrecognised tool code. Newer firmware introduces tool variants
    /// (Paintbrush, etc.) that we don't model yet; preserving the raw
    /// value lets the renderer fall back to a generic ink stroke.
    Unknown(u32),
}

impl TryFrom<u32> for Tool {
    type Error = ParseError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(match value {
            0x00 | 0x0c => Tool::Brush,
            0x01 | 0x0e => Tool::Pencil,
            0x02 | 0x0f => Tool::BallPoint,
            0x03 | 0x10 => Tool::Marker,
            0x04 | 0x11 => Tool::FineLiner,
            0x05 | 0x12 => Tool::Highlighter,
            0x06 => Tool::Eraser,
            0x07 | 0x0d => Tool::MechanicalPencil,
            0x08 => Tool::EraseArea,
            0x09 => Tool::EraseAll,
            0x0a | 0x0b => Tool::SelectionBrush,
            0x15 => Tool::Calligraphy,
            0x17 => Tool::Shader,
            other => Tool::Unknown(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_shader_tool_code() {
        // 0x17 is the Paper Pro shader; before this it fell through to
        // `Unknown(0x17)` and rendered as opaque generic ink.
        assert!(matches!(Tool::try_from(0x17).unwrap(), Tool::Shader));
    }

    #[test]
    fn preserves_unknown_tool_codes() {
        assert!(matches!(Tool::try_from(0x99).unwrap(), Tool::Unknown(0x99)));
    }
}
