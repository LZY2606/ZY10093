use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::util::parse_hex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    Little,
    Big,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    pub id: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Magic {
    pub offset: usize,
    pub hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntType {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
}

impl IntType {
    pub fn bytes(self) -> usize {
        match self {
            IntType::U8 | IntType::I8 => 1,
            IntType::U16 | IntType::I16 => 2,
            IntType::U32 | IntType::I32 => 4,
            IntType::U64 | IntType::I64 => 8,
        }
    }
    pub fn signed(self) -> bool {
        matches!(self, IntType::I8 | IntType::I16 | IntType::I32 | IntType::I64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadKind {
    Zero,
    Preserve,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Len {
    Fixed { value: usize },
    Field { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cmp {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    HasBit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cond {
    pub path: String,
    pub cmp: Cmp,
    pub value: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Algo {
    Xor8,
    Sum16,
    Crc32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RangePoint {
    Start,
    End,
    FieldEnd { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub at: RangePoint,
    pub offset: isize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChkRange {
    #[serde(default)]
    pub start: Option<Edge>,
    #[serde(default)]
    pub end: Option<Edge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Count {
    Fixed { value: usize },
    Field { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtHeader {
    pub tag: IntType,
    #[serde(default = "default_one")]
    pub tag_scale: u64,
    pub len: IntType,
    #[serde(default = "default_one")]
    pub len_scale: u64,
    pub endian: Endian,
}

fn default_one() -> u64 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitPart {
    pub name: String,
    pub lsb: u32,
    pub bits: u32,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntField {
    pub name: String,
    pub int: IntType,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitsField {
    pub name: String,
    pub int: IntType,
    pub parts: Vec<BitPart>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlignField {
    pub name: String,
    pub boundary: usize,
    #[serde(default = "default_pad")]
    pub pad: PadKind,
}

fn default_pad() -> PadKind {
    PadKind::Zero
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytesField {
    pub name: String,
    pub len: Len,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arm {
    #[serde(default)]
    pub when: Option<Cond>,
    pub layout: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchField {
    pub name: String,
    pub arms: Vec<Arm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksumField {
    pub name: String,
    pub int: IntType,
    pub algo: Algo,
    #[serde(default)]
    pub range: ChkRange,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtField {
    pub name: String,
    pub count: Count,
    pub header: ExtHeader,
    #[serde(default)]
    pub known: BTreeMap<String, Vec<Item>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Item {
    Magic(Magic),
    Int(IntField),
    Bits(BitsField),
    Align(AlignField),
    Bytes(BytesField),
    Branch(BranchField),
    Checksum(ChecksumField),
    Ext(ExtField),
}

impl Item {
    pub fn name(&self) -> Option<&str> {
        Some(match self {
            Item::Magic(_) => return None,
            Item::Int(f) => &f.name,
            Item::Bits(f) => &f.name,
            Item::Align(f) => &f.name,
            Item::Bytes(f) => &f.name,
            Item::Branch(f) => &f.name,
            Item::Checksum(f) => &f.name,
            Item::Ext(f) => &f.name,
        })
    }
    pub fn offset(&self) -> Option<usize> {
        match self {
            Item::Int(f) => f.offset,
            Item::Bits(f) => f.offset,
            Item::Bytes(f) => f.offset,
            Item::Checksum(f) => f.offset,
            Item::Magic(m) => Some(m.offset),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatDoc {
    pub id: String,
    pub version: u32,
    #[serde(default)]
    pub inherits: Option<Ref>,
    pub endian: Endian,
    #[serde(default)]
    pub magic: Vec<Magic>,
    pub layout: Vec<Item>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct Compiled {
    pub doc: FormatDoc,
    pub effective: Vec<Item>,
    pub paths: BTreeSet<String>,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct FormatError {
    pub id: String,
    pub message: String,
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "format {}: {}", self.id, self.message)
    }
}

pub trait FormatLookup {
    fn get(&self, id: &str, version: u32) -> Option<&FormatDoc>;
}

fn validate_name(id: &str, name: &str) -> Result<(), FormatError> {
    if name.is_empty() || name.contains(".") || name == "arm" {
        return Err(FormatError {
            id: id.to_string(),
            message: format!("invalid field name: {name:?}"),
        });
    }
    Ok(())
}

fn ferr(id: &str, msg: impl Into<String>) -> FormatError {
    FormatError { id: id.to_string(), message: msg.into() }
}

fn magic_valid(id: &str, magics: &[Magic]) -> Result<(), FormatError> {
    for m in magics {
        let bytes = parse_hex(&m.hex).map_err(|e| ferr(id, format!("bad magic hex: {e}")))?;
        if bytes.is_empty() {
            return Err(ferr(id, "empty magic"));
        }
    }
    Ok(())
}

fn walk_items(
    id: &str,
    items: &[Item],
    prefix: &str,
    paths: &mut BTreeSet<String>,
) -> Result<(), FormatError> {
    for item in items {
        match item {
            Item::Int(f) => {
                validate_name(id, &f.name)?;
                check_dup(id, prefix, &f.name, paths)?;
            }
            Item::Bits(f) => {
                validate_name(id, &f.name)?;
                let p = check_dup(id, prefix, &f.name, paths)?;
                let mut part_paths: BTreeSet<String> = BTreeSet::new();
                for part in &f.parts {
                    validate_name(id, &part.name)?;
                    let full = format!("{p}.{}", part.name);
                    if !part_paths.insert(full.clone()) {
                        return Err(ferr(id, format!("duplicate bit part path: {full}")));
                    }
                    paths.insert(full);
                }
                if f.int.bytes() > 8 {
                    return Err(ferr(id, "bits width too large"));
                }
            }
            Item::Align(f) => {
                validate_name(id, &f.name)?;
                check_dup(id, prefix, &f.name, paths)?;
                if f.boundary == 0 {
                    return Err(ferr(id, "alignment boundary must be > 0"));
                }
            }
            Item::Bytes(f) => {
                validate_name(id, &f.name)?;
                check_dup(id, prefix, &f.name, paths)?;
            }
            Item::Checksum(f) => {
                validate_name(id, &f.name)?;
                check_dup(id, prefix, &f.name, paths)?;
            }
            Item::Branch(f) => {
                validate_name(id, &f.name)?;
                let p = check_dup(id, prefix, &f.name, paths)?;
                for (idx, arm) in f.arms.iter().enumerate() {
                    let arm_prefix = format!("{p}.arm{idx}");
                    walk_items(id, &arm.layout, &arm_prefix, paths)?;
                }
            }
            Item::Ext(f) => {
                validate_name(id, &f.name)?;
                let p = check_dup(id, prefix, &f.name, paths)?;
                let mut seen_tags = BTreeSet::new();
                for (tag, layout) in &f.known {
                    if tag.is_empty() {
                        return Err(ferr(id, "empty extension tag"));
                    }
                    if !seen_tags.insert(tag.clone()) {
                        return Err(ferr(id, format!("duplicate known tag: {tag}")));
                    }
                    walk_items(id, layout, &p, paths)?;
                }
            }
            Item::Magic(_) => {}
        }
    }
    Ok(())
}

fn check_dup(
    id: &str,
    prefix: &str,
    name: &str,
    paths: &mut BTreeSet<String>,
) -> Result<String, FormatError> {
    let full = if prefix.is_empty() { name.to_string() } else { format!("{prefix}.{name}") };
    if !paths.insert(full.clone()) {
        return Err(ferr(id, format!("duplicate or shadowed field path: {full}")));
    }
    Ok(full)
}

fn checksum_self_refs(id: &str, items: &[Item]) -> Result<(), FormatError> {
    fn point_matches(point: &RangePoint, target: &str) -> bool {
        match point {
            RangePoint::FieldEnd { path } => path == target,
            _ => false,
        }
    }
    for item in items {
        match item {
            Item::Checksum(f) => {
                let range_targets_self =
                    f.range.start.as_ref().map(|e| point_matches(&e.at, &f.name)).unwrap_or(false)
                        || f.range.end.as_ref().map(|e| point_matches(&e.at, &f.name)).unwrap_or(false);
                if range_targets_self {
                    return Err(ferr(id, format!("checksum {} range references itself", f.name)));
                }
            }
            Item::Branch(f) => {
                for arm in &f.arms {
                    checksum_self_refs(id, &arm.layout)?;
                }
            }
            Item::Ext(f) => {
                for layout in f.known.values() {
                    checksum_self_refs(id, layout)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn compile<L: FormatLookup>(lookup: &L, doc: &FormatDoc) -> Result<Compiled, FormatError> {
    let chain = resolve_chain(lookup, doc)?;
    let mut effective: Vec<Item> = Vec::new();
    let mut magics: Vec<Magic> = Vec::new();
    for ancestor in &chain {
        magics.extend(ancestor.magic.iter().cloned());
        effective.extend(ancestor.layout.iter().cloned());
    }
    magics.extend(doc.magic.iter().cloned());
    effective.extend(doc.layout.iter().cloned());

    let mut merged = doc.clone();
    merged.magic = magics;
    merged.layout = effective.clone();

    magic_valid(&doc.id, &merged.magic)?;
    let mut paths = BTreeSet::new();
    walk_items(&doc.id, &merged.layout, "", &mut paths)?;
    checksum_self_refs(&doc.id, &merged.layout)?;

    let fp_value = serde_json::to_value(&merged).map_err(|e| ferr(&doc.id, e.to_string()))?;
    let fingerprint = crate::util::fnv_fingerprint(&fp_value);
    Ok(Compiled {
        doc: merged,
        effective,
        paths,
        fingerprint,
    })
}

fn resolve_chain<L: FormatLookup>(lookup: &L, doc: &FormatDoc) -> Result<Vec<FormatDoc>, FormatError> {
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    seen.insert((doc.id.clone(), doc.version));
    let mut current = doc.inherits.clone();
    while let Some(r) = current {
        if !seen.insert((r.id.clone(), r.version)) {
            return Err(ferr(
                &doc.id,
                format!("inheritance cycle involving {}@v{}", r.id, r.version),
            ));
        }
        let parent = lookup
            .get(&r.id, r.version)
            .ok_or_else(|| ferr(&doc.id, format!("missing parent {}@v{}", r.id, r.version)))?;
        current = parent.inherits.clone();
        chain.insert(0, parent.clone());
    }
    Ok(chain)
}

pub fn encode_int(value: i128, int: IntType, endian: Endian) -> Vec<u8> {
    let bytes = int.bytes();
    let raw = if int.signed() { value as i128 } else { value as u128 as i128 };
    let bits = bytes * 8;
    let mask: u128 = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
    let unsigned = (raw as u128) & mask;
    let mut out = Vec::with_capacity(bytes);
    for i in 0..bytes {
        let shift = i * 8;
        out.push(((unsigned >> shift) & 0xff) as u8);
    }
    if endian == Endian::Big {
        out.reverse();
    }
    out
}

pub fn decode_int(bytes: &[u8], int: IntType, endian: Endian) -> i64 {
    let mut ordered = bytes.to_vec();
    if endian == Endian::Big {
        ordered.reverse();
    }
    let mut raw: u64 = 0;
    for (i, b) in ordered.iter().enumerate().take(8) {
        raw |= (*b as u64) << (i * 8);
    }
    if int.signed() {
        let bits = int.bytes() * 8;
        let sign = 1u64 << (bits - 1);
        if raw & sign != 0 {
            raw |= u64::MAX << bits;
            return raw as i64;
        }
    }
    raw as i64
}

pub fn fits_int(value: i128, int: IntType) -> bool {
    let bytes = int.bytes();
    if int.signed() {
        let max = (1i128 << (bytes * 8 - 1)) - 1;
        let min = -(1i128 << (bytes * 8 - 1));
        value >= min && value <= max
    } else {
        let max = if bytes == 8 { u64::MAX as i128 } else { (1i128 << (bytes * 8)) - 1 };
        value >= 0 && value <= max
    }
}
