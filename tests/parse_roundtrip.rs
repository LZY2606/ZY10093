mod common;
use std::collections::BTreeMap;

use bfw::model::*;
use bfw::parser::Parser;
use bfw::writer::{write, WriteRequest};
use common::*;

#[test]
fn parses_and_roundtrips_byte_identical() {
    let doc = v1();
    let compiled = compile(&Lookup(vec![doc.clone()]), &doc).unwrap();
    let data = sample_bytes();
    let outcome = Parser::parse(&compiled, &data);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>());
    assert!(outcome.warnings.is_empty());

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
fn reports_short_read_at_exact_offset() {
    let doc = v1();
    let compiled = compile(&Lookup(vec![doc.clone()]), &doc).unwrap();
    let full = sample_bytes();
    let truncated = full[..full.len() - 2].to_vec();
    let outcome = Parser::parse(&compiled, &truncated);
    let err = outcome.errors.iter().find(|e| e.code == "short_read").expect("short_read");
    assert_eq!(err.path, "crc");
    assert_eq!(err.offset, 15);
}

#[test]
fn reports_length_out_of_bounds_for_scope() {
    let mut doc = v1();
    if let Item::Bytes(f) = &mut doc.layout[5] {
        f.len = Len::Fixed { value: 999 };
    }
    let compiled = compile(&Lookup(vec![doc.clone()]), &doc).unwrap();
    let outcome = Parser::parse(&compiled, &sample_bytes());
    let err = outcome.errors.iter().find(|e| e.code == "length_out_of_bounds").expect("length error");
    assert_eq!(err.path, "comment");
    assert_eq!(err.offset, 13);
}

#[test]
fn reports_overlapping_absolute_fields() {
    let mut doc = v1();
    doc.layout.insert(1, Item::Int(IntField {
        name: "rogue".into(), int: IntType::U16, offset: Some(5), description: None,
    }));
    let compiled = compile(&Lookup(vec![doc.clone()]), &doc).unwrap();
    let outcome = Parser::parse(&compiled, &sample_bytes());
    assert!(outcome.errors.iter().any(|e| e.code == "overlap"), "expected overlap, got {:?}", outcome.errors.iter().map(|e| e.code.clone()).collect::<Vec<_>>());
}

#[test]
fn checksum_self_range_is_rejected_at_compile_time() {
    let mut doc = v1();
    if let Item::Checksum(f) = &mut doc.layout[6] {
        f.range = ChkRange {
            start: None,
            end: Some(Edge { at: RangePoint::FieldEnd { path: "crc".into() }, offset: 0 }),
        };
    }
    let err = compile(&Lookup(vec![doc.clone()]), &doc).unwrap_err();
    assert!(err.message.contains("range references itself"), "{}", err.message);
}

#[test]
fn checksum_self_covering_range_at_parse_is_flagged() {
    let mut doc = v1();
    if let Item::Checksum(f) = &mut doc.layout[6] {
        f.range = ChkRange {
            start: Some(Edge { at: RangePoint::Start, offset: 0 }),
            end: Some(Edge { at: RangePoint::End, offset: 0 }),
        };
    }
    let compiled = compile(&Lookup(vec![doc.clone()]), &doc).unwrap();
    let outcome = Parser::parse(&compiled, &sample_bytes());
    assert!(outcome.errors.iter().any(|e| e.code == "checksum_self_reference"));
}

#[test]
fn edit_updates_length_and_checksum_but_keeps_other_bytes() {
    let doc = v1();
    let compiled = compile(&Lookup(vec![doc.clone()]), &doc).unwrap();
    let data = sample_bytes();
    let outcome = Parser::parse(&compiled, &data);
    let mut edits = BTreeMap::new();
    edits.insert("comment".to_string(), serde_json::json!({ "hex": "616263" }));
    let result = write(WriteRequest {
        compiled: &compiled,
        tree: &outcome.tree,
        edits,
        original: Some(&data),
    })
    .unwrap();
    let reparsed = Parser::parse(&compiled, &result.bytes);
    assert!(reparsed.errors.is_empty(), "{:?}", reparsed.errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>());
    assert!(reparsed.warnings.is_empty(), "{:?}", reparsed.warnings.iter().map(|w| w.message.clone()).collect::<Vec<_>>());
    assert_ne!(result.bytes, data);
}

#[test]
fn preserves_unknown_trailer_bytes_verbatim() {
    let doc = v1();
    let compiled = compile(&Lookup(vec![doc.clone()]), &doc).unwrap();
    let mut data = sample_bytes();
    data.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    let outcome = Parser::parse(&compiled, &data);
    assert!(outcome.errors.is_empty());
    let result = write(WriteRequest {
        compiled: &compiled,
        tree: &outcome.tree,
        edits: BTreeMap::new(),
        original: Some(&data),
    })
    .unwrap();
    assert_eq!(result.bytes, data);
    assert!(outcome.tree.iter().any(|n| n.path == "unidentified.trailer" && !n.identified));
}
