use bfw::model::*;
use bfw::parser::Parser;
use bfw::writer::{write, WriteRequest};
use std::collections::BTreeMap;

struct Map(Vec<FormatDoc>);
impl FormatLookup for Map {
    fn get(&self, id: &str, version: u32) -> Option<&FormatDoc> {
        self.0.iter().find(|d| d.id == id && d.version == version)
    }
}

fn main() {
    let doc = FormatDoc {
        id: "img".into(),
        version: 1,
        inherits: None,
        endian: Endian::Little,
        magic: vec![Magic { offset: 0, hex: "49 4d 58 31".into() }],
        description: String::new(),
        layout: vec![
            Item::Int(IntField { name: "width".into(), int: IntType::U16, offset: Some(4), description: None }),
            Item::Int(IntField { name: "height".into(), int: IntType::U16, offset: None, description: None }),
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
            Item::Int(IntField { name: "comment_len".into(), int: IntType::U8, offset: None, description: None }),
            Item::Bytes(BytesField { name: "comment".into(), len: Len::Field { path: "comment_len".into() }, offset: None }),
            Item::Checksum(ChecksumField {
                name: "crc".into(),
                int: IntType::U32,
                algo: Algo::Crc32,
                range: ChkRange::default(),
                offset: None,
            }),
        ],
    };
    let compiled = compile(&Map(vec![]), &doc).unwrap();
    // construct sample: magic 4 + width 2 + height 2 + flags 1 + pad 1 + len 1 + comment 2 + crc 4
    let mut data = vec![
        0x49, 0x4d, 0x58, 0x31, // magic
        64, 0, // width
        48, 0, // height
        0b0000_0110, // flags at offset 9
        0, 0, 0, // align to offset 12
        2, // comment_len
        b'h', b'i',
    ];
    let body_crc = bfw::util::crc32_ieee(&data);
    data.extend_from_slice(&(body_crc as u32).to_le_bytes());

    let outcome = Parser::parse(&compiled, &data);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>());
    println!("parsed {} nodes, warnings: {:?}", outcome.tree.len(), outcome.warnings.iter().map(|w| w.message.clone()).collect::<Vec<_>>());
    let result = write(WriteRequest {
        compiled: &compiled,
        tree: &outcome.tree,
        edits: BTreeMap::new(),
        original: Some(&data),
    })
    .unwrap();
    assert_eq!(result.bytes, data, "roundtrip not byte identical");
    println!("roundtrip byte-identical: {} bytes", result.bytes.len());
}
