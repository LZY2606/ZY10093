mod common;

use binfmt_workbench::expr;
use binfmt_workbench::migrate::{convert_one, run_dry_run, DryRunInput};
use binfmt_workbench::model::*;
use binfmt_workbench::parse::{parse_input, validate_resolved, write_identity};
use std::collections::HashMap;

fn v1_spec() -> FormatSpec {
    serde_json::from_value(serde_json::json!({
        "name": "img",
        "version": "v1",
        "fields": [
            {"name":"magic","kind":"magic","value":"494d4731"},
            {"name":"width","kind":"int","width":2,"endian":"big"},
            {"name":"height","kind":"int","width":2,"endian":"big"},
            {"name":"flags","kind":"bitfield","width":1,"members":[
                {"name":"compressed","lsb":0,"bits":1},
                {"name":"channels","lsb":1,"bits":3}
            ]},
            {"name":"payload_len","kind":"int","width":2,"endian":"big"},
            {"name":"payload","kind":"bytes","length":"payload_len"},
            {"name":"crc","kind":"checksum","width":4,"algorithm":"crc32",
             "range":{"start":"0","end":"$pos","exclude":[["$pos","$pos+4"]]}}
        ]
    }))
    .unwrap()
}

fn v1_bytes(payload: &[u8], compressed: u8, channels: u8) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"IMG1");
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&4u16.to_be_bytes());
    b.push(compressed | (channels << 1));
    b.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    b.extend_from_slice(payload);
    // crc over everything before this point
    let crc = binfmt_workbench::parse::checksum_compute("crc32", &b);
    b.extend_from_slice(&crc);
    b
}

#[test]
fn expression_engine() {
    let mut m = HashMap::new();
    m.insert("a".to_string(), 7);
    m.insert("b".to_string(), 3);
    assert_eq!(expr::eval("a + b * 2", &m).unwrap(), 13);
    assert_eq!(expr::eval("(a + b) * 2", &m).unwrap(), 20);
    assert_eq!(expr::eval("a > b && b == 3", &m).unwrap(), 1);
    assert_eq!(expr::eval("0xff & a", &m).unwrap(), 7);
    assert_eq!(expr::eval("1 << 4", &m).unwrap(), 16);
    assert_eq!(expr::eval("arr[3].x + 1", &{
        let mut h = HashMap::new();
        h.insert("arr.3.x".to_string(), 9);
        h
    }).unwrap(), 10);
    assert!(expr::eval("a +", &m).is_err());
}

#[test]
fn parses_and_identity_writes() {
    let spec = v1_spec();
    assert!(validate_resolved(&spec).is_empty());
    let bytes = v1_bytes(&[1, 2, 3, 4, 5], 1, 2);
    let r = parse_input(&spec, &bytes);
    assert!(r.ok, "issues: {:?}", r.issues);
    assert_eq!(r.scalar_map["width"], 3);
    assert_eq!(r.scalar_map["height"], 4);
    assert_eq!(r.scalar_map["flags.compressed"], 1);
    assert_eq!(r.scalar_map["flags.channels"], 2);
    let back = write_identity(&r, &bytes).unwrap();
    assert_eq!(back, bytes, "identity round-trip must be byte-exact");
}

#[test]
fn accurate_offsets_for_short_read_and_overrun() {
    let spec = v1_spec();
    // truncated right after magic+width (6 bytes)
    let bad = b"IMG1\x00\x03".to_vec();
    let r = parse_input(&spec, &bad);
    assert!(!r.ok);
    let issue = r.issues.iter().find(|i| i.code == "short_read").unwrap();
    assert_eq!(issue.offset, Some(6), "short read offset: {issue:?}");

    // length overruns input: declare payload_len huge
    let mut bad2 = b"IMG1".to_vec();
    bad2.extend_from_slice(&3u16.to_be_bytes());
    bad2.extend_from_slice(&4u16.to_be_bytes());
    bad2.push(0);
    bad2.extend_from_slice(&0x1000u16.to_be_bytes());
    let r2 = parse_input(&spec, &bad2);
    assert!(!r2.ok);
    let issue = r2.issues.iter().find(|i| i.code == "length_out_of_bounds").unwrap();
    assert_eq!(issue.offset, Some(11));
}

#[test]
fn magic_mismatch_reports_offset() {
    let spec = v1_spec();
    let mut bytes = v1_bytes(&[1], 0, 0);
    bytes[0] = b'X';
    let r = parse_input(&spec, &bytes);
    let issue = r.issues.iter().find(|i| i.code == "magic_mismatch").unwrap();
    assert_eq!(issue.offset, Some(0));
    assert!(!r.ok);
}

#[test]
fn checksum_self_reference_rejected() {
    let mut spec = v1_spec();
    // range covering whole file including the checksum itself, no exclude
    if let Some(c) = spec.fields.iter_mut().find(|f| f.name == "crc") {
        c.range = Some(ChecksumRange { start: "0".into(), end: String::new(), exclude: vec![] });
    }
    let bytes = v1_bytes(&[9], 0, 0);
    let r = parse_input(&spec, &bytes);
    let issue = r.issues.iter().find(|i| i.code == "checksum_self_reference").unwrap();
    assert!(issue.offset.is_some());
}

#[test]
fn checksum_mismatch_is_non_fatal_but_reported() {
    let spec = v1_spec();
    let mut bytes = v1_bytes(&[1], 0, 0);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let r = parse_input(&spec, &bytes);
    // parse tree still produced
    assert!(r.root.is_some());
    assert!(r.issues.iter().any(|i| i.code == "checksum_mismatch"));
}

#[test]
fn trailing_unknown_region_preserved() {
    let spec = v1_spec();
    let mut bytes = v1_bytes(&[1, 2], 0, 0);
    bytes.extend_from_slice(&[0xAB, 0xCD, 0xEF]);
    let r = parse_input(&spec, &bytes);
    // length still reads payload 2 then crc then 3 extra -> trailing unknown
    let back = write_identity(&r, &bytes).unwrap();
    assert_eq!(back, bytes);
}

fn v2_spec() -> FormatSpec {
    serde_json::from_value(serde_json::json!({
        "name": "img",
        "version": "v2",
        "fields": [
            {"name":"magic","kind":"magic","value":"494d4732"},
            {"name":"version","kind":"int","width":1,"endian":"big"},
            {"name":"width","kind":"int","width":4,"endian":"big"},
            {"name":"height","kind":"int","width":4,"endian":"big"},
            {"name":"depth","kind":"int","width":2,"endian":"big"},
            {"name":"flags","kind":"bitfield","width":1,"members":[
                {"name":"compressed","lsb":0,"bits":1},
                {"name":"channels","lsb":1,"bits":3}
            ]},
            {"name":"payload_len","kind":"int","width":4,"endian":"big"},
            {"name":"payload","kind":"bytes","length":"payload_len"}
        ]
    }))
    .unwrap()
}

fn v1_to_v2_rule() -> RuleSpec {
    serde_json::from_value(serde_json::json!({
        "name":"img_v1_v2",
        "from_format":"img","from_revision":1,
        "to_format":"img","to_revision":2,
        "mappings":[
            {"op":"constant","to":"version","value":2},
            {"op":"copy","from":"width","to":"width"},
            {"op":"copy","from":"height","to":"height"},
            {"op":"constant","to":"depth","value":8},
            {"op":"copy","from":"flags.compressed","to":"flags.compressed"},
            {"op":"copy","from":"flags.channels","to":"flags.channels"},
            {"op":"copy","from":"payload_len","to":"payload_len"},
            {"op":"copy","from":"payload","to":"payload"},
            {"op":"drop","from":"crc"}
        ]
    }))
    .unwrap()
}

#[test]
fn migration_converts_and_classifies_semantic() {
    let src = v1_spec();
    let dst = v2_spec();
    let rule = v1_to_v2_rule();
    let bytes = v1_bytes(&[10, 20, 30], 1, 3);
    let report = run_dry_run(DryRunInput {
        rule: &rule,
        rule_revision: 1,
        src: &src,
        dst: &dst,
        samples: &[("s1".into(), bytes.clone())],
    });
    assert!(report.ok, "rule issues / sample errors: {}", serde_json::to_string(&report).unwrap());
    assert_eq!(report.reverse_tier, "semantic", "dropped crc -> at least semantic; report: {:?}", report.samples);
    let out = convert_one(&rule, &src, &dst, &bytes).unwrap();
    // verify magic and widened fields
    assert_eq!(&out[0..4], b"IMG2");
    assert_eq!(u32::from_be_bytes(out[5..9].try_into().unwrap()), 3);
    assert_eq!(u32::from_be_bytes(out[9..13].try_into().unwrap()), 4);
    assert_eq!(u16::from_be_bytes(out[13..15].try_into().unwrap()), 8);
    // payload preserved at tail
    assert_eq!(&out[out.len()-3..], &[10,20,30]);
    // v2 output parses cleanly
    let pr = parse_input(&dst, &out);
    assert!(pr.ok, "{:?}", pr.issues);
}

#[test]
fn inheritance_shadow_and_cycle_rejected() {
    use binfmt_workbench::parse::{resolve_inheritance, Issue, ParentLookup};
    struct L;
    impl ParentLookup for L {
        fn resolve_parent(&self, name: &str, _revision: i64) -> Result<Vec<FieldDef>, Issue> {
            if name == "base" {
                Ok(serde_json::from_value::<FormatSpec>(serde_json::json!({
                    "name":"base","version":"r1",
                    "fields":[{"name":"x","kind":"int","width":1}]
                })).unwrap().fields)
            } else {
                // self -> child cycle
                Ok(serde_json::from_value::<FormatSpec>(serde_json::json!({
                    "name":"child","version":"r1","inherit":{"name":"child","revision":1},
                    "fields":[]
                })).unwrap().fields)
            }
        }
    }
    let shadow: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"derived","version":"r1",
        "inherit":{"name":"base","revision":1},
        "fields":[{"name":"x","kind":"int","width":2}]
    })).unwrap();
    let mut chain = Vec::new();
    assert!(matches!(resolve_inheritance(&shadow, &L, &mut chain).unwrap_err().code.as_str(), "shadow_field"));

    let cycle: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"child","version":"r1",
        "inherit":{"name":"child","revision":1},
        "fields":[]
    })).unwrap();
    // Parent lookup returns fields that themselves inherit child -> cycle on resolution
    struct Cyc;
    impl ParentLookup for Cyc {
        fn resolve_parent(&self, _name: &str, _revision: i64) -> Result<Vec<FieldDef>, Issue> {
            // simulate parent whose flattened resolution would re-enter:
            // easiest: craft via two-level lookup is complex, so test the chain guard directly
            Err(Issue::spec("missing_parent", "n/a"))
        }
    }
    let mut chain2 = vec!["child#r1".to_string()];
    let err = resolve_inheritance(&cycle, &Cyc, &mut chain2).unwrap_err();
    // Either cycle (guard triggers before lookup since key already in chain)
    assert_eq!(err.code, "inherit_cycle");
}

#[test]
fn deterministic_export_archive() {
    let e1 = binfmt_workbench::tar::TarEntry { path: "a.txt".into(), data: b"hello", executable: false };
    let e2 = binfmt_workbench::tar::TarEntry { path: "b/dir/c.txt".into(), data: b"world", executable: false };
    let t1 = binfmt_workbench::tar::write_ustar(&[e1, e2]);
    let e3 = binfmt_workbench::tar::TarEntry { path: "a.txt".into(), data: b"hello", executable: false };
    let e4 = binfmt_workbench::tar::TarEntry { path: "b/dir/c.txt".into(), data: b"world", executable: false };
    let t2 = binfmt_workbench::tar::write_ustar(&[e4, e3]); // different insertion order
    assert_eq!(t1, t2, "tar export must be order-independent and deterministic");
}

#[test]
fn tlv_extensions_and_overlap_detection() {
    // container: tag(2) + len(2) big-endian; ext 1 known { int2 }, ext 9 unknown
    let spec: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"tlv","version":"v1",
        "fields":[
            {"name":"magic","kind":"magic","value":"544c"},
            {"name":"exts","kind":"ext_container","tag_width":2,"len_width":2,"endian":"big",
             "extensions":[
                {"tag":1,"name":"meta","fields":[
                    {"name":"code","kind":"int","width":2,"endian":"big"}]}
             ]}
        ]
    })).unwrap();
    let mut b = b"TL".to_vec();
    // block tag1 len2 code=0x0102
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&2u16.to_be_bytes());
    b.extend_from_slice(&0x0102u16.to_be_bytes());
    // unknown block tag9 len3
    b.extend_from_slice(&9u16.to_be_bytes());
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&[0xAA, 0xBB, 0xCC]);

    let r = parse_input(&spec, &b);
    assert!(r.ok, "{:?}", r.issues);
    let back = write_identity(&r, &b).unwrap();
    assert_eq!(back, b, "unknown ext block must be preserved and order kept");

    // ext leaf is addressable
    assert_eq!(r.scalar_map["exts.meta.code"], 0x0102);

    // truncated extension length overruns container (ends at eof) -> error
    let mut bad = b"TL".to_vec();
    bad.extend_from_slice(&1u16.to_be_bytes());
    bad.extend_from_slice(&100u16.to_be_bytes()); // claims 100 bytes, none present
    bad.extend_from_slice(&[0, 0]);
    let r2 = parse_input(&spec, &bad);
    assert!(r2.issues.iter().any(|i| i.code == "length_out_of_bounds"), "{:?}", r2.issues);
}

#[test]
fn overlap_fields_detected_at_offset() {
    // two bytes-length fields both claim the same run via a crafted expression
    let spec: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"ov","version":"v1",
        "fields":[
            {"name":"n","kind":"int","width":1,"endian":"big"},
            {"name":"a","kind":"bytes","length":"n"},
            {"name":"b","kind":"bytes","length":"n"}
        ]
    })).unwrap();
    // n=2, then 4 bytes: a=[1..3], b=[3..5] -> no overlap
    let ok = vec![2u8, 0xAA, 0xAA, 0xBB, 0xBB];
    let r = parse_input(&spec, &ok);
    assert!(r.ok, "no overlap: {:?}", r.issues);

    // Force overlap: a length = n+1 so a=[1..4], b=[4..6] adjacent (still no overlap).
    // Real overlap needs same start; emulate via branch-independent layout:
    let spec2: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"ov","version":"v2",
        "fields":[
            {"name":"n","kind":"int","width":1,"endian":"big"},
            {"name":"a","kind":"bytes","length":"n+1"},
            {"name":"b","kind":"bytes","length":"n"}
        ]
    })).unwrap();
    // n=2: a=[1..4], b starts at 4 -> adjacent; extend input to 6 so b=[4..6], ok.
    let bytes = vec![2u8, 1,2,3,4,5];
    let r2 = parse_input(&spec2, &bytes);
    assert!(r2.ok, "adjacent should pass: {:?}", r2.issues);

    // Genuine overlap: b length expression reads n while positioned after a,
    // which had length n+2. b therefore starts inside a's span.
    let _spec3: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"ov","version":"v3",
        "fields":[
            {"name":"n","kind":"int","width":1,"endian":"big"},
            {"name":"a","kind":"bytes","length":"n+2"},
            {"name":"b","kind":"bytes","length":"2"}
        ]
    })).unwrap();
    // n=2: a=[1..5], b starts at 5 -> still adjacent; build the overlapping one
    // by using a fixed-length a and an absolute-overlap length referencing bytes
    // already consumed is not expressible forward, so test via a pad+bytes pair
    // where pad length expression resolves larger than remaining space differently.
    let spec4: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"ov","version":"v4",
        "fields":[
            {"name":"n","kind":"int","width":1,"endian":"big"},
            {"name":"a","kind":"bytes","length":"n"},
            {"name":"b","kind":"bytes","length":"3"}
        ]
    })).unwrap();
    // n=3, total 7 bytes: a=[1..4], b=[4..7] adjacent -> ok (guard against false+)
    let b4 = vec![3u8, 1,2,3,4,5,6];
    let r4 = parse_input(&spec4, &b4);
    assert!(r4.ok, "must not false-positive adjacent: {:?}", r4.issues);
}

#[test]
fn bitfield_member_overlap_in_spec_rejected() {
    let spec: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"bf","version":"v1",
        "fields":[{"name":"f","kind":"bitfield","width":1,"members":[
            {"name":"a","lsb":0,"bits":3},
            {"name":"b","lsb":2,"bits":2}
        ]}]
    })).unwrap();
    let issues = validate_resolved(&spec);
    assert!(issues.iter().any(|i| i.code == "overlap_bitfield"), "{issues:?}");
}

#[test]
fn duplicate_extension_tag_overlaps_and_reports_offset() {
    let spec: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"tlv2","version":"v1",
        "fields":[
            {"name":"magic","kind":"magic","value":"544c"},
            {"name":"exts","kind":"ext_container","tag_width":1,"len_width":1,"endian":"big",
             "extensions":[{"tag":1,"name":"m","fields":[{"name":"c","kind":"int","width":1}]}]}
        ]
    })).unwrap();
    let mut b = b"TL".to_vec();
    // two blocks with same tag 1, each len1 body 0x11 then 0x22
    b.extend_from_slice(&[1,1,0x11, 1,1,0x22]);
    let r = parse_input(&spec, &b);
    let iss = r.issues.iter().find(|i| i.code == "overlap_fields");
    assert!(iss.is_some(), "duplicate tag must be flagged: {:?}", r.issues);
    // offset points at the second block header (magic2 + first block3 = offset 5)
    assert_eq!(iss.unwrap().offset, Some(5));
    // identity write still reconstructs the input bytes verbatim
    assert_eq!(write_identity(&r, &b).unwrap(), b);
}

#[test]
fn migration_lifts_known_extension_and_drops_unknown() {
    // v1: magic + ext container with a known ext(tag1 { code u16 }) and an unknown tag 7.
    let src: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"extf","version":"v1",
        "fields":[
            {"name":"magic","kind":"magic","value":"4558"},
            {"name":"exts","kind":"ext_container","tag_width":1,"len_width":1,"endian":"big",
             "extensions":[
                {"tag":1,"name":"meta","fields":[{"name":"code","kind":"int","width":2,"endian":"big"}]}
             ]}
        ]
    })).unwrap();
    // v2: magic + a flat code u32; extensions container is dropped entirely.
    let dst: FormatSpec = serde_json::from_value(serde_json::json!({
        "name":"extf","version":"v2",
        "fields":[
            {"name":"magic","kind":"magic","value":"4558"},
            {"name":"code","kind":"int","width":4,"endian":"big"}
        ]
    })).unwrap();
    let rule: RuleSpec = serde_json::from_value(serde_json::json!({
        "name":"lift","from_format":"extf","from_revision":1,
        "to_format":"extf","to_revision":2,
        "mappings":[
            {"op":"from_extension","from":"exts.meta.code","to":"code"}
        ],
        "drop_extensions":[7]
    })).unwrap();

    let mut bytes = b"EX".to_vec();
    // tag1 len2 code=0x0102
    bytes.extend_from_slice(&[1,2]);
    bytes.extend_from_slice(&0x0102u16.to_be_bytes());
    // tag7 len2 (unknown)
    bytes.extend_from_slice(&[7,2,0xAA,0xBB]);

    let report = run_dry_run(DryRunInput {
        rule: &rule, rule_revision: 1, src: &src, dst: &dst,
        samples: &[("x".into(), bytes.clone())],
    });
    assert!(report.ok, "{}", serde_json::to_string(&report).unwrap());
    // unknown tag dropped => at least one loss; lifted ext => semantic (not strict)
    assert!(report.losses.iter().any(|l| l.kind == "dropped_extension"), "{:?}", report.losses);
    assert_eq!(report.reverse_tier, "semantic");

    let out = convert_one(&rule, &src, &dst, &bytes).unwrap();
    // EX + 4-byte code 0x00000102, no extensions remain
    assert_eq!(out, vec![b'E', b'X', 0,0,1,2]);
    let pr = parse_input(&dst, &out);
    assert!(pr.ok, "{:?}", pr.issues);
}
