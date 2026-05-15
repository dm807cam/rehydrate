use std::collections::HashMap;

use crate::{
    bitreader::Readable,
    v6::{
        crdt::{CrdtId, CrdtSequenceItem},
        scene_item::{group::Group, text::Text},
        tagged_bit_reader::TaggedBitreader,
        TypeParse,
    },
    ParseError,
};

use super::{BlockInfo, BlockParse};

#[derive(Debug, Clone)]
pub struct MigrationInfoBlock {
    pub migration_id: CrdtId,
    pub is_device: bool,
}
impl BlockParse for MigrationInfoBlock {
    fn parse(
        info: &BlockInfo,
        reader: &mut TaggedBitreader<impl Readable>,
    ) -> Result<Self, ParseError> {
        let migration_id = reader.read_id(1)?;

        let is_device = reader.read_u8(2)? > 0;

        if info.has_bytes_remaining(&mut reader.bit_reader) {
            _ = reader.bit_reader.read_u8();
        }
        Ok(Self {
            migration_id,
            is_device,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AuthorsIdsBlock {
    pub authors: HashMap<u16, String>,
}
impl BlockParse for AuthorsIdsBlock {
    fn parse(
        _info: &BlockInfo,
        reader: &mut TaggedBitreader<impl Readable>,
    ) -> Result<Self, ParseError> {
        let amount_subblocks = reader.bit_reader.read_varuint()?;
        let mut authors = HashMap::new();

        for _ in 0..amount_subblocks {
            let block = reader.read_subblock(0)?;

            let uuid = reader.bit_reader.read_uuid()?;

            let author_id = reader.bit_reader.read_u16()?;
            authors.insert(author_id, uuid);
            block.validate_size(reader)?;
        }

        Ok(Self { authors })
    }
}

#[derive(Debug, Clone)]
pub struct PageInfoBlock {
    pub loads_count: u32,
    pub merges_count: u32,
    pub text_chars_count: u32,
    pub text_lines_count: u32,
}
impl BlockParse for PageInfoBlock {
    fn parse(
        info: &BlockInfo,
        reader: &mut TaggedBitreader<impl Readable>,
    ) -> Result<Self, ParseError> {
        let loads_count = reader.read_u32(1)?;
        let merges_count = reader.read_u32(2)?;
        let text_chars_count = reader.read_u32(3)?;
        let text_lines_count = reader.read_u32(4)?;

        if info.has_bytes_remaining(&mut reader.bit_reader) {
            reader.read_u32(5)?;
        }

        Ok(Self {
            loads_count,
            merges_count,
            text_chars_count,
            text_lines_count,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TreeNodeBlock {
    pub group: Group,
}
impl BlockParse for TreeNodeBlock {
    fn parse(
        info: &BlockInfo,
        reader: &mut TaggedBitreader<impl Readable>,
    ) -> Result<Self, ParseError> {
        let mut group = Group::default();
        group.node_id = reader.read_id(1)?;
        group.label = reader.read_lww_string(2)?;
        group.visible = reader.read_lww_bool(3)?;

        if info.has_bytes_remaining(&reader.bit_reader) {
            group.anchor_id = Some(reader.read_lww_id(7)?);
            group.anchor_type = Some(reader.read_lww_u8(8)?);
            group.anchor_threshold = Some(reader.read_lww_f32(9)?);
            group.anchor_origin_x = Some(reader.read_lww_f32(10)?);
        }

        Ok(Self { group })
    }
}

#[derive(Debug, Clone)]
pub struct SceneTreeBlock {
    pub tree_id: CrdtId,
    pub node_id: CrdtId,
    pub is_update: bool,
    pub parent_id: CrdtId,
}
impl BlockParse for SceneTreeBlock {
    fn parse(
        _info: &BlockInfo,
        reader: &mut TaggedBitreader<impl Readable>,
    ) -> Result<Self, ParseError> {
        let tree_id = reader.read_id(1)?;
        let node_id = reader.read_id(2)?;
        let is_update = reader.read_bool(3)?;

        let subblock = reader.read_subblock(4)?;
        let parent_id = reader.read_id(1)?;
        subblock.validate_size(reader)?;

        Ok(Self {
            tree_id,
            node_id,
            is_update,
            parent_id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RootTextBlock {
    pub block_id: CrdtId,
    pub text: Text,
}
impl BlockParse for RootTextBlock {
    fn parse(
        _info: &BlockInfo,
        reader: &mut TaggedBitreader<impl Readable>,
    ) -> Result<Self, ParseError> {
        let block_id = reader.read_id(1)?;

        Ok(RootTextBlock {
            block_id,
            text: Text::parse(reader)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SceneItemType {
    SceneGlyphItemBlock,
    SceneGroupItemBlock,
    SceneLineItemBlock,
    SceneTextItemBlock,
    /// Inner subtype tag introduced by newer firmware that this
    /// parser doesn't recognise (rM Paper Pro adds them; rmscene's
    /// README still calls v6 "being reverse engineered"). The
    /// block-level dispatch already returns `Block::Unknown` for
    /// unrecognised outer block_types; this variant extends the
    /// same courtesy one layer down so that a single new inner
    /// tag inside a recognised outer block doesn't kill the
    /// entire file parse (issue #35).
    Unknown(u8),
}
impl SceneItemType {
    pub fn validate(self, scene_item_type: SceneItemType) -> Result<(), ParseError> {
        // Forward-compat: an Unknown subtype is the parser's "I don't
        // model this yet" signal — the caller skips the rest of the
        // subblock and yields a value-less item, so there's nothing
        // here to validate against the expected variant. Without this
        // short-circuit, validate would reject every Unknown as a
        // type mismatch and re-introduce the whole-file failure mode
        // the Unknown variant exists to avoid.
        if matches!(self, SceneItemType::Unknown(_)) {
            return Ok(());
        }
        if self != scene_item_type {
            return Err(ParseError::invalid(format!(
                "Invalid scene item type given '{:?}' expected '{:?}'",
                self, scene_item_type
            )));
        }

        Ok(())
    }
}
impl TryFrom<u8> for SceneItemType {
    type Error = ParseError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => SceneItemType::SceneGlyphItemBlock,
            2 => SceneItemType::SceneGroupItemBlock,
            3 => SceneItemType::SceneLineItemBlock,
            5 => SceneItemType::SceneTextItemBlock,
            other => SceneItemType::Unknown(other),
        })
    }
}
#[derive(Debug, Clone)]
pub struct SceneItemBlock<N> {
    pub parent_id: CrdtId,
    pub item: CrdtSequenceItem<Option<N>>,
}
impl<N> SceneItemBlock<N> {
    pub fn parse<R: Readable>(
        info: &BlockInfo,
        reader: &mut TaggedBitreader<R>,
        scene_item_type: SceneItemType,
        get_value: fn(&BlockInfo, &mut TaggedBitreader<R>) -> Result<N, ParseError>,
    ) -> Result<Self, ParseError> {
        let parent_id = reader.read_id(1)?;
        let item_id = reader.read_id(2)?;
        let left_id = reader.read_id(3)?;
        let right_id = reader.read_id(4)?;
        let deleted_length = reader.read_u32(5)?;

        let value = if reader.has_subblock(6)? {
            let subblock = reader.read_subblock(6)?;
            let subtype = SceneItemType::try_from(reader.bit_reader.read_u8()?)?;
            if matches!(subtype, SceneItemType::Unknown(_)) {
                // Newer-firmware inner subtype — we can't decode the
                // payload, but `validate_size` is exactly the
                // skip-trailing-tail mechanic that `Block` uses for
                // unknown outer block_types. Drop the value and let
                // the CRDT positioning fields (parent_id, item_id,
                // left/right) still carry through.
                subblock.validate_size(reader)?;
                None
            } else {
                subtype.validate(scene_item_type)?;
                let value = get_value(info, reader)?;
                subblock.validate_size(reader)?;
                Some(value)
            }
        } else {
            None
        };

        Ok(SceneItemBlock {
            parent_id,
            item: CrdtSequenceItem {
                left_id,
                item_id,
                right_id,
                deleted_length,
                value,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #35 regression. The recognised tags must still round-trip
    /// to their named variants — the Unknown(_) fallthrough must NOT
    /// swallow them. A regression that changed the match-arm order or
    /// dropped a known discriminant would surface here as a
    /// known-tag-now-Unknown false positive.
    #[test]
    fn known_subtype_tags_decode_to_named_variants() {
        assert_eq!(
            SceneItemType::try_from(1).unwrap(),
            SceneItemType::SceneGlyphItemBlock,
        );
        assert_eq!(
            SceneItemType::try_from(2).unwrap(),
            SceneItemType::SceneGroupItemBlock,
        );
        assert_eq!(
            SceneItemType::try_from(3).unwrap(),
            SceneItemType::SceneLineItemBlock,
        );
        assert_eq!(
            SceneItemType::try_from(5).unwrap(),
            SceneItemType::SceneTextItemBlock,
        );
    }

    /// Issue #35: a subtype tag not in {1, 2, 3, 5} used to abort the
    /// whole file parse — rM Paper Pro and newer xochitl firmware
    /// have already started introducing new inner tags. The new
    /// contract is that try_from yields `Unknown(tag)` and the caller
    /// (`SceneItemBlock::parse`) skips the rest of the subblock.
    #[test]
    fn novel_subtype_tag_becomes_unknown_not_error() {
        for novel in [0u8, 4, 6, 7, 42, 99, 255] {
            assert_eq!(
                SceneItemType::try_from(novel).unwrap(),
                SceneItemType::Unknown(novel),
                "tag {novel} must decode to Unknown, not Err",
            );
        }
    }

    /// Issue #35: `validate` against the expected outer block type
    /// is the strict format check — a mismatch between two recognised
    /// variants is a real corruption signal and must still fail. The
    /// Unknown wildcard is only meant to bypass validation for
    /// genuinely-novel tags, not to relax checks on known ones.
    #[test]
    fn known_subtype_mismatch_still_rejected() {
        let err = SceneItemType::SceneGlyphItemBlock
            .validate(SceneItemType::SceneGroupItemBlock)
            .expect_err("known-vs-known mismatch must error");
        assert!(
            err.to_string().contains("Invalid scene item type"),
            "expected the strict-mismatch error, got: {err}",
        );
    }

    /// Issue #35: an Unknown subtype is the "forward-compat fall through"
    /// signal; calling validate on it must NOT propagate a mismatch
    /// error, regardless of which named variant the outer block was
    /// expecting. SceneItemBlock::parse relies on this to keep the
    /// rest of the file readable.
    #[test]
    fn unknown_subtype_validate_is_a_no_op() {
        for expected in [
            SceneItemType::SceneGlyphItemBlock,
            SceneItemType::SceneGroupItemBlock,
            SceneItemType::SceneLineItemBlock,
            SceneItemType::SceneTextItemBlock,
        ] {
            SceneItemType::Unknown(99)
                .validate(expected)
                .expect("Unknown subtype must validate-as-Ok against any expected variant");
        }
    }
}
