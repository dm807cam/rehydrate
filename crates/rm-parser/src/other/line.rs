use crate::bitreader::Readable;
use crate::shared::pen_color::PenColor;
use crate::shared::tool::Tool;
use crate::ParseError;

use super::point::Point;
use super::Parse;

#[derive(Debug)]
pub struct Line {
    pub points: Vec<Point>,
    pub tool: Tool,
    pub color: PenColor,
    pub brush_size: f32,
}

impl Parse for Line {
    fn parse(
        version: u32,
        reader: &mut crate::Bitreader<impl Readable>,
    ) -> Result<Self, crate::ParseError> {
        let tool = Tool::try_from(reader.read_u32()?)?;
        let color = PenColor::try_from(reader.read_u32()?)?;
        reader.read_u32()?; // Skip unknown value
        let brush_size = reader.read_f32()?;
        if version >= 5 {
            reader.read_u32()?; // Skip unkown value
        }
        let amount_points = reader.read_u32()?;

        // DoS guard (issue #34). See `page.rs` for the full rationale.
        // A Point is 6 × f32 = 24 bytes; cap pre-allocation against
        // what the stream could possibly hold so a malicious
        // `amount_points = u32::MAX` doesn't trigger a ~100 GB
        // Vec::with_capacity before the first read_f32 fails on EOF.
        const POINT_SIZE: usize = 24;
        let cap = (amount_points as usize).min(reader.remaining() / POINT_SIZE);
        let mut points = Vec::with_capacity(cap);
        for _ in 0..amount_points {
            points.push(Point {
                x: reader.read_f32()?,
                y: reader.read_f32()?,
                speed: reader.read_f32()?,
                direction: reader.read_f32()?,
                width: reader.read_f32()?,
                pressure: reader.read_f32()?,
            });
        }

        Ok(Line {
            tool,
            color,
            brush_size,
            points,
        })
    }
}
