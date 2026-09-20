use crate::migration::{Binding, RuleDoc, Source};
use crate::model::*;
use crate::store::{SampleDoc, Store, StoreError};

fn u16le(name: &str, offset: Option<usize>) -> Item {
    Item::Int(IntField {
        name: name.into(),
        int: IntType::U16,
        offset,
        description: None,
    })
}

fn u8(name: &str, offset: Option<usize>) -> Item {
    Item::Int(IntField {
        name: name.into(),
        int: IntType::U8,
        offset,
        description: None,
    })
}

pub fn seed(store: &Store) -> Result<(), StoreError> {
    let state = store.snapshot();
    if !state.formats.is_empty() {
        return Ok(());
    }
    drop(state);

    let v1 = FormatDoc {
        id: "img".into(),
        version: 1,
        inherits: None,
        endian: Endian::Little,
        magic: vec![Magic { offset: 0, hex: "494d5831".into() }],
        description: "Legacy IMX image container".into(),
        layout: vec![
            u16le("width", Some(4)),
            u16le("height", None),
            Item::Bits(BitsField {
                name: "flags".into(),
                int: IntType::U8,
                parts: vec![
                    BitPart { name: "alpha".into(), lsb: 0, bits: 1, description: None },
                    BitPart { name: "kind".into(), lsb: 1, bits: 2, description: None },
                ],
                offset: None,
            }),
            Item::Align(AlignField { name: "pad".into(), boundary: 4, pad: PadKind::Zero }),
            u8("comment_len", None),
            Item::Bytes(BytesField {
                name: "comment".into(),
                len: Len::Field { path: "comment_len".into() },
                offset: None,
            }),
            Item::Checksum(ChecksumField {
                name: "crc".into(),
                int: IntType::U32,
                algo: Algo::Crc32,
                range: ChkRange::default(),
                offset: None,
            }),
        ],
    };
    store.upsert_format(v1, None).map(|_| ())?;

    let mut data: Vec<u8> = vec![
        0x49, 0x4d, 0x58, 0x31,
        64, 0,
        48, 0,
        0b0000_0110,
        0, 0, 0,
        2,
        b'h', b'i',
    ];
    let crc = crate::util::crc32_ieee(&data) as u32;
    data.extend_from_slice(&crc.to_le_bytes());
    let sample = SampleDoc {
        id: "sample-v1".into(),
        rev: 0,
        format: Ref { id: "img".into(), version: 1 },
        name: "64x48 hi comment".into(),
        hex: crate::util::to_hex(&data),
        note: "Built-in demo sample, original bytes are immutable.".into(),
        derived_from: None,
    };
    store.upsert_sample(sample, None).map(|_| ())?;

    let v2 = FormatDoc {
        id: "img".into(),
        version: 2,
        inherits: None,
        endian: Endian::Little,
        magic: vec![Magic { offset: 0, hex: "494d5831".into() }],
        description: "IMX v2 adds fps and drops free-form comments".into(),
        layout: vec![
            u16le("width", Some(4)),
            u16le("height", None),
            Item::Bits(BitsField {
                name: "flags".into(),
                int: IntType::U8,
                parts: vec![
                    BitPart { name: "alpha".into(), lsb: 0, bits: 1, description: None },
                    BitPart { name: "kind".into(), lsb: 1, bits: 2, description: None },
                ],
                offset: None,
            }),
            u8("fps", None),
            Item::Align(AlignField { name: "pad".into(), boundary: 4, pad: PadKind::Zero }),
            Item::Checksum(ChecksumField {
                name: "crc".into(),
                int: IntType::U32,
                algo: Algo::Crc32,
                range: ChkRange::default(),
                offset: None,
            }),
        ],
    };
    store.upsert_format(v2, None).map(|_| ())?;

    let v3 = FormatDoc {
        id: "img-ext".into(),
        version: 1,
        inherits: Some(Ref { id: "img".into(), version: 2 }),
        endian: Endian::Little,
        magic: vec![],
        description: "Inheritance demo: v2 layout plus an extension count at the tail.".into(),
        layout: vec![Item::Ext(ExtField {
            name: "extensions".into(),
            count: Count::Fixed { value: 0 },
            header: ExtHeader {
                tag: IntType::U8,
                tag_scale: 1,
                len: IntType::U8,
                len_scale: 1,
                endian: Endian::Little,
            },
            known: std::collections::BTreeMap::new(),
        })],
    };
    store.upsert_format(v3, None).map(|_| ())?;

    let rule = RuleDoc {
        id: "img-v1-to-v2".into(),
        version: 1,
        from: Ref { id: "img".into(), version: 1 },
        to: Ref { id: "img".into(), version: 2 },
        bindings: vec![
            Binding { target: "width".into(), source: Source::Field { path: "width".into() }, note: "direct".into() },
            Binding { target: "height".into(), source: Source::Field { path: "height".into() }, note: "direct".into() },
            Binding { target: "flags.alpha".into(), source: Source::Field { path: "flags.alpha".into() }, note: "bit".into() },
            Binding { target: "flags.kind".into(), source: Source::Field { path: "flags.kind".into() }, note: "bit".into() },
            Binding { target: "fps".into(), source: Source::Constant { value: 30 }, note: "new field default 30fps".into() },
        ],
        description: "Lossy: comment_* has no v2 representation.".into(),
    };
    store.upsert_rule(rule, 0, None).map(|_| ())?;
    Ok(())
}
