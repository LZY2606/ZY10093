use bfw::model::*;
use bfw::parser::Parser;
use bfw::writer::{write, WriteRequest};
use std::collections::{BTreeMap, BTreeSet};

struct Empty;
impl FormatLookup for Empty {
    fn get(&self, _id: &str, _v: u32) -> Option<&FormatDoc> { None }
}

fn ext_doc() -> FormatDoc {
    let mut known = BTreeMap::new();
    known.insert(
        "1".to_string(),
        vec![Item::Int(IntField {
            name: "gain".into(),
            int: IntType::U8,
            offset: None,
            description: None,
        })],
    );
    FormatDoc {
        id: "box".into(),
        version: 1,
        inherits: None,
        endian: Endian::Little,
        magic: vec![Magic { offset: 0, hex: "424f5831".into() }],
        description: "".into(),
        layout: vec![
            Item::Int(IntField {
                name: "ext_count".into(),
                int: IntType::U8,
                offset: Some(4),
                description: None,
            }),
            Item::Ext(ExtField {
                name: "extensions".into(),
                count: Count::Field { path: "ext_count".into() },
                header: ExtHeader {
                    tag: IntType::U8,
                    tag_scale: 1,
                    len: IntType::U8,
                    len_scale: 1,
                    endian: Endian::Little,
                },
                known,
            }),
        ],
    }
}

#[test]
fn unknown_extension_payload_roundtrips_verbatim() {
    let doc = ext_doc();
    let compiled = compile(&Empty, &doc).unwrap();
    // magic(4) count(1) block: tag=9(unknown) len=3 payload aabbcc ; block tag=1 len=1 gain=7
    let data = vec![
        0x42, 0x4f, 0x58, 0x31,
        2,
        9, 3, 0xAA, 0xBB, 0xCC,
        1, 1, 7,
    ];
    let outcome = Parser::parse(&compiled, &data);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>());

    let unknown_paths: BTreeSet<String> = outcome
        .tree
        .iter()
        .flat_map(|n| n.children.iter())
        .filter(|n| !n.identified)
        .map(|n| n.path.clone())
        .collect();
    assert!(unknown_paths.iter().any(|p| p.starts_with("extensions[0]")));

    let result = write(WriteRequest {
        compiled: &compiled,
        tree: &outcome.tree,
        edits: BTreeMap::new(),
        original: Some(&data),
    })
    .unwrap();
    assert_eq!(result.bytes, data);
}

#[test]
fn short_extension_payload_reports_offset() {
    let doc = ext_doc();
    let compiled = compile(&Empty, &doc).unwrap();
    let data = vec![0x42, 0x4f, 0x58, 0x31, 2, 9, 3, 0xAA];
    let outcome = Parser::parse(&compiled, &data);
    let err = outcome
        .errors
        .iter()
        .find(|e| e.code == "length_out_of_bounds" || e.code == "short_read")
        .expect("length/short read error");
    assert_eq!(err.offset, 7);
}
