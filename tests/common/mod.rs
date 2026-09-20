#![allow(dead_code)]
use bfw::migration::{Binding, RuleDoc, Source};
use bfw::model::*;
use bfw::store::SampleDoc;
use serde_json::Value;
use std::collections::BTreeMap;

fn int(name: &str, int: IntType, offset: Option<usize>) -> Item {
    Item::Int(IntField { name: name.into(), int, offset, description: None })
}

pub fn v1() -> FormatDoc {
    FormatDoc {
        id: "img".into(), version: 1, inherits: None, endian: Endian::Little,
        magic: vec![Magic { offset: 0, hex: "494d5831".into() }],
        description: "v1".into(),
        layout: vec![
            int("width", IntType::U16, Some(4)),
            int("height", IntType::U16, None),
            Item::Bits(BitsField {
                name: "flags".into(), int: IntType::U8,
                parts: vec![
                    BitPart { name: "alpha".into(), lsb: 0, bits: 1, description: None },
                    BitPart { name: "kind".into(), lsb: 1, bits: 2, description: None },
                ],
                offset: None,
            }),
            Item::Align(AlignField { name: "pad".into(), boundary: 4, pad: PadKind::Zero }),
            int("comment_len", IntType::U8, None),
            Item::Bytes(BytesField { name: "comment".into(), len: Len::Field { path: "comment_len".into() }, offset: None }),
            Item::Checksum(ChecksumField { name: "crc".into(), int: IntType::U32, algo: Algo::Crc32, range: ChkRange::default(), offset: None }),
        ],
    }
}

pub fn v2() -> FormatDoc {
    FormatDoc {
        id: "img".into(), version: 2, inherits: None, endian: Endian::Little,
        magic: vec![Magic { offset: 0, hex: "494d5831".into() }],
        description: "v2".into(),
        layout: vec![
            int("width", IntType::U16, Some(4)),
            int("height", IntType::U16, None),
            Item::Bits(BitsField {
                name: "flags".into(), int: IntType::U8,
                parts: vec![
                    BitPart { name: "alpha".into(), lsb: 0, bits: 1, description: None },
                    BitPart { name: "kind".into(), lsb: 1, bits: 2, description: None },
                ],
                offset: None,
            }),
            int("fps", IntType::U8, None),
            Item::Align(AlignField { name: "pad".into(), boundary: 4, pad: PadKind::Zero }),
            Item::Checksum(ChecksumField { name: "crc".into(), int: IntType::U32, algo: Algo::Crc32, range: ChkRange::default(), offset: None }),
        ],
    }
}

pub fn rule() -> RuleDoc {
    RuleDoc {
        id: "r".into(), version: 1,
        from: Ref { id: "img".into(), version: 1 },
        to: Ref { id: "img".into(), version: 2 },
        bindings: vec![
            Binding { target: "width".into(), source: Source::Field { path: "width".into() }, note: "".into() },
            Binding { target: "height".into(), source: Source::Field { path: "height".into() }, note: "".into() },
            Binding { target: "flags.alpha".into(), source: Source::Field { path: "flags.alpha".into() }, note: "".into() },
            Binding { target: "flags.kind".into(), source: Source::Field { path: "flags.kind".into() }, note: "".into() },
            Binding { target: "fps".into(), source: Source::Constant { value: 30 }, note: "".into() },
        ],
        description: "".into(),
    }
}

pub struct Lookup(pub Vec<FormatDoc>);
impl FormatLookup for Lookup {
    fn get(&self, id: &str, version: u32) -> Option<&FormatDoc> {
        self.0.iter().find(|d| d.id == id && d.version == version)
    }
}

pub fn sample_bytes() -> Vec<u8> {
    let mut d: Vec<u8> = vec![0x49, 0x4d, 0x58, 0x31, 64, 0, 48, 0, 0b0110, 0, 0, 0, 2, b'h', b'i'];
    let crc = bfw::util::crc32_ieee(&d) as u32;
    d.extend_from_slice(&crc.to_le_bytes());
    d
}

pub fn sample_doc() -> SampleDoc {
    SampleDoc {
        id: "s".into(), rev: 0,
        format: Ref { id: "img".into(), version: 1 },
        name: "s".into(), hex: bfw::util::to_hex(&sample_bytes()),
        note: "".into(), derived_from: None,
    }
}

#[allow(dead_code)]
pub fn empty_map() -> BTreeMap<String, Value> { BTreeMap::new() }
