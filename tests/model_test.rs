mod common;
use bfw::model::*;
use common::*;
use std::collections::BTreeMap;

struct Map(BTreeMap<(String, u32), FormatDoc>);
impl FormatLookup for Map {
    fn get(&self, id: &str, version: u32) -> Option<&FormatDoc> {
        self.0.get(&(id.to_string(), version))
    }
}

fn base(id: &str, version: u32, inherits: Option<Ref>, layout: Vec<Item>) -> FormatDoc {
    FormatDoc {
        id: id.into(), version, inherits,
        endian: Endian::Little,
        magic: vec![Magic { offset: 0, hex: "aa".into() }],
        description: "".into(), layout,
    }
}

fn i(name: &str) -> Item {
    Item::Int(IntField { name: name.into(), int: IntType::U8, offset: None, description: None })
}

#[test]
fn rejects_inheritance_cycle() {
    let a = base("a", 1, Some(Ref { id: "b".into(), version: 1 }), vec![i("x")]);
    let b = base("b", 1, Some(Ref { id: "a".into(), version: 1 }), vec![i("y")]);
    let lookup = Map([
        (("a".to_string(), 1), a.clone()),
        (("b".to_string(), 1), b),
    ].into_iter().collect());
    let err = compile(&lookup, &a).unwrap_err();
    assert!(err.message.contains("cycle"), "{}", err.message);
}

#[test]
fn rejects_shadowed_field_names_via_inheritance() {
    let parent = base("p", 1, None, vec![i("dup")]);
    let child = base("p", 2, Some(Ref { id: "p".into(), version: 1 }), vec![i("dup")]);
    let lookup = Map([
        (("p".to_string(), 1), parent),
        (("p".to_string(), 2), child.clone()),
    ].into_iter().collect());
    let err = compile(&lookup, &child).unwrap_err();
    assert!(err.message.contains("shadowed") || err.message.contains("duplicate"), "{}", err.message);
}

#[test]
fn rejects_duplicate_bit_parts() {
    let mut doc = base("x", 1, None, vec![]);
    doc.layout = vec![Item::Bits(BitsField {
        name: "f".into(), int: IntType::U8, offset: None,
        parts: vec![
            BitPart { name: "p".into(), lsb: 0, bits: 1, description: None },
            BitPart { name: "p".into(), lsb: 1, bits: 1, description: None },
        ],
    })];
    let err = compile(&Map(BTreeMap::new()), &doc).unwrap_err();
    assert!(err.message.contains("duplicate"), "{}", err.message);
}

#[test]
fn fingerprint_changes_when_definition_changes() {
    let d1 = v1();
    let c1 = compile(&Lookup(vec![d1.clone()]), &d1).unwrap();
    let mut d2 = d1.clone();
    d2.description = "changed".into();
    let root = d2.clone(); let c2 = compile(&Lookup(vec![d2]), &root).unwrap();
    assert_ne!(c1.fingerprint, c2.fingerprint);
}
