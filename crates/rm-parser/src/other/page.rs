use crate::{bitreader::Readable, ParseError};

use super::{layer::Layer, Parse};

#[derive(Debug)]
pub struct Page {
    pub layers: Vec<Layer>,
}

impl Parse for Page {
    fn parse(
        version: u32,
        reader: &mut crate::Bitreader<impl Readable>,
    ) -> Result<Self, crate::ParseError> {
        let amount_layers = reader.read_u32()?;

        // DoS guard (issue #34). The natural `(0..amount_layers).map(...).collect()`
        // shape calls `Vec::with_capacity(amount_layers)` via `Range<u32>`'s
        // exact size_hint *before* the inner parses run, so a hostile or
        // truncated file with `amount_layers = u32::MAX` triggers a multi-GB
        // allocation before the first child Layer fails on EOF. Cap the
        // pre-allocation against what the remaining stream can possibly
        // supply (a Layer is at least 4 bytes — its own amount_lines u32)
        // and grow the vec via push so the inner parse failures kill the
        // loop before any real memory is committed. The matching v6 guard
        // lives in `Bitreader::read_bytes`.
        const LAYER_MIN_SIZE: usize = 4;
        let cap = (amount_layers as usize).min(reader.remaining() / LAYER_MIN_SIZE);
        let mut layers = Vec::with_capacity(cap);
        for _ in 0..amount_layers {
            layers.push(Layer::parse(version, reader)?);
        }
        Ok(Page { layers })
    }
}
