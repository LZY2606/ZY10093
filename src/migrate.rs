//! Migration engine: rule validation, deterministic target serialization,
//! dry-run provenance reports and reverse equivalence verification.

use crate::expr::{self, EvalError, Resolver};
use crate::model::*;
#[allow(unused_imports)]
use crate::model::BitMember;
use crate::parse::{
    checksum_compute, encode_int, read_int, validate_resolved, Node, ParseResult, Value,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use crate::model::RuleSpec;

// ------------------------------------------------------------- static schema

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeafKind {
    Int,
    Bytes,
    Fixed,
    Magic,
    Pad,
    Checksum,
    BitMember,
}

#[derive(Clone, Debug, Default)]
pub struct SchemaLeaves {
    /// full dotted path -> kind
    pub leaves: BTreeMap<String, LeafKind>,
    /// array path -> element kind ("int"|"bytes"|"fixed"|"struct") and count expr
    pub arrays: BTreeMap<String, (String, String)>,
    /// extension scalar leaves: "container.extName.field"
    pub ext_leaves: BTreeSet<String>,
    /// extension container field names
    pub ext_containers: BTreeSet<String>,
}

fn enum_fields(
    fields: &[FieldDef],
    prefix: &str,
    out: &mut SchemaLeaves,
    in_array_struct: bool,
) {
    for f in fields {
        let path = if prefix.is_empty() {
            f.name.clone()
        } else {
            format!("{prefix}.{}", f.name)
        };
        match f.kind.as_str() {
            "int" => {
                out.leaves.insert(path.clone(), LeafKind::Int);
            }
            "bitfield" => {
                for m in &f.members {
                    out.leaves.insert(format!("{path}.{}", m.name), LeafKind::BitMember);
                }
            }
            "bytes" => {
                out.leaves.insert(path, LeafKind::Bytes);
            }
            "fixed" => {
                out.leaves.insert(path, LeafKind::Fixed);
            }
            "magic" => {
                out.leaves.insert(path, LeafKind::Magic);
            }
            "pad" => {
                out.leaves.insert(path, LeafKind::Pad);
            }
            "checksum" => {
                out.leaves.insert(path, LeafKind::Checksum);
            }
            "struct" => {
                enum_fields(&f.fields, &path, out, in_array_struct);
            }
            "branch" => {
                for (case, body) in &f.cases {
                    enum_fields(body, &format!("{path}.{case}"), out, in_array_struct);
                }
            }
            "array" => {
                let el = &f.element[0];
                out.arrays
                    .insert(path.clone(), (f.count.as_deref().unwrap_or("0").to_string(), el.kind.clone()));
                match el.kind.as_str() {
                    "int" | "bytes" | "fixed" => {}
                    "struct" => enum_fields(&el.fields, &format!("{path}.0"), out, true),
                    _ => {}
                }
            }
            "ext_container" => {
                out.ext_containers.insert(path.clone());
                for e in &f.extensions {
                    enum_ext_fields(&e.fields, &format!("{path}.{}", e.name), out);
                }
            }
            _ => {}
        }
    }
}


fn enum_ext_fields(fields: &[FieldDef], prefix: &str, out: &mut SchemaLeaves) {
    for f in fields {
        let p = format!("{prefix}.{}", f.name);
        match f.kind.as_str() {
            "int" | "bitfield" => {
                if f.kind == "bitfield" {
                    for m in &f.members {
                        out.ext_leaves.insert(format!("{p}.{}", m.name));
                    }
                } else {
                    out.ext_leaves.insert(p);
                }
            }
            "bytes" | "fixed" | "magic" | "pad" | "checksum" => {
                out.ext_leaves.insert(p);
            }
            "struct" => enum_ext_fields(&f.fields, &p, out),
            "branch" => {
                for (c, b) in &f.cases {
                    enum_ext_fields(b, &format!("{p}.{c}"), out);
                }
            }
            _ => {}
        }
    }
}

pub fn schema_leaves(spec: &FormatSpec) -> SchemaLeaves {
    let mut s = SchemaLeaves::default();
    enum_fields(&spec.fields, "", &mut s, false);
    s
}

/// Leaves that must be supplied by a rule (everything structural/auto excluded).
pub fn required_leaves(s: &SchemaLeaves) -> BTreeSet<String> {
    s.leaves
        .iter()
        .filter(|(_, k)| {
            matches!(k, LeafKind::Int | LeafKind::Bytes | LeafKind::BitMember)
        })
        .map(|(p, _)| p.clone())
        .collect()
}

// ------------------------------------------------------------- rule validation

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleIssue {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

pub fn validate_rule(rule: &RuleSpec, src: &FormatSpec, dst: &FormatSpec) -> Vec<RuleIssue> {
    let mut issues = Vec::new();
    if rule.mappings.iter().filter(|m| matches!(m, Mapping::Copy { .. })).count() == 0
        && rule.mappings.iter().filter(|m| matches!(m, Mapping::Constant { .. })).count() == 0
        && rule.mappings.iter().filter(|m| matches!(m, Mapping::FromExtension { .. })).count() == 0
    {
        issues.push(RuleIssue {
            code: "empty_rule".into(),
            message: "rule has no copy/constant/from_extension mappings".into(),
            path: None,
        });
    }
    let ss = schema_leaves(src);
    let ts = schema_leaves(dst);

    // from -> format revision cross-check is performed by the caller/store.
    let mut assigned: BTreeSet<String> = BTreeSet::new();
    for m in &rule.mappings {
        match m {
            Mapping::Copy { from, to, transform } => {
                let from_kind = ss.leaves.get(from).or_else(|| {
                    if ss.arrays.contains_key(from) {
                        Some(&LeafKind::Int)
                    } else {
                        None
                    }
                });
                if from_kind.is_none() && !ss.ext_leaves.contains(from) {
                    issues.push(RuleIssue {
                        code: "unknown_source".into(),
                        message: format!("source leaf `{from}` does not exist"),
                        path: Some(from.clone()),
                    });
                }
                let to_kind = ts.leaves.get(to).or_else(|| {
                    if ts.arrays.contains_key(to) {
                        Some(&LeafKind::Int)
                    } else {
                        None
                    }
                });
                if to_kind.is_none() {
                    issues.push(RuleIssue {
                        code: "unknown_target".into(),
                        message: format!("target leaf `{to}` does not exist"),
                        path: Some(to.clone()),
                    });
                }
                if let Some(t) = transform {
                    if !valid_transform(t) {
                        issues.push(RuleIssue {
                            code: "bad_transform".into(),
                            message: format!("unknown transform `{t}`"),
                            path: Some(to.clone()),
                        });
                    }
                }
                if !assigned.insert(to.clone()) {
                    issues.push(RuleIssue {
                        code: "double_assignment".into(),
                        message: format!("target `{to}` is assigned by more than one mapping"),
                        path: Some(to.clone()),
                    });
                }
            }
            Mapping::Constant { to, .. } => {
                if ts.leaves.get(to).is_none() && !ts.arrays.contains_key(to) {
                    issues.push(RuleIssue {
                        code: "unknown_target".into(),
                        message: format!("constant target `{to}` does not exist"),
                        path: Some(to.clone()),
                    });
                }
                if !assigned.insert(to.clone()) {
                    issues.push(RuleIssue {
                        code: "double_assignment".into(),
                        message: format!("target `{to}` is assigned by more than one mapping"),
                        path: Some(to.clone()),
                    });
                }
            }
            Mapping::Drop { from } => {
                if ss.leaves.get(from).is_none() && !ss.arrays.contains_key(from) {
                    issues.push(RuleIssue {
                        code: "unknown_source".into(),
                        message: format!("drop source `{from}` does not exist"),
                        path: Some(from.clone()),
                    });
                }
            }
            Mapping::FromExtension { from, to } => {
                if !ss.ext_leaves.contains(from) {
                    issues.push(RuleIssue {
                        code: "unknown_ext_source".into(),
                        message: format!("extension leaf `{from}` does not exist in source"),
                        path: Some(from.clone()),
                    });
                }
                if ts.leaves.get(to).is_none() {
                    issues.push(RuleIssue {
                        code: "unknown_target".into(),
                        message: format!("target leaf `{to}` does not exist"),
                        path: Some(to.clone()),
                    });
                }
                if !assigned.insert(to.clone()) {
                    issues.push(RuleIssue {
                        code: "double_assignment".into(),
                        message: format!("target `{to}` is assigned by more than one mapping"),
                        path: Some(to.clone()),
                    });
                }
            }
        }
    }

    // Coverage: every required target leaf must be assigned (branch-specific
    // leaves are checked again at runtime).
    for p in required_leaves(&ts) {
        if !assigned.contains(&p) {
            issues.push(RuleIssue {
                code: "uncovered_target".into(),
                message: format!("target leaf `{p}` has no mapping or default"),
                path: Some(p),
            });
        }
    }
    let _ = required_leaves(&ss);

    // Format specs themselves must be internally valid.
    for i in validate_resolved(src) {
        issues.push(RuleIssue { code: format!("source:{}", i.code), message: i.message, path: None });
    }
    for i in validate_resolved(dst) {
        issues.push(RuleIssue { code: format!("target:{}", i.code), message: i.message, path: None });
    }
    issues
}

fn valid_transform(t: &str) -> bool {
    matches!(t, "u16_to_u32")
        || t.strip_prefix("bytes_truncate_").and_then(|s| s.parse::<usize>().ok()).is_some()
        || t.strip_prefix("bytes_pad_")
            .map(|rest| {
                let mut it = rest.split('_');
                let n: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let byte = it.next().and_then(|s| u8::from_str_radix(s, 16).ok());
                n > 0 && byte.is_some()
            })
            .unwrap_or(false)
}

// ------------------------------------------------------------- value extraction

#[derive(Clone, Debug)]
pub enum V {
    Int(i128),
    Bytes(Vec<u8>),
}

/// Gather leaf values from a parse tree.
/// `ints[path]` and `raw[path]` (byte-typed leaves) are populated,
/// plus `ext_raw[container][tag]` for preserving whole extension blocks.
#[derive(Default, Debug)]
pub struct Gathered {
    pub ints: BTreeMap<String, i128>,
    pub raw: BTreeMap<String, Vec<u8>>,
    /// container field name -> tag -> full block bytes (header+body)
    pub ext_blocks: BTreeMap<String, Vec<(i64, Vec<u8>)>>,
    /// ext leaf scalar values keyed "container.extName.field"
    pub ext_ints: BTreeMap<String, i128>,
    pub ext_raw: BTreeMap<String, Vec<u8>>,
}

fn gather_node(n: &Node, prefix: &str, g: &mut Gathered) {
    let p = if prefix.is_empty() { n.name.clone() } else { format!("{prefix}.{}", n.name) };
    match n.kind.as_str() {
        "root" | "struct" | "branch" | "array" => {
            for c in &n.children {
                gather_node(c, if n.kind == "root" { "" } else { &p }, g);
            }
        }
        "bitfield" => {
            for c in &n.children {
                if let Some(Value::Int(v)) = &c.value {
                    g.ints.insert(format!("{p}.{}", c.name), *v);
                }
            }
        }
        "int" => {
            if let Some(Value::Int(v)) = &n.value {
                g.ints.insert(p, *v);
            }
        }
        "bytes" | "fixed" | "pad" | "magic" | "checksum" => {
            if let Some(Value::Bytes(b)) = &n.value {
                g.raw.insert(p, b.0.clone());
            }
        }
        "ext_container" => {
            for blk in &n.children {
                if let Some(tag) = blk.tag {
                    // full block bytes come from the parse input range
                    // (stored in a side map built by the caller via node ranges)
                    if blk.matched.as_deref() == Some("known") {
                        for c in &blk.children {
                            gather_ext_known(c, &format!("{p}.{}", blk.name), g);
                        }
                    } else if let Some(Value::Bytes(b)) = &blk.value {
                        g.ext_raw.insert(format!("{p}.{}", blk.name), b.0.clone());
                    }
                    let entry = g.ext_blocks.entry(n.name.clone()).or_default();
                    // placeholder; filled by caller with full range bytes
                    entry.push((tag, Vec::new()));
                }
            }
        }
        "ext_block" => {
            for c in &n.children {
                gather_ext_known(c, &p, g);
            }
        }
        "unknown" => {}
        _ => {}
    }
}

fn gather_ext_known(n: &Node, prefix: &str, g: &mut Gathered) {
    let p = format!("{prefix}.{}", n.name);
    match n.kind.as_str() {
        "struct" | "branch" => {
            for c in &n.children {
                gather_ext_known(c, &p, g);
            }
        }
        "int" => {
            if let Some(Value::Int(v)) = &n.value {
                g.ext_ints.insert(p, *v);
            }
        }
        "bitfield" => {
            for c in &n.children {
                if let Some(Value::Int(v)) = &c.value {
                    g.ext_ints.insert(format!("{p}.{}", c.name), *v);
                }
            }
        }
        "bytes" | "fixed" | "pad" | "checksum" => {
            if let Some(Value::Bytes(b)) = &n.value {
                g.ext_raw.insert(p, b.0.clone());
            }
        }
        _ => {}
    }
}

pub fn gather(result: &ParseResult, input: &[u8]) -> Gathered {
    let mut g = Gathered::default();
    if let Some(root) = &result.root {
        gather_node(root, "", &mut g);
    }
    // Fill full extension block bytes using node ranges, in order.
    if let Some(root) = &result.root {
        for c in &root.children {
            if c.kind == "ext_container" {
                let blocks: Vec<(i64, Vec<u8>)> = c
                    .children
                    .iter()
                    .filter_map(|b| b.tag.map(|t| (t, input[b.start..b.end].to_vec())))
                    .collect();
                g.ext_blocks.insert(c.name.clone(), blocks);
            }
        }
    }
    g
}

fn apply_transform(t: &str, v: V) -> Result<V, String> {
    match v {
        V::Int(n) if t == "u16_to_u32" => Ok(V::Int(n)),
        V::Bytes(mut b) => {
            if let Some(rest) = t.strip_prefix("bytes_truncate_") {
                let n: usize = rest.parse().map_err(|_| "bad truncate".to_string())?;
                b.truncate(n);
                Ok(V::Bytes(b))
            } else if let Some(rest) = t.strip_prefix("bytes_pad_") {
                let mut it = rest.split('_');
                let n: usize = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| "bad pad".to_string())?;
                let byte = it
                    .next()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                    .ok_or_else(|| "bad pad byte".to_string())?;
                if b.len() < n {
                    b.extend(std::iter::repeat_n(byte, n - b.len()));
                }
                Ok(V::Bytes(b))
            } else {
                Err(format!("transform {t} cannot apply to bytes"))
            }
        }
        V::Int(_) => Err(format!("transform {t} cannot apply to integer")),
    }
}

// ------------------------------------------------------------- serializer

pub(crate) struct SerCtx {
    ints: BTreeMap<String, i128>,
    raw: BTreeMap<String, Vec<u8>>,
    /// extension blocks to preserve verbatim: container -> (tag, block bytes)
    preserve: BTreeMap<String, BTreeMap<i64, Vec<u8>>>,
    emitted_int: BTreeSet<String>,
}

struct SerResolver<'a> {
    ctx: &'a SerCtx,
    pos: usize,
}

impl<'a> Resolver for SerResolver<'a> {
    fn resolve(&self, p: &str) -> Result<i128, EvalError> {
        match p {
            "$pos" => Ok(self.pos as i128),
            "$eof" => Err(EvalError::UnknownPath("$eof at write time".into())),
            _ => self.ctx.ints.get(p).copied().ok_or_else(|| EvalError::UnknownPath(p.to_string())),
        }
    }
    fn byte_len(&self, p: &str) -> Result<i128, EvalError> {
        self.ctx
            .raw
            .get(p)
            .map(|b| b.len() as i128)
            .ok_or_else(|| EvalError::UnknownPath(format!("len({p})")))
    }
}

fn const_of(f: &FieldDef) -> Vec<u8> {
    if let Some(v) = &f.value {
        v.0.clone()
    } else if let Some(a) = &f.ascii {
        a.as_bytes().to_vec()
    } else {
        Vec::new()
    }
}

fn serialize_fields(ctx: &mut SerCtx, fields: &[FieldDef], prefix: &str, out: &mut Vec<u8>) -> Result<(), String> {
    for f in fields {
        let path = if prefix.is_empty() { f.name.clone() } else { format!("{prefix}.{}", f.name) };
        match f.kind.as_str() {
            "magic" => out.extend_from_slice(&const_of(f)),
            "fixed" => {
                let want = const_of(f);
                if let Some(got) = ctx.raw.get(&path) {
                    if got != &want {
                        return Err(format!("fixed `{path}` value cannot be changed by migration"));
                    }
                }
                out.extend_from_slice(&want);
            }
            "int" => {
                let v = *ctx.ints.get(&path).ok_or_else(|| format!("missing value for `{path}`"))?;
                out.extend_from_slice(&encode_int(v, f.width.unwrap(), f.endian.unwrap_or_default()));
                ctx.emitted_int.insert(path);
            }
            "bitfield" => {
                let w = f.width.unwrap();
                let mut v: i128 = 0;
                for m in &f.members {
                    let mp = format!("{path}.{}", m.name);
                    let mv = ctx.ints.get(&mp).copied().unwrap_or(0);
                    v |= (mv & ((1i128 << m.bits) - 1)) << m.lsb;
                }
                out.extend_from_slice(&encode_int(v, w, f.endian.unwrap_or_default()));
            }
            "bytes" => {
                let data = ctx.raw.get(&path).cloned().ok_or_else(|| format!("missing bytes `{path}`"))?;
                let pos = out.len();
                let r = SerResolver { ctx, pos };
                if let Some(e) = &f.length {
                    let want = expr::eval(e, &r).map_err(|e| format!("length of `{path}`: {e}"))? as usize;
                    if want != data.len() {
                        return Err(format!(
                            "bytes `{path}` length expression wants {want} but value has {}",
                            data.len()
                        ));
                    }
                }
                out.extend_from_slice(&data);
            }
            "pad" => {
                let pos = out.len();
                let r = SerResolver { ctx, pos };
                let len = expr::eval(f.length.as_deref().unwrap_or("0"), &r)
                    .map_err(|e| format!("pad `{path}`: {e}"))? as usize;
                match f.fill.as_deref() {
                    Some("raw") => {
                        // raw pads only retain data for identity; migrations zero them
                        out.extend(std::iter::repeat_n(0u8, len));
                    }
                    _ => out.extend(std::iter::repeat_n(0u8, len)),
                }
            }
            "struct" => serialize_fields(ctx, &f.fields, &path, out)?,
            "branch" => {
                let sel_path = {
                    let rel = f.selector.as_deref().unwrap_or("");
                    let parent = path.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
                    if rel.starts_with('.') || rel.contains('.') {
                        rel.trim_start_matches('.').to_string()
                    } else if parent.is_empty() {
                        rel.to_string()
                    } else {
                        format!("{parent}.{rel}")
                    }
                };
                let sel = *ctx.ints.get(&sel_path).ok_or_else(|| format!("missing selector `{sel_path}`"))?;
                let key = sel.to_string();
                let body = if f.cases.contains_key(&key) {
                    f.cases.get(&key).unwrap()
                } else if let Some(d) = &f.default_case {
                    f.cases.get(d).unwrap()
                } else {
                    return Err(format!("branch `{path}` no case for {sel}"));
                };
                serialize_fields(ctx, body, &format!("{path}.{key}"), out)?;
            }
            "array" => {
                let pos = out.len();
                let r = SerResolver { ctx, pos };
                let count = expr::eval(f.count.as_deref().unwrap_or("0"), &r)
                    .map_err(|e| format!("array `{path}` count: {e}"))? as usize;
                let el = &f.element[0];
                for i in 0..count {
                    let ep = format!("{path}.{i}");
                    match el.kind.as_str() {
                        "int" => {
                            let v = *ctx
                                .ints
                                .get(&ep)
                                .ok_or_else(|| format!("missing array element `{ep}`"))?;
                            out.extend_from_slice(&encode_int(v, el.width.unwrap(), el.endian.unwrap_or_default()));
                        }
                        "fixed" => out.extend_from_slice(&const_of(el)),
                        "bytes" => {
                            let data = ctx
                                .raw
                                .get(&ep)
                                .cloned()
                                .ok_or_else(|| format!("missing array element `{ep}`"))?;
                            out.extend_from_slice(&data);
                        }
                        "struct" => serialize_fields(ctx, &el.fields, &ep, out)?,
                        _ => return Err(format!("unsupported array element in `{path}`")),
                    }
                }
            }
            "ext_container" => {
                let tw = f.tag_width.unwrap();
                let lw = f.len_width.unwrap();
                let endian = f.ext_endian.unwrap_or_default();
                let blocks = ctx
                    .preserve
                    .remove(&f.name)
                    .unwrap_or_default();
                for (tag, block) in &blocks {
                    if block.len() < tw + lw {
                        return Err(format!("preserved extension tag {tag} too small"));
                    }
                    let body_len = block.len() - tw - lw;
                    let declared = read_int(block, tw, lw, endian, false) as usize;
                    if declared != body_len {
                        return Err(format!(
                            "preserved extension tag {tag} header length {declared} != body {body_len}"
                        ));
                    }
                    out.extend_from_slice(block);
                }
            }
            "checksum" => {
                // patched in a second pass; reserve zero bytes now
                let w = f.width.unwrap();
                out.extend(std::iter::repeat_n(0u8, w));
            }
            other => return Err(format!("unsupported field kind `{other}` at `{path}`")),
        }
    }
    Ok(())
}

pub struct Serialized {
    pub bytes: Vec<u8>,
    pub checksum_fields: Vec<ChecksumPatch>,
}

pub struct ChecksumPatch {
    pub f: FieldDef,
    pub offset: usize,
    pub width: usize,
}

pub(crate) fn serialize(spec: &FormatSpec, mut ctx: SerCtx) -> Result<Serialized, String> {
    let mut out = Vec::new();
    let mut patches: Vec<ChecksumPatch> = Vec::new();
    // Record checksum offsets by walking the same layout with a marker pass:
    serialize_with_patches(&mut ctx, &spec.fields, "", &mut out, &mut patches)?;
    // Second pass: compute checksums over the fully laid out buffer.
    for p in &patches {
        let rng = &p.f.range.clone().unwrap_or_default();
        let pos = p.offset;
        let resolver = LayoutRangeResolver { ints: &ctx.ints, raw: &ctx.raw, pos, len: out.len() };
        let start = match expr::eval_opt(&rng.start, &resolver) {
            Ok(Some(v)) => v.max(0) as usize,
            Ok(None) => 0,
            Err(e) => return Err(format!("checksum range start: {e}")),
        };
        let end = match expr::eval_opt(&rng.end, &resolver) {
            Ok(Some(v)) => v.max(0) as usize,
            Ok(None) => out.len(),
            Err(e) => return Err(format!("checksum range end: {e}")),
        };
        if start > end || end > out.len() {
            return Err(format!("checksum `{}` range {start}..{end} invalid", p.f.name));
        }
        let mut covered = out[start..end].to_vec();
        for [a, b] in &rng.exclude {
            let a = expr::eval(a, &resolver).map_err(|e| e.to_string())? as usize;
            let b = expr::eval(b, &resolver).map_err(|e| e.to_string())? as usize;
            for x in a..b {
                if x < covered.len() {
                    covered[x] = 0;
                }
            }
        }
        let calc = checksum_compute(p.f.algorithm.as_deref().unwrap_or(""), &covered);
        out[p.offset..p.offset + p.width].copy_from_slice(&calc);
    }
    Ok(Serialized { bytes: out, checksum_fields: patches })
}

fn serialize_with_patches(
    ctx: &mut SerCtx,
    fields: &[FieldDef],
    prefix: &str,
    out: &mut Vec<u8>,
    patches: &mut Vec<ChecksumPatch>,
) -> Result<(), String> {
    for f in fields {
        let path = if prefix.is_empty() { f.name.clone() } else { format!("{prefix}.{}", f.name) };
        if f.kind == "checksum" {
            let off = out.len();
            let w = f.width.unwrap();
            out.extend(std::iter::repeat_n(0u8, w));
            patches.push(ChecksumPatch { f: f.clone(), offset: off, width: w });
            continue;
        }
        match f.kind.as_str() {
            "struct" => serialize_with_patches(ctx, &f.fields, &path, out, patches)?,
            "branch" => {
                let sel_path = {
                    let rel = f.selector.as_deref().unwrap_or("");
                    let parent = path.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
                    if rel.starts_with('.') || rel.contains('.') {
                        rel.trim_start_matches('.').to_string()
                    } else if parent.is_empty() {
                        rel.to_string()
                    } else {
                        format!("{parent}.{rel}")
                    }
                };
                let sel = *ctx
                    .ints
                    .get(&sel_path)
                    .ok_or_else(|| format!("missing selector `{sel_path}`"))?;
                let key = sel.to_string();
                let body = if f.cases.contains_key(&key) {
                    f.cases.get(&key).unwrap()
                } else if let Some(d) = &f.default_case {
                    f.cases.get(d).unwrap()
                } else {
                    return Err(format!("branch `{path}` no case for {sel}"));
                };
                serialize_with_patches(ctx, body, &format!("{path}.{key}"), out, patches)?;
            }
            "array" => {
                let pos = out.len();
                let r = SerResolver { ctx, pos };
                let count = expr::eval(f.count.as_deref().unwrap_or("0"), &r)
                    .map_err(|e| format!("array `{path}` count: {e}"))? as usize;
                let el = &f.element[0];
                for i in 0..count {
                    let ep = format!("{path}.{i}");
                    if el.kind == "struct" {
                        serialize_with_patches(ctx, &el.fields, &ep, out, patches)?;
                    } else {
                        match el.kind.as_str() {
                            "int" => {
                                let v = *ctx
                                    .ints
                                    .get(&ep)
                                    .ok_or_else(|| format!("missing array element `{ep}`"))?;
                                out.extend_from_slice(&encode_int(
                                    v,
                                    el.width.unwrap(),
                                    el.endian.unwrap_or_default(),
                                ));
                            }
                            "fixed" => out.extend_from_slice(&const_of(el)),
                            "bytes" => {
                                let data = ctx
                                    .raw
                                    .get(&ep)
                                    .cloned()
                                    .ok_or_else(|| format!("missing array element `{ep}`"))?;
                                out.extend_from_slice(&data);
                            }
                            _ => return Err(format!("unsupported array element in `{path}`")),
                        }
                    }
                }
            }
            "ext_container" => {
                // emit blocks inline; no checksums inside
                let tw = f.tag_width.unwrap();
                let lw = f.len_width.unwrap();
                let endian = f.ext_endian.unwrap_or_default();
                let blocks = ctx.preserve.get(&f.name).cloned().unwrap_or_default();
                for (tag, block) in &blocks {
                    let _ = (tw, lw, endian, tag);
                    out.extend_from_slice(block);
                }
            }
            _ => {
                let one = vec![f.clone()];
                serialize_fields(ctx, &one, prefix, out)?;
            }
        }
    }
    Ok(())
}

struct LayoutRangeResolver<'a> {
    ints: &'a BTreeMap<String, i128>,
    raw: &'a BTreeMap<String, Vec<u8>>,
    pos: usize,
    len: usize,
}

impl<'a> Resolver for LayoutRangeResolver<'a> {
    fn resolve(&self, p: &str) -> Result<i128, EvalError> {
        match p {
            "$pos" => Ok(self.pos as i128),
            "$eof" => Ok(self.len as i128),
            _ => self.ints.get(p).copied().ok_or_else(|| EvalError::UnknownPath(p.to_string())),
        }
    }
    fn byte_len(&self, p: &str) -> Result<i128, EvalError> {
        self.raw.get(p).map(|b| b.len() as i128).ok_or_else(|| EvalError::UnknownPath(p.into()))
    }
}

// ------------------------------------------------------------- dry run

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldProvenance {
    pub target: String,
    pub source: String,
    /// "copy" | "constant" | "from_extension" | "auto"
    pub via: String,
    pub default: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LossItem {
    pub path: String,
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReverseItem {
    pub target: String,
    pub tier: String, // strict | semantic | lossy
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SampleRun {
    pub sample_id: String,
    pub ok: bool,
    pub error: Option<String>,
    pub output_len: usize,
    pub output_hex_preview: String,
    pub reverse_tier: String,
    pub reverse_items: Vec<ReverseItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DryRunReport {
    pub rule_name: String,
    pub rule_revision: i64,
    pub ok: bool,
    pub rule_issues: Vec<RuleIssue>,
    pub provenance: Vec<FieldProvenance>,
    pub defaults: Vec<FieldProvenance>,
    pub losses: Vec<LossItem>,
    pub layout: Vec<LayoutRegion>,
    pub samples: Vec<SampleRun>,
    /// Overall reverse tier across samples: strict < semantic < lossy.
    pub reverse_tier: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayoutRegion {
    pub name: String,
    pub kind: String,
    pub start: usize,
    pub end: usize,
}

fn layout_regions(spec: &FormatSpec, bytes: &[u8]) -> Vec<LayoutRegion> {
    let pr = crate::parse::parse_input(spec, bytes);
    let mut out = Vec::new();
    if let Some(root) = pr.root {
        for c in flatten_regions(&root) {
            out.push(c);
        }
    }
    let bytes_len = bytes.len();
    if out.is_empty() {
        out.push(LayoutRegion { name: "<output>".into(), kind: "blob".into(), start: 0, end: bytes_len });
    }
    out
}

fn flatten_regions(n: &Node) -> Vec<LayoutRegion> {
    let mut out = Vec::new();
    if n.kind != "root" && n.kind != "ext_container" {
        out.push(LayoutRegion { name: n.name.clone(), kind: n.kind.clone(), start: n.start, end: n.end });
    }
    for c in &n.children {
        out.extend(flatten_regions(c));
    }
    out
}

fn json_to_v(val: &serde_json::Value, kind: &LeafKind) -> Result<V, String> {
    match (kind, val) {
        (LeafKind::Int, serde_json::Value::Number(n)) => {
            n.as_i64().map(|i| V::Int(i as i128)).ok_or_else(|| "integer constant out of range".into())
        }
        (LeafKind::Int, serde_json::Value::Bool(b)) => Ok(V::Int(*b as i128)),
        (LeafKind::Bytes, serde_json::Value::String(h)) => {
            let cleaned: String = h.chars().filter(|c| !c.is_whitespace()).collect();
            hex::decode(&cleaned).map(V::Bytes).map_err(|e| e.to_string())
        }
        _ => Err(format!("constant {val} incompatible with target kind")),
    }
}

/// Build target serializer inputs for one sample and return provenance/loss info.
fn build_ctx(
    rule: &RuleSpec,
    dst: &FormatSpec,
    g: &Gathered,
    report: &mut DryRunReport,
) -> Result<(BTreeMap<String, i128>, BTreeMap<String, Vec<u8>>, BTreeMap<String, BTreeMap<i64, Vec<u8>>>), String>
{
    let ts = schema_leaves(dst);
    let mut ints: BTreeMap<String, i128> = BTreeMap::new();
    let mut raw: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut assigned: BTreeSet<String> = BTreeSet::new();

    let get_src = |from: &str, transform: Option<&str>| -> Result<V, String> {
        let v = if let Some(i) = g.ints.get(from) {
            V::Int(*i)
        } else if let Some(b) = g.raw.get(from) {
            V::Bytes(b.clone())
        } else if let Some(i) = g.ext_ints.get(from) {
            V::Int(*i)
        } else if let Some(b) = g.ext_raw.get(from) {
            V::Bytes(b.clone())
        } else {
            return Err(format!("source `{from}` not present in this sample"));
        };
        if let Some(t) = transform {
            apply_transform(t, v)
        } else {
            Ok(v)
        }
    };

    for m in &rule.mappings {
        match m {
            Mapping::Copy { from, to, transform } => {
                match get_src(from, transform.as_deref()) {
                    Ok(V::Int(v)) => {
                        ints.insert(to.clone(), v);
                    }
                    Ok(V::Bytes(b)) => {
                        raw.insert(to.clone(), b);
                    }
                    Err(e) => report.losses.push(LossItem {
                        path: to.clone(),
                        kind: "missing_source".into(),
                        message: e,
                    }),
                }
                assigned.insert(to.clone());
            }
            Mapping::Constant { to, value } => {
                let kind = ts.leaves.get(to).cloned().unwrap_or(LeafKind::Int);
                match json_to_v(value, &kind) {
                    Ok(V::Int(v)) => {
                        ints.insert(to.clone(), v);
                    }
                    Ok(V::Bytes(b)) => {
                        raw.insert(to.clone(), b);
                    }
                    Err(e) => return Err(e),
                }
                assigned.insert(to.clone());
            }
            Mapping::FromExtension { from, to } => {
                if let Some(i) = g.ext_ints.get(from) {
                    ints.insert(to.clone(), *i);
                } else if let Some(b) = g.ext_raw.get(from) {
                    raw.insert(to.clone(), b.clone());
                } else {
                    report.losses.push(LossItem {
                        path: to.clone(),
                        kind: "missing_extension".into(),
                        message: format!("extension leaf `{from}` absent"),
                    });
                }
                assigned.insert(to.clone());
            }
            Mapping::Drop { .. } => {}
        }
    }

    // Array copies: mappings whose `to` is an array path copy every element.
    for m in &rule.mappings {
        if let Mapping::Copy { from, to, transform } = m {
            if ts.arrays.contains_key(to) {
                let count = g.ints.get(from).copied().unwrap_or(0) as usize;
                ints.insert(to.clone(), count as i128);
                for i in 0..count {
                    let sp = format!("{from}.{i}");
                    let tp = format!("{to}.{i}");
                    match get_src(&sp, transform.as_deref()) {
                        Ok(V::Int(v)) => {
                            ints.insert(tp, v);
                        }
                        Ok(V::Bytes(b)) => {
                            raw.insert(tp, b);
                        }
                        Err(e) => report.losses.push(LossItem {
                            path: tp,
                            kind: "missing_source".into(),
                            message: e,
                        }),
                    }
                }
            }
        }
    }

    // Preserve extension blocks that are not dropped and not consumed field-wise.
    let consumed_containers: BTreeSet<String> = rule
        .mappings
        .iter()
        .filter_map(|m| match m {
            Mapping::FromExtension { from, .. } => from.split('.').next().map(|s| s.to_string()),
            _ => None,
        })
        .collect();
    let mut preserve: BTreeMap<String, BTreeMap<i64, Vec<u8>>> = BTreeMap::new();
    for (container, blocks) in &g.ext_blocks {
        // A container whose fields are lifted via from_extension is not
        // re-emitted; but explicit drop_extensions must still be recorded as
        // losses, on every container (consumed or not).
        let map = preserve.entry(container.clone()).or_default();
        for (tag, bytes) in blocks {
            if rule.drop_extensions.contains(tag) {
                report.losses.push(LossItem {
                    path: format!("{container}#{tag}"),
                    kind: "dropped_extension".into(),
                    message: format!("extension tag {tag} dropped by rule"),
                });
                continue;
            }
            if consumed_containers.contains(container) {
                // container is flattened into target leaves; nothing preserved
                continue;
            }
            map.insert(*tag, bytes.clone());
        }
    }

    // Auto fields (magic/fixed/checksum/pad) need no values; flag missing leaves.
    for (p, k) in &ts.leaves {
        if assigned.contains(p) {
            continue;
        }
        match k {
            LeafKind::Magic | LeafKind::Fixed | LeafKind::Checksum | LeafKind::Pad => {}
            LeafKind::Int | LeafKind::BitMember => {
                report.losses.push(LossItem {
                    path: p.clone(),
                    kind: "uncovered".into(),
                    message: format!("target leaf `{p}` has no value for this sample"),
                });
            }
            LeafKind::Bytes => {
                report.losses.push(LossItem {
                    path: p.clone(),
                    kind: "uncovered".into(),
                    message: format!("target bytes `{p}` has no value for this sample"),
                });
            }
        }
    }

    Ok((ints, raw, preserve))
}

// ------------------------------------------------------------- reverse verification


/// Reverse-verify one converted sample.
/// `out_bytes` is the target-format output produced by the forward migration.
pub fn reverse_check(
    rule: &RuleSpec,
    src: &FormatSpec,
    dst: &FormatSpec,
    original: &[u8],
    out_bytes: &[u8],
) -> (String, Vec<ReverseItem>) {
    let mut items: Vec<ReverseItem> = Vec::new();
    let mut tier_rank = 0; // 0 strict, 1 semantic, 2 lossy
    let bump = |rank: &mut i32, new: i32| {
        if new > *rank {
            *rank = new;
        }
    };

    // Parse the target output to read back every assigned target leaf.
    let tpr = crate::parse::parse_input(dst, out_bytes);
    let tg = gather(&tpr, out_bytes);
    let sp = crate::parse::parse_input(src, original);
    let sg = gather(&sp, original);
    let src_schema = schema_leaves(src);

    let mut patched = original.to_vec();

    for m in &rule.mappings {
        match m {
            Mapping::Drop { from } => {
                let auto = matches!(
                    src_schema.leaves.get(from),
                    Some(LeafKind::Checksum) | Some(LeafKind::Pad) | Some(LeafKind::Magic) | Some(LeafKind::Fixed)
                );
                if auto {
                    items.push(ReverseItem {
                        target: from.clone(),
                        tier: "semantic".into(),
                        detail: "auto/recomputable field dropped; regenerable on reverse".into(),
                    });
                    bump(&mut tier_rank, 1);
                } else {
                    items.push(ReverseItem {
                        target: from.clone(),
                        tier: "lossy".into(),
                        detail: "source field dropped; no reverse representation".into(),
                    });
                    bump(&mut tier_rank, 2);
                }
            }
            Mapping::Constant { to, .. } => {
                items.push(ReverseItem {
                    target: to.clone(),
                    tier: "semantic".into(),
                    detail: "constant inserted; reverse cannot recover source".into(),
                });
                bump(&mut tier_rank, 1);
            }
            Mapping::Copy { from, to, transform } => {
                // Locate target value.
                let tv_int = tg.ints.get(to);
                let tv_raw = tg.raw.get(to);
                let sv_int = sg.ints.get(from);
                let sv_raw = sg.raw.get(from);

                if let Some(tv) = tv_int {
                    if let Some(sv) = sv_int {
                        let range = leaf_byte_range(&sp, from);
                        let reencoded = encode_int_back(src, from, *tv, original, range.unwrap_or((0, 0)));
                        match reencoded {
                            Ok((bytes, _is_bit)) => {
                                if let Some((s, e)) = range {
                                    patched[s..e].copy_from_slice(&bytes);
                                }
                                if tv == sv {
                                    items.push(ReverseItem {
                                        target: to.clone(),
                                        tier: "strict".into(),
                                        detail: format!("{from} = {tv}"),
                                    });
                                } else {
                                    items.push(ReverseItem {
                                        target: to.clone(),
                                        tier: "semantic".into(),
                                        detail: format!("value {tv} != source {sv} but representable"),
                                    });
                                    bump(&mut tier_rank, 1);
                                }
                            }
                            Err(e) => {
                                items.push(ReverseItem {
                                    target: to.clone(),
                                    tier: "lossy".into(),
                                    detail: e,
                                });
                                bump(&mut tier_rank, 2);
                            }
                        }
                    } else {
                        bump(&mut tier_rank, 2);
                        items.push(ReverseItem {
                            target: to.clone(),
                            tier: "lossy".into(),
                            detail: "source integer leaf absent".into(),
                        });
                    }
                } else if let Some(tb) = tv_raw {
                    if let Some(sb) = sv_raw {
                        if let Some((s, e)) = leaf_byte_range(&sp, from) {
                            if tb.len() == e - s {
                                patched[s..e].copy_from_slice(tb);
                            }
                        }
                        if tb == sb && transform.is_none() {
                            items.push(ReverseItem {
                                target: to.clone(),
                                tier: "strict".into(),
                                detail: format!("{} bytes identical", from),
                            });
                        } else {
                            items.push(ReverseItem {
                                target: to.clone(),
                                tier: "semantic".into(),
                                detail: "bytes carried but length/value changed".into(),
                            });
                            bump(&mut tier_rank, 1);
                        }
                    } else {
                        bump(&mut tier_rank, 2);
                        items.push(ReverseItem {
                            target: to.clone(),
                            tier: "lossy".into(),
                            detail: "source bytes leaf absent".into(),
                        });
                    }
                } else {
                    items.push(ReverseItem {
                        target: to.clone(),
                        tier: "lossy".into(),
                        detail: "target leaf unreadable after conversion".into(),
                    });
                    bump(&mut tier_rank, 2);
                }
            }
            Mapping::FromExtension { from, to } => {
                items.push(ReverseItem {
                    target: to.clone(),
                    tier: "semantic".into(),
                    detail: format!("lifted from extension `{from}`; reverse would rebuild the block"),
                });
                bump(&mut tier_rank, 1);
            }
        }
    }

    // Byte-identity of the patched source vs original decides strict.
    if patched == original && tier_rank == 0 {
        ("strict".into(), items)
    } else if tier_rank <= 1 {
        ("semantic".into(), items)
    } else {
        ("lossy".into(), items)
    }
}

fn leaf_byte_range(pr: &ParseResult, path: &str) -> Option<(usize, usize)> {
    fn walk(n: &Node, parts: &[String]) -> Option<(usize, usize)> {
        if parts.is_empty() {
            return Some((n.start, n.end));
        }
        let (head, rest) = (&parts[0], &parts[1..]);
        if n.kind == "bitfield" {
            // members share the container's byte range
            if n.children.iter().any(|c| &c.name == head) {
                return Some((n.start, n.end));
            }
        }
        for c in &n.children {
            if &c.name == head {
                return walk(c, rest);
            }
        }
        None
    }
    let root = pr.root.as_ref()?;
    let parts: Vec<String> = path.split('.').map(String::from).collect();
    walk(root, &parts)
}

/// Re-encode a source integer leaf with a carried value.
/// Returns `(bytes, bit_info)` where bit_info is Some for bitfield members:
/// (lsb, bits, full-width-current-bytes are patched by caller via read-modify-write).
fn encode_int_back(spec: &FormatSpec, path: &str, v: i128, original: &[u8], range: (usize, usize)) -> Result<(Vec<u8>, Option<(usize, usize)>), String> {
    struct Found<'a> { f: &'a FieldDef, member: Option<&'a BitMember> }
    fn find<'a>(fields: &'a [FieldDef], parts: &[String]) -> Option<Found<'a>> {
        let head = &parts[0];
        let f = fields.iter().find(|x| &x.name == head)?;
        if parts.len() == 1 {
            return Some(Found { f, member: None });
        }
        match f.kind.as_str() {
            "struct" => find(&f.fields, &parts[1..]),
            "branch" => {
                let case = &parts[1];
                let body = f.cases.get(case)?;
                find(body, &parts[2..])
            }
            "array" => find(&f.element, &parts[1..]),
            "bitfield" => {
                let mname = parts.last().unwrap();
                let m = f.members.iter().find(|m| &m.name == mname)?;
                Some(Found { f, member: Some(m) })
            }
            _ => None,
        }
    }
    let parts: Vec<String> = path.split('.').map(String::from).collect();
    let found = find(&spec.fields, &parts).ok_or_else(|| format!("field {path} not found"))?;
    let f = found.f;
    if let Some(m) = found.member {
        let w = f.width.unwrap();
        let cur = read_int(original, range.0, w, f.endian.unwrap_or_default(), false);
        let mask = (1i128 << m.bits) - 1;
        let nv = (cur & !(mask << m.lsb)) | ((v & mask) << m.lsb);
        return Ok((encode_int(nv, w, f.endian.unwrap_or_default()), Some((m.lsb, m.bits))));
    }
    if f.kind != "int" {
        return Err(format!("`{path}` is not an integer field"));
    }
    Ok((encode_int(v, f.width.unwrap(), f.endian.unwrap_or_default()), None))
}

// ------------------------------------------------------------- dry-run driver

pub struct DryRunInput<'a> {
    pub rule: &'a RuleSpec,
    pub rule_revision: i64,
    pub src: &'a FormatSpec,
    pub dst: &'a FormatSpec,
    /// (sample_id, original bytes)
    pub samples: &'a [(String, Vec<u8>)],
}

pub fn run_dry_run(input: DryRunInput) -> DryRunReport {
    let DryRunInput { rule, rule_revision, src, dst, samples } = input;
    let rule_issues = validate_rule(rule, src, dst);

    let mut report = DryRunReport {
        rule_name: rule.name.clone(),
        rule_revision,
        ok: rule_issues.is_empty(),
        rule_issues,
        provenance: Vec::new(),
        defaults: Vec::new(),
        losses: Vec::new(),
        layout: Vec::new(),
        samples: Vec::new(),
        reverse_tier: "strict".into(),
    };

    // Provenance is rule-level (field sources/defaults).
    for m in &rule.mappings {
        match m {
            Mapping::Copy { from, to, transform } => report.provenance.push(FieldProvenance {
                target: to.clone(),
                source: from.clone(),
                via: "copy".into(),
                default: None,
                transform: transform.clone(),
            }),
            Mapping::Constant { to, value } => {
                let p = FieldProvenance {
                    target: to.clone(),
                    source: "<constant>".into(),
                    via: "constant".into(),
                    default: Some(value.clone()),
                    transform: None,
                };
                report.provenance.push(p.clone());
                report.defaults.push(p);
            }
            Mapping::FromExtension { from, to } => report.provenance.push(FieldProvenance {
                target: to.clone(),
                source: from.clone(),
                via: "from_extension".into(),
                default: None,
                transform: None,
            }),
            Mapping::Drop { from } => report.losses.push(LossItem {
                path: from.clone(),
                kind: "dropped".into(),
                message: "field dropped by rule".into(),
            }),
        }
    }

    let mut worst = 0i32;
    for (id, bytes) in samples {
        let spr = crate::parse::parse_input(src, bytes);
        if !spr.ok {
            report.samples.push(SampleRun {
                sample_id: id.clone(),
                ok: false,
                error: Some(
                    spr.issues
                        .iter()
                        .map(|i| format!("[{}@{}] {}", i.code, i.offset.unwrap_or(0), i.message))
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                output_len: 0,
                output_hex_preview: String::new(),
                reverse_tier: "lossy".into(),
                reverse_items: Vec::new(),
            });
            worst = 2;
            continue;
        }
        let g = gather(&spr, bytes);
        let mut sample_report = DryRunReport {
            rule_name: rule.name.clone(),
            rule_revision,
            ok: true,
            rule_issues: Vec::new(),
            provenance: Vec::new(),
            defaults: Vec::new(),
            losses: Vec::new(),
            layout: Vec::new(),
            samples: Vec::new(),
            reverse_tier: "strict".into(),
        };
        let built = build_ctx(rule, dst, &g, &mut sample_report);
        let (ints, raw, preserve) = match built {
            Ok(v) => v,
            Err(e) => {
                report.samples.push(SampleRun {
                    sample_id: id.clone(),
                    ok: false,
                    error: Some(e),
                    output_len: 0,
                    output_hex_preview: String::new(),
                    reverse_tier: "lossy".into(),
                    reverse_items: Vec::new(),
                });
                worst = 2;
                continue;
            }
        };
        report.losses.extend(sample_report.losses.clone());

        let ctx = SerCtx {

            ints,
            raw,
            preserve,
            emitted_int: BTreeSet::new(),
        };
        let serialized = match serialize(dst, ctx) {
            Ok(s) => s,
            Err(e) => {
                report.samples.push(SampleRun {
                    sample_id: id.clone(),
                    ok: false,
                    error: Some(e),
                    output_len: 0,
                    output_hex_preview: String::new(),
                    reverse_tier: "lossy".into(),
                    reverse_items: Vec::new(),
                });
                worst = 2;
                continue;
            }
        };
        let (tier, items) = reverse_check(rule, src, dst, bytes, &serialized.bytes);
        worst = worst.max(match tier.as_str() {
            "strict" => 0,
            "semantic" => 1,
            _ => 2,
        });
        if report.layout.is_empty() {
            report.layout = layout_regions(dst, &serialized.bytes);
        }
        let preview_len = serialized.bytes.len().min(32);
        report.samples.push(SampleRun {
            sample_id: id.clone(),
            ok: true,
            error: None,
            output_len: serialized.bytes.len(),
            output_hex_preview: hex::encode(&serialized.bytes[..preview_len]),
            reverse_tier: tier,
            reverse_items: items,
        });
    }

    report.ok = report.ok && report.samples.iter().all(|s| s.ok);
    report.reverse_tier = match worst {
        0 => "strict".into(),
        1 => "semantic".into(),
        _ => "lossy".into(),
    };
    report
}

/// Perform the actual conversion of one sample (used by batch conversion).
pub fn convert_one(rule: &RuleSpec, src: &FormatSpec, dst: &FormatSpec, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let spr = crate::parse::parse_input(src, bytes);
    if !spr.ok {
        return Err(spr
            .issues
            .iter()
            .map(|i| format!("[{}@{}] {}", i.code, i.offset.unwrap_or(0), i.message))
            .collect::<Vec<_>>()
            .join("; "));
    }
    let g = gather(&spr, bytes);
    let mut tmp = DryRunReport {
        rule_name: rule.name.clone(),
        rule_revision: 0,
        ok: true,
        rule_issues: Vec::new(),
        provenance: Vec::new(),
        defaults: Vec::new(),
        losses: Vec::new(),
        layout: Vec::new(),
        samples: Vec::new(),
        reverse_tier: "strict".into(),
    };
    let (ints, raw, preserve) = build_ctx(rule, dst, &g, &mut tmp)?;
    // Intentional, rule-declared losses (drop / dropped_extension) are allowed:
    // they were surfaced in the dry-run and bound at plan freeze. Only
    // unexpected losses (uncovered target leaves, missing sources) abort.
    let fatal: Vec<&LossItem> = tmp
        .losses
        .iter()
        .filter(|l| l.kind != "dropped" && l.kind != "dropped_extension")
        .collect();
    if !fatal.is_empty() {
        return Err(fatal
            .iter()
            .map(|l| format!("[{}] {}: {}", l.kind, l.path, l.message))
            .collect::<Vec<_>>()
            .join("; "));
    }
    let ctx = SerCtx { ints, raw, preserve, emitted_int: BTreeSet::new() };
    Ok(serialize(dst, ctx)?.bytes)
}
