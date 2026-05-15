use crate::{bitreader::Readable, ParseError};

use super::{line::Line, Parse};

#[derive(Debug)]
pub struct Layer {
    pub lines: Vec<Line>,
}

impl Parse for Layer {
    fn parse(
        version: u32,
        reader: &mut crate::Bitreader<impl Readable>,
    ) -> Result<Self, crate::ParseError> {
        let amount_lines = reader.read_u32()?;

        // DoS guard (issue #34). See `page.rs` for the full rationale.
        // A Line is at least 20 bytes (tool/color/unknown/brush_size/
        // amount_points u32s + brush_size f32); cap pre-allocation
        // against what the stream could possibly hold.
        const LINE_MIN_SIZE: usize = 20;
        let cap = (amount_lines as usize).min(reader.remaining() / LINE_MIN_SIZE);
        let mut lines = Vec::with_capacity(cap);
        for _ in 0..amount_lines {
            lines.push(Line::parse(version, reader)?);
        }
        Ok(Layer { lines })
    }
}
