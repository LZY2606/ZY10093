//! Binary parser: spec validation, resolution (inheritance), node-tree parse,
//! accurate-offset error reporting, gap/unknown capture and identity write-back.

use crate::expr::{self, EvalError, Resolver};
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ------------------------------------------------------------------ errors

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub code: String,
    pub message: String,
    /// Byte offset in the input the issue refers to (absent for pure spec errors).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Issue {
    pub fn spec(code: &str, message: impl Into<String>) -> Self {
        Issue { code: code.into(), message: message.into(), offset: None, path: None }
    }
    fn at(code: &str, message: impl Into<String>, offset: usize, path: &str) -> Self {
        Issue {
            code: code.into(),
            message: message.into(),
            offset: Some(offset),
            path: Some(path.to_string()),
        }
    }
}

// ------------------------------------------------------------------ values

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Value {
    Int(i128),
    Bytes(HexBytes),
}

impl Value {
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub kind: String,
    pub start: usize,
    pub end: usize,
    /// Byte range actually read for this construct's declared content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<Node>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub claimed: BTreeSet<usize>,
}

impl Node {
    fn leaf(name: &str, kind: &str, start: usize, end: usize, value: Option<Value>) -> Self {
        Node {
            name: name.to_string(),
            kind: kind.to_string(),
            start,
            end,
            value,
            tag: None,
            matched: None,
            children: Vec::new(),
            claimed: BTreeSet::new(),
        }
    }
    fn all_leaves<'a>(&'a self, prefix: &str, out: &mut Vec<(String, &'a Node)>) {
        let p = if prefix.is_empty() {
            self.name.clone()
        } else {
            format!("{prefix}.{}", self.name)
        };
        match self.kind.as_str() {
            "struct" | "branch" | "bitfield" | "ext_container" | "array" => {
                for c in &self.children {
                    c.all_leaves(&p, out);
                }
            }
            "ext_block" => {
                for c in &self.children {
                    c.all_leaves(&p, out);
                }
            }
            _ => {
                if !self.children.is_empty() {
                    for c in &self.children {
                        c.all_leaves(&p, out);
                    }
                } else {
                    out.push((p, self));
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ParseResult {
    pub ok: bool,
    pub root: Option<Node>,
    pub issues: Vec<Issue>,
    pub input_len: usize,
    /// Map of leaf path -> integer value, for migration/expression use.
    #[serde(default)]
    pub scalar_map: BTreeMap<String, i128>,
    #[serde(default)]
    pub leaf_paths: BTreeMap<String, String>,
    /// Element/byte lengths keyed by leaf path.
    #[serde(default)]
    pub byte_lens: BTreeMap<String, i128>,
}

// ------------------------------------------------------------------ validation

const KINDS: &[&str] = &[
    "magic",
    "int",
    "fixed",
    "bytes",
    "pad",
    "bitfield",
    "struct",
    "branch",
    "array",
    "ext_container",
    "checksum",
];

const ALGS: &[&str] = &["sum8", "xor8", "crc32", "crc32c"];

fn valid_name(n: &str) -> bool {
    if n.is_empty() {
        return false;
    }
    let mut ch = n.chars();
    let first = ch.next().unwrap();
    (first.is_ascii_alphabetic() || first == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn expr_ok(s: &str) -> bool {
    // Syntax-only check: evaluate with a resolver that accepts every path.
    struct All;
    impl Resolver for All {
        fn resolve(&self, _p: &str) -> Result<i128, EvalError> {
            Ok(1)
        }
        fn byte_len(&self, _p: &str) -> Result<i128, EvalError> {
            Ok(1)
        }
    }
    expr::eval(s, &All).is_ok()
}

fn validate_fields(fields: &[FieldDef], path: &str, issues: &mut Vec<Issue>, seen: &mut BTreeSet<String>) {
    for f in fields {
        let fpath = if path.is_empty() { f.name.clone() } else { format!("{path}.{}", f.name) };
        if !valid_name(&f.name) {
            issues.push(Issue::spec("bad_name", format!("field `{fpath}` has an invalid name")));
        }
        if !seen.insert(f.name.clone()) {
            issues.push(Issue::spec(
                "duplicate_field",
                format!("duplicate field name `{fpath}` (shadowing is rejected)"),
            ));
        }
        if !KINDS.contains(&f.kind.as_str()) {
            issues.push(Issue::spec("bad_kind", format!("field `{fpath}` has unknown kind `{}`", f.kind)));
            continue;
        }
        match f.kind.as_str() {
            "magic" => {
                let has_val = f.value.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
                let has_ascii = f.ascii.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
                if has_val == has_ascii {
                    issues.push(Issue::spec(
                        "magic_value",
                        format!("field `{fpath}` requires exactly one of value/ascii"),
                    ));
                }
            }
            "int" => {
                match f.width {
                    Some(w) if [1usize, 2, 4, 8, 16].contains(&w) => {}
                    _ => issues.push(Issue::spec(
                        "bad_width",
                        format!("field `{fpath}` int width must be 1,2,4,8 or 16 bytes"),
                    )),
                }
            }
            "fixed" => {
                let has_val = f.value.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
                let has_ascii = f.ascii.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
                if !has_val && !has_ascii {
                    issues.push(Issue::spec("fixed_value", format!("field `{fpath}` needs value or ascii")));
                }
            }
            "bytes" => {
                let l = f.length.as_deref().unwrap_or("");
                if l.trim().is_empty() {
                    issues.push(Issue::spec("missing_length", format!("field `{fpath}` requires length")));
                } else if !expr_ok(l) {
                    issues.push(Issue::spec("bad_expr", format!("field `{fpath}` length expression invalid")));
                }
            }
            "pad" => {
                let l = f.length.as_deref().unwrap_or("");
                if l.trim().is_empty() {
                    issues.push(Issue::spec("missing_length", format!("field `{fpath}` pad requires length")));
                } else if !expr_ok(l) {
                    issues.push(Issue::spec("bad_expr", format!("field `{fpath}` pad length invalid")));
                }
                match f.fill.as_deref() {
                    Some("zero") | Some("raw") | None => {}
                    Some(x) => issues.push(Issue::spec(
                        "bad_fill",
                        format!("field `{fpath}` fill must be zero|raw, got {x}"),
                    )),
                }
            }
            "bitfield" => {
                match f.width {
                    Some(w) if w >= 1 && w <= 16 => {}
                    _ => issues.push(Issue::spec(
                        "bad_width",
                        format!("field `{fpath}` bitfield width must be 1..=16 bytes"),
                    )),
                }
                if f.members.is_empty() {
                    issues.push(Issue::spec("empty_bitfield", format!("field `{fpath}` has no members")));
                }
                let bits = f.width.unwrap_or(0) * 8;
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                let mut mseen = BTreeSet::new();
                for m in &f.members {
                    if !valid_name(&m.name) || !mseen.insert(m.name.clone()) {
                        issues.push(Issue::spec(
                            "bad_member",
                            format!("field `{fpath}` has duplicate/invalid member `{}`", m.name),
                        ));
                    }
                    if m.bits == 0 || m.lsb.checked_add(m.bits).map_or(true, |e| e > bits) {
                        issues.push(Issue::spec(
                            "member_span",
                            format!("member `{}.{}` exceeds bitfield width", fpath, m.name),
                        ));
                    }
                    ranges.push((m.lsb, m.lsb + m.bits));
                }
                ranges.sort();
                for w in ranges.windows(2) {
                    if w[0].1 > w[1].0 {
                        issues.push(Issue::spec(
                            "overlap_bitfield",
                            format!("field `{fpath}` members overlap at bits {}..{}", w[1].0, w[0].1),
                        ));
                        break;
                    }
                }
            }
            "struct" => {
                if f.fields.is_empty() {
                    issues.push(Issue::spec("empty_struct", format!("field `{fpath}` has no fields")));
                }
                validate_fields(&f.fields, &fpath, issues, &mut BTreeSet::new());
            }
            "branch" => {
                if f.selector.as_deref().unwrap_or("").trim().is_empty() {
                    issues.push(Issue::spec("missing_selector", format!("field `{fpath}` needs selector")));
                }
                if f.cases.is_empty() {
                    issues.push(Issue::spec("empty_branch", format!("field `{fpath}` has no cases")));
                }
                for (case, body) in &f.cases {
                    if case.parse::<i128>().is_err() {
                        issues.push(Issue::spec(
                            "bad_case",
                            format!("field `{fpath}` case key `{case}` must be an integer"),
                        ));
                    }
                    validate_fields(body, &format!("{fpath}[{case}]"), issues, &mut BTreeSet::new());
                }
                if let Some(d) = &f.default_case {
                    if !f.cases.contains_key(d) {
                        issues.push(Issue::spec(
                            "bad_default",
                            format!("field `{fpath}` default_case `{d}` is not a defined case"),
                        ));
                    }
                }
            }
            "array" => {
                let c = f.count.as_deref().unwrap_or("");
                if c.trim().is_empty() || !expr_ok(c) {
                    issues.push(Issue::spec("bad_count", format!("field `{fpath}` needs a count expression")));
                }
                if f.element.len() != 1 {
                    issues.push(Issue::spec(
                        "bad_element",
                        format!("field `{fpath}` element must contain exactly one field"),
                    ));
                } else {
                    let el = &f.element[0];
                    if !matches!(el.kind.as_str(), "int" | "bytes" | "fixed" | "struct") {
                        issues.push(Issue::spec(
                            "bad_element",
                            format!("field `{fpath}` element kind must be int|bytes|fixed|struct"),
                        ));
                    }
                    if el.kind == "bytes" {
                        // element byte length must be a fixed numeric, not a field reference
                        if let Some(l) = &el.length {
                            let references_field =
                                l.chars().any(|ch| ch.is_ascii_alphabetic() || ch == '$' || ch == '_');
                            if references_field {
                                issues.push(Issue::spec(
                                    "dynamic_array_element",
                                    format!("field `{fpath}` bytes elements must have a constant length"),
                                ));
                            }
                        }
                    }
                    let mut es = BTreeSet::new();
                    validate_fields(&f.element, &format!("{fpath}[]"), issues, &mut es);
                }
            }
            "ext_container" => {
                if !matches!(f.tag_width, Some(1) | Some(2) | Some(4) | Some(8)) {
                    issues.push(Issue::spec("bad_tag_width", format!("field `{fpath}` tag_width must be 1,2,4,8")));
                }
                if !matches!(f.len_width, Some(1) | Some(2) | Some(4) | Some(8)) {
                    issues.push(Issue::spec("bad_len_width", format!("field `{fpath}` len_width must be 1,2,4,8")));
                }
                let mut tags = BTreeSet::new();
                let mut enames = BTreeSet::new();
                for e in &f.extensions {
                    if !tags.insert(e.tag) {
                        issues.push(Issue::spec(
                            "dup_ext_tag",
                            format!("field `{fpath}` repeats extension tag {}", e.tag),
                        ));
                    }
                    if !valid_name(&e.name) || !enames.insert(e.name.clone()) {
                        issues.push(Issue::spec(
                            "bad_ext_name",
                            format!("field `{fpath}` extension `{}` invalid/duplicate", e.name),
                        ));
                    }
                    validate_fields(&e.fields, &format!("{fpath}.{}", e.name), issues, &mut BTreeSet::new());
                }
            }
            "checksum" => {
                if !f.algorithm.as_deref().map(|a| ALGS.contains(&a)).unwrap_or(false) {
                    issues.push(Issue::spec(
                        "bad_algorithm",
                        format!("field `{fpath}` algorithm must be one of {ALGS:?}"),
                    ));
                }
                match f.width {
                    Some(w) if [1usize, 4].contains(&w) => {}
                    _ => issues.push(Issue::spec(
                        "bad_checksum_width",
                        format!("field `{fpath}` checksum width must be 1 or 4 bytes"),
                    )),
                }
                if let Some(r) = &f.range {
                    if !r.start.trim().is_empty() && !expr_ok(&r.start) {
                        issues.push(Issue::spec("bad_expr", format!("field `{fpath}` range.start invalid")));
                    }
                    if !r.end.trim().is_empty() && !expr_ok(&r.end) {
                        issues.push(Issue::spec("bad_expr", format!("field `{fpath}` range.end invalid")));
                    }
                    for [a, b] in &r.exclude {
                        if !expr_ok(a) || !expr_ok(b) {
                            issues.push(Issue::spec("bad_expr", format!("field `{fpath}` range.exclude invalid")));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Validate a single (already inheritance-resolved) format definition.
pub fn validate_resolved(spec: &FormatSpec) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !valid_name(&spec.name) {
        issues.push(Issue::spec("bad_name", "format name must be [A-Za-z_][A-Za-z0-9_]*"));
    }
    if spec.version.trim().is_empty() {
        issues.push(Issue::spec("bad_version", "format version label is empty"));
    }
    let mut seen = BTreeSet::new();
    validate_fields(&spec.fields, "", &mut issues, &mut seen);
    issues
}


// ------------------------------------------------------------------ checksum math

pub fn checksum_compute(alg: &str, data: &[u8]) -> Vec<u8> {
    match alg {
        "sum8" => vec![data.iter().map(|b| *b as u16).sum::<u16>() as u8],
        "xor8" => vec![data.iter().fold(0u8, |a, b| a ^ b)],
        "crc32" => crc32(data, 0xEDB8_8842).to_be_bytes().to_vec(),

        "crc32c" => crc32(data, 0x82F6_3B78).to_be_bytes().to_vec(),
        _ => Vec::new(),
    }
}

fn crc32(data: &[u8], poly: u32) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ poly } else { crc >> 1 };
        }
    }
    !crc
}

// ------------------------------------------------------------------ runtime

struct Runtime<'a> {
    input: &'a [u8],
    ints: BTreeMap<String, i128>,
    byte_lens: BTreeMap<String, i128>,
    issues: Vec<Issue>,
    fatal: bool,
}

struct RtResolver<'a> {
    rt: &'a Runtime<'a>,
    pos: usize,
    eof: usize,
}

impl<'a> Resolver for RtResolver<'a> {
    fn resolve(&self, p: &str) -> Result<i128, EvalError> {
        match p {
            "$pos" => Ok(self.pos as i128),
            "$eof" => Ok(self.eof as i128),
            _ => self.rt.ints.get(p).copied().ok_or_else(|| EvalError::UnknownPath(p.to_string())),
        }
    }
    fn byte_len(&self, p: &str) -> Result<i128, EvalError> {
        self.rt.byte_lens.get(p).copied().ok_or_else(|| EvalError::UnknownPath(format!("len({p})")))
    }
}

pub fn read_int(input: &[u8], pos: usize, width: usize, endian: Endian, signed: bool) -> i128 {
    let chunk = &input[pos..pos + width];
    let bytes_be: Vec<u8> = match endian {
        Endian::Big => chunk.to_vec(),
        Endian::Little => chunk.iter().rev().copied().collect(),
    };
    let neg = signed && bytes_be.first().map(|b| b & 0x80 != 0).unwrap_or(false);
    let mut v: i128 = 0;
    for b in &bytes_be {
        v = (v << 8) | *b as i128;
    }
    if neg {
        v |= -1i128 << (width * 8);
    }
    v
}

pub fn encode_int(v: i128, width: usize, endian: Endian) -> Vec<u8> {
    let mut out = vec![0u8; width];
    for i in 0..width {
        out[i] = ((v >> ((width - 1 - i) * 8)) & 0xff) as u8;
    }
    if matches!(endian, Endian::Little) {
        out.reverse();
    }
    out
}

fn const_bytes(f: &FieldDef) -> Vec<u8> {
    if let Some(v) = &f.value {
        v.0.clone()
    } else if let Some(a) = &f.ascii {
        a.as_bytes().to_vec()
    } else {
        Vec::new()
    }
}

fn fail(rt: &mut Runtime, code: &str, msg: String, offset: usize, path: &str) {
    rt.issues.push(Issue::at(code, msg, offset, path));
    rt.fatal = true;
}

fn eval_length(rt: &Runtime, expr_s: &str, pos: usize, eof: usize) -> Result<usize, Issue> {
    let r = RtResolver { rt, pos, eof };
    match expr::eval(expr_s, &r) {
        Ok(v) if v >= 0 => Ok(v as usize),
        Ok(_) => Err(Issue {
            code: "bad_length".into(),
            message: "negative length expression".into(),
            offset: Some(pos),
            path: None,
        }),
        Err(e) => Err(Issue {
            code: "bad_length".into(),
            message: e.to_string(),
            offset: Some(pos),
            path: None,
        }),
    }
}

/// Static byte size of a fixed layout; None if dynamic.

fn parse_usize_lit(s: &str) -> Option<usize> {
    let s = s.trim();
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        usize::from_str_radix(h, 16).ok()?
    } else {
        s.parse().ok()?
    };
    Some(v)
}

/// Parse a bounded scope. Returns sibling nodes.
/// `bounded` struct/ext-block scopes reject trailing bytes; root/branch/extcontainer consume to end.
fn parse_fields(
    rt: &mut Runtime,
    fields: &[FieldDef],
    prefix: &str,
    pos: &mut usize,
    scope_end: usize,
    bounded: bool,
) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut intervals: Vec<(usize, usize, String)> = Vec::new();
    let scope_start = *pos;

    for f in fields {
        if rt.fatal {
            return nodes;
        }
        let path = if prefix.is_empty() { f.name.clone() } else { format!("{prefix}.{}", f.name) };
        let start = *pos;

        match f.kind.as_str() {
            "magic" => {
                let want = const_bytes(f);
                if start + want.len() > rt.input.len() {
                    fail(rt, "short_read", format!("magic `{path}` needs {} bytes", want.len()), start, &path);
                    return nodes;
                }
                let got = &rt.input[start..start + want.len()];
                *pos += want.len();
                if got != want.as_slice() {
                    fail(
                        rt,
                        "magic_mismatch",
                        format!("expected {}, got {}", hex::encode(&want), hex::encode(got)),
                        start,
                        &path,
                    );
                    return nodes;
                }
                let mut n = Node::leaf(&f.name, "magic", start, *pos, Some(Value::Bytes(HexBytes(want))));
                n.value = Some(Value::Bytes(HexBytes(got.to_vec())));
                nodes.push(n);
            }
            "fixed" => {
                let want = const_bytes(f);
                if start + want.len() > rt.input.len() {
                    fail(rt, "short_read", format!("fixed `{path}` overrun"), start, &path);
                    return nodes;
                }
                let got = rt.input[start..start + want.len()].to_vec();
                *pos += want.len();
                if got != want {
                    rt.issues.push(Issue::at(
                        "fixed_mismatch",
                        format!("expected {}, got {}", hex::encode(&want), hex::encode(&got)),
                        start,
                        &path,
                    ));
                }
                intervals.push((start, *pos, path.clone()));
                nodes.push(Node::leaf(&f.name, "fixed", start, *pos, Some(Value::Bytes(HexBytes(got)))));
            }
            "int" => {
                let w = f.width.unwrap();
                if start + w > rt.input.len() {
                    fail(rt, "short_read", format!("int `{path}` overrun"), start, &path);
                    return nodes;
                }
                let v = read_int(rt.input, start, w, f.endian.unwrap_or_default(), f.signed.unwrap_or(false));
                *pos += w;
                rt.ints.insert(path.clone(), v);
                intervals.push((start, *pos, path.clone()));
                nodes.push(Node::leaf(&f.name, "int", start, *pos, Some(Value::Int(v))));
            }
            "bitfield" => {
                let w = f.width.unwrap();
                if start + w > rt.input.len() {
                    fail(rt, "short_read", format!("bitfield `{path}` overrun"), start, &path);
                    return nodes;
                }
                let v = read_int(rt.input, start, w, f.endian.unwrap_or_default(), false);
                *pos += w;
                let mut children = Vec::new();
                for m in &f.members {
                    let mv = (v >> m.lsb) & ((1i128 << m.bits) - 1);
                    let mp = format!("{path}.{}", m.name);
                    rt.ints.insert(mp, mv);
                    children.push(Node::leaf(&m.name, "int", start, *pos, Some(Value::Int(mv))));
                }
                intervals.push((start, *pos, path.clone()));
                nodes.push(Node {
                    name: f.name.clone(),
                    kind: "bitfield".into(),
                    start,
                    end: *pos,
                    value: Some(Value::Int(v)),
                    tag: None,
                    matched: None,
                    children,
                    claimed: BTreeSet::new(),
                });
            }
            "bytes" => {
                let e = f.length.as_deref().unwrap_or("");
                let len = match eval_length(rt, e, start, scope_end) {
                    Ok(v) => v,
                    Err(mut is) => {
                        is.path = Some(path.clone());
                        rt.issues.push(is);
                        rt.fatal = true;
                        return nodes;
                    }
                };
                if start.checked_add(len).map_or(true, |end| end > rt.input.len()) {
                    fail(
                        rt,
                        "length_out_of_bounds",
                        format!("bytes `{path}` length {len} exceeds input"),
                        start,
                        &path,
                    );
                    return nodes;
                }
                if start + len > scope_end {
                    fail(
                        rt,
                        "length_out_of_bounds",
                        format!("bytes `{path}` length {len} crosses scope end {scope_end}"),
                        scope_end,
                        &path,
                    );
                    return nodes;
                }
                let data = rt.input[start..start + len].to_vec();
                *pos += len;
                rt.byte_lens.insert(path.clone(), len as i128);
                intervals.push((start, *pos, path.clone()));
                nodes.push(Node::leaf(&f.name, "bytes", start, *pos, Some(Value::Bytes(HexBytes(data)))));
            }
            "pad" => {
                let e = f.length.as_deref().unwrap_or("");
                let len = match eval_length(rt, e, start, scope_end) {
                    Ok(v) => v,
                    Err(mut is) => {
                        is.path = Some(path.clone());
                        rt.issues.push(is);
                        rt.fatal = true;
                        return nodes;
                    }
                };
                if start + len > scope_end.min(rt.input.len()) {
                    fail(rt, "short_read", format!("pad `{path}` overrun"), start, &path);
                    return nodes;
                }
                let raw = rt.input[start..start + len].to_vec();
                if f.fill.as_deref() == Some("zero") && raw.iter().any(|b| *b != 0) {
                    rt.issues.push(Issue::at(
                        "pad_nonzero",
                        "declared zero pad contains non-zero bytes",
                        start,
                        &path,
                    ));
                }
                *pos += len;
                intervals.push((start, *pos, path.clone()));
                nodes.push(Node::leaf(&f.name, "pad", start, *pos, Some(Value::Bytes(HexBytes(raw)))));
            }
            "struct" => {
                let children = parse_fields(rt, &f.fields, &path, pos, scope_end, true);
                nodes.push(Node {
                    name: f.name.clone(),
                    kind: "struct".into(),
                    start,
                    end: *pos,
                    value: None,
                    tag: None,
                    matched: None,
                    children,
                    claimed: BTreeSet::new(),
                });
            }
            "branch" => {
                let node = parse_branch(rt, f, &path, pos, scope_end);
                nodes.push(node);
            }
            "array" => {
                let node = parse_array(rt, f, &path, pos, scope_end);
                nodes.push(node);
            }
            "ext_container" => {
                let node = parse_ext_container(rt, f, &path, pos, scope_end, &mut intervals);
                nodes.push(node);
            }
            "checksum" => {
                let node = parse_checksum(rt, f, &path, pos, scope_end);
                if let Some(n) = node {
                    nodes.push(n);
                }
            }
            other => {
                fail(rt, "bad_kind", format!("unsupported field kind `{other}`"), start, &path);
                return nodes;
            }
        }
    }

    if rt.fatal {
        return nodes;
    }

    // overlap detection among declared leaf intervals in this scope
    intervals.sort_by_key(|(s, e, _)| (*s, *e));
    for w in intervals.windows(2) {
        if w[0].1 > w[1].0 {
            rt.issues.push(Issue::at(
                "overlap_fields",
                format!("fields `{}` and `{}` overlap at offset {}", w[0].2, w[1].2, w[1].0),
                w[1].0,
                &w[1].2,
            ));
        }
    }

    if *pos < scope_end {
        if bounded {
            rt.issues.push(Issue::at(
                "scope_underrun",
                format!("scope ending at {scope_end} finished early at {}", *pos),
                *pos,
                prefix,
            ));
        } else {
            let gap = rt.input[*pos..scope_end].to_vec();
            nodes.push(Node::leaf("", "unknown", *pos, scope_end, Some(Value::Bytes(HexBytes(gap)))));
            *pos = scope_end;
        }
    }
    let _ = scope_start;
    nodes
}

fn parse_branch(rt: &mut Runtime, f: &FieldDef, path: &str, pos: &mut usize, scope_end: usize) -> Node {
    let start = *pos;
    let sel_rel = f.selector.as_deref().unwrap_or("");
    let sel_path = if sel_rel.starts_with('.') || sel_rel.contains('.') {
        // relative-to-branch-parent: strip leading dot and prefix with the branch parent
        sel_rel.trim_start_matches('.').to_string()
    } else {
        // sibling selector: same prefix as this branch
        let parent = path.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
        if parent.is_empty() { sel_rel.to_string() } else { format!("{parent}.{sel_rel}") }
    };
    let sel = match rt.ints.get(&sel_path) {
        Some(v) => *v,
        None => {
            fail(
                rt,
                "bad_selector",
                format!("branch `{path}` selector `{sel_rel}` not found"),
                start,
                path,
            );
            return Node::leaf(&f.name, "branch", start, *pos, None);
        }
    };
    let key = sel.to_string();
    let (matched, body) = if let Some(b) = f.cases.get(&key) {
        (key, b)
    } else if let Some(d) = &f.default_case {
        (d.clone(), f.cases.get(d).expect("validated default"))
    } else {
        fail(
            rt,
            "no_branch_case",
            format!("branch `{path}` has no case for selector {sel}"),
            start,
            path,
        );
        return Node::leaf(&f.name, "branch", start, *pos, None);
    };
    let children = parse_fields(rt, body, &format!("{path}.{matched}"), pos, scope_end, false);
    Node {
        name: f.name.clone(),
        kind: "branch".into(),
        start,
        end: *pos,
        value: Some(Value::Int(sel)),
        tag: None,
        matched: Some(matched),
        children,
        claimed: BTreeSet::new(),
    }
}

fn parse_array(rt: &mut Runtime, f: &FieldDef, path: &str, pos: &mut usize, scope_end: usize) -> Node {
    let start = *pos;
    let count_expr = f.count.as_deref().unwrap_or("");
    let r = RtResolver { rt, pos: start, eof: scope_end };
    let count = match expr::eval(count_expr, &r) {
        Ok(v) if v >= 0 => v as usize,
        _ => {
            fail(rt, "bad_count", format!("array `{path}` count invalid"), start, path);
            return Node::leaf(&f.name, "array", start, *pos, None);
        }
    };
    let el = &f.element[0];
    let mut children: Vec<Node> = Vec::new();
    for i in 0..count {
        if rt.fatal {
            break;
        }
        let epath = format!("{path}.{i}");
        let estart = *pos;
        match el.kind.as_str() {
            "int" => {
                let w = el.width.unwrap();
                if *pos + w > rt.input.len() || *pos + w > scope_end {
                    fail(rt, "short_read", format!("array `{path}` element {i} overrun"), estart, &epath);
                    break;
                }
                let v = read_int(rt.input, *pos, w, el.endian.unwrap_or_default(), el.signed.unwrap_or(false));
                *pos += w;
                rt.ints.insert(epath.clone(), v);
                rt.byte_lens.insert(path.to_string(), count as i128);
                children.push(Node::leaf(&i.to_string(), "int", estart, *pos, Some(Value::Int(v))));
            }
            "fixed" => {
                let want = const_bytes(el);
                if *pos + want.len() > scope_end.min(rt.input.len()) {
                    fail(rt, "short_read", format!("array `{path}` element {i} overrun"), estart, &epath);
                    break;
                }
                let got = rt.input[*pos..*pos + want.len()].to_vec();
                *pos += want.len();
                if got != want {
                    rt.issues.push(Issue::at(
                        "fixed_mismatch",
                        format!("array `{path}` element {i} mismatch"),
                        estart,
                        &epath,
                    ));
                }
                children.push(Node::leaf(&i.to_string(), "fixed", estart, *pos, Some(Value::Bytes(HexBytes(got)))));
            }
            "bytes" => {
                let len = parse_usize_lit(el.length.as_deref().unwrap_or("")).unwrap_or(0);
                if *pos + len > scope_end.min(rt.input.len()) {
                    fail(rt, "length_out_of_bounds", format!("array `{path}` element {i} overrun"), estart, &epath);
                    break;
                }
                let data = rt.input[*pos..*pos + len].to_vec();
                *pos += len;
                rt.byte_lens.insert(epath.clone(), len as i128);
                rt.byte_lens.insert(path.to_string(), count as i128);
                children.push(Node::leaf(&i.to_string(), "bytes", estart, *pos, Some(Value::Bytes(HexBytes(data)))));
            }
            "struct" => {
                let body = parse_fields(rt, &el.fields, &epath, pos, scope_end, true);
                children.push(Node {
                    name: i.to_string(),
                    kind: "struct".into(),
                    start: estart,
                    end: *pos,
                    value: None,
                    tag: None,
                    matched: None,
                    children: body,
                    claimed: BTreeSet::new(),
                });
            }
            _ => {
                fail(rt, "bad_element", format!("array `{path}` unsupported element kind"), estart, &epath);
            }
        }
    }
    rt.byte_lens.entry(path.to_string()).or_insert(count as i128);
    Node {
        name: f.name.clone(),
        kind: "array".into(),
        start,
        end: *pos,
        value: Some(Value::Int(count as i128)),
        tag: None,
        matched: None,
        children,
        claimed: BTreeSet::new(),
    }
}

fn parse_ext_container(
    rt: &mut Runtime,
    f: &FieldDef,
    path: &str,
    pos: &mut usize,
    scope_end: usize,
    intervals: &mut Vec<(usize, usize, String)>,
) -> Node {
    let start = *pos;
    let tw = f.tag_width.unwrap();
    let lw = f.len_width.unwrap();
    let endian = f.ext_endian.unwrap_or_default();
    let mut children: Vec<Node> = Vec::new();
    let mut tags_seen: BTreeMap<i64, usize> = BTreeMap::new();

    while *pos < scope_end && !rt.fatal {
        let bstart = *pos;
        if *pos + tw + lw > scope_end {
            fail(
                rt,
                "short_read",
                format!("ext_container `{path}` truncated header at {bstart}"),
                bstart,
                path,
            );
            break;
        }
        let tag128 = read_int(rt.input, *pos, tw, endian, false);
        let tag: i64 = match tag128.try_into() {
            Ok(t) => t,
            Err(_) => {
                fail(
                    rt,
                    "bad_tag",
                    format!("ext_container `{path}` tag value {tag128} does not fit i64"),
                    bstart,
                    path,
                );
                break;
            }
        };
        *pos += tw;
        let body_len = read_int(rt.input, *pos, lw, endian, false) as usize;
        *pos += lw;
        if *pos + body_len > scope_end {
            fail(
                rt,
                "length_out_of_bounds",
                format!("ext_container `{path}` block tag {tag} length {body_len} overruns container end {scope_end}"),
                bstart,
                path,
            );
            break;
        }
        let block_end = *pos + body_len;
        if let Some(prev) = tags_seen.get(&tag) {
            rt.issues.push(Issue::at(
                "overlap_fields",
                format!("ext_container `{path}` duplicate tag {tag} (first at {prev})"),
                bstart,
                path,
            ));
        }
        tags_seen.insert(tag, bstart);

        let def = f.extensions.iter().find(|e| e.tag == tag);
        let node = match def {
            Some(e) => {
                let body = parse_fields(rt, &e.fields, &format!("{path}.{}", e.name), pos, block_end, true);
                *pos = block_end;
                Node {
                    name: e.name.clone(),
                    kind: "ext_block".into(),
                    start: bstart,
                    end: block_end,
                    value: None,
                    tag: Some(tag),
                    matched: Some("known".into()),
                    children: body,
                    claimed: BTreeSet::new(),
                }
            }
            None => {
                let raw = rt.input[*pos..block_end].to_vec();
                *pos = block_end;
                Node {
                    name: format!("unknown_{tag}"),
                    kind: "ext_block".into(),
                    start: bstart,
                    end: block_end,
                    value: Some(Value::Bytes(HexBytes(raw))),
                    tag: Some(tag),
                    matched: Some("unknown".into()),
                    children: Vec::new(),
                    claimed: BTreeSet::new(),
                }
            }
        };
        intervals.push((bstart, block_end, format!("{path}#tag{tag}")));
        children.push(node);
    }
    Node {
        name: f.name.clone(),
        kind: "ext_container".into(),
        start,
        end: *pos,
        value: None,
        tag: None,
        matched: None,
        children,
        claimed: BTreeSet::new(),
    }
}

fn parse_checksum(
    rt: &mut Runtime,
    f: &FieldDef,
    path: &str,
    pos: &mut usize,
    scope_end: usize,
) -> Option<Node> {
    let start = *pos;
    let w = f.width.unwrap();
    if start + w > rt.input.len() {
        fail(rt, "short_read", format!("checksum `{path}` overrun"), start, path);
        return None;
    }
    let stored = rt.input[start..start + w].to_vec();
    let stored_val = read_int(rt.input, start, w, Endian::Big, false);
    *pos += w;

    let range = f.range.clone().unwrap_or_default();
    let r = RtResolver { rt, pos: start, eof: scope_end };
    let rs = match expr::eval_opt(&range.start, &r) {
        Ok(Some(v)) => v.max(0) as usize,
        Ok(None) => 0,
        Err(e) => {
            fail(rt, "bad_range", format!("checksum `{path}` start: {e}"), start, path);
            return None;
        }
    };
    let re = match expr::eval_opt(&range.end, &r) {
        Ok(Some(v)) => v.max(0) as usize,
        Ok(None) => rt.input.len(),
        Err(e) => {
            fail(rt, "bad_range", format!("checksum `{path}` end: {e}"), start, path);
            return None;
        }
    };
    if rs > re || re > rt.input.len() {
        fail(
            rt,
            "checksum_range",
            format!("checksum `{path}` range {rs}..{re} out of bounds (input {})", rt.input.len()),
            start,
            path,
        );
        return None;
    }
    let mut covered: Vec<u8> = rt.input[rs..re].to_vec();
    let mut excludes: Vec<(usize, usize)> = Vec::new();
    for [a, b] in &range.exclude {
        let a = expr::eval(a, &r).ok()? as usize;
        let b = expr::eval(b, &r).ok()? as usize;
        excludes.push((a, b));
    }
    // checksum self-reference: the field's own bytes lie inside its coverage
    if start < re && start + w > rs {
        let self_excluded = excludes.iter().any(|(a, b)| *a <= start && start + w <= *b);
        if !self_excluded {
            fail(
                rt,
                "checksum_self_reference",
                format!(
                    "checksum `{path}` at {start}..{} covers its own bytes (range {rs}..{re}); add an exclude",
                    start + w
                ),
                start,
                path,
            );
            return None;
        }
    }
    for (a, b) in &excludes {
        for x in *a..*b {
            if x < covered.len() {
                covered[x] = 0;
            }
        }
    }
    let calc = checksum_compute(f.algorithm.as_deref().unwrap_or(""), &covered);
    let calc_val = calc.iter().fold(0i128, |a, b| (a << 8) | *b as i128);
    if calc_val != stored_val {
        rt.issues.push(Issue::at(
            "checksum_mismatch",
            format!(
                "checksum `{path}` stored 0x{} != computed 0x{} over {}..{}",
                hex::encode(&stored),
                hex::encode(&calc),
                rs,
                re
            ),
            start,
            path,
        ));
    }
    rt.ints.insert(path.to_string(), stored_val);
    Some(Node::leaf(&f.name, "checksum", start, *pos, Some(Value::Bytes(HexBytes(stored)))))
}

// ------------------------------------------------------------------ public API

/// Parse raw input against a validated, inheritance-resolved spec.
pub fn parse_input(spec: &FormatSpec, input: &[u8]) -> ParseResult {
    let mut rt = Runtime { input, ints: BTreeMap::new(), byte_lens: BTreeMap::new(), issues: Vec::new(), fatal: false };
    let mut pos = 0usize;
    let nodes = parse_fields(&mut rt, &spec.fields, "", &mut pos, input.len(), false);
    let ok = !rt.fatal && !rt.issues.iter().any(|i| {
        matches!(
            i.code.as_str(),
            "short_read"
                | "length_out_of_bounds"
                | "overlap_fields"
                | "magic_mismatch"
                | "checksum_self_reference"
                | "no_branch_case"
                | "bad_selector"
                | "bad_count"
                | "bad_length"
        )
    });
    let root = Node {
        name: format!("{}@{}", spec.name, spec.version),
        kind: "root".into(),
        start: 0,
        end: input.len(),
        value: None,
        tag: None,
        matched: None,
        children: nodes,
        claimed: BTreeSet::new(),
    };

    // Build scalar/leaf maps for migration.
    let mut scalar_map = rt.ints.clone();
    scalar_map.insert("$eof".into(), input.len() as i128);
    let mut leaf_paths: BTreeMap<String, String> = BTreeMap::new();
    let mut leaves: Vec<(String, &Node)> = Vec::new();
    for c in &root.children {
        c.all_leaves("", &mut leaves);
    }
    for (p, n) in &leaves {
        leaf_paths.insert(p.clone(), n.kind.to_string());
    }

    ParseResult {
        ok,
        root: Some(root),
        issues: rt.issues,
        input_len: input.len(),
        scalar_map,
        leaf_paths,
        byte_lens: rt.byte_lens,
    }
}

fn is_container(n: &Node) -> bool {
    matches!(n.kind.as_str(), "struct" | "branch" | "ext_container" | "array")
}

fn emit_identity(n: &Node, input: &[u8], out: &mut Vec<u8>) {
    if is_container(n) {
        for c in &n.children {
            emit_identity(c, input, out);
        }
    } else {
        out.extend_from_slice(&input[n.start..n.end]);
    }
}

/// Write parsed data back untouched. Guarantees byte-identity with the input when
/// parsing succeeded: the leaf/ext-block/unknown partition covers every byte.
pub fn write_identity(result: &ParseResult, input: &[u8]) -> Result<Vec<u8>, String> {
    let root = result.root.as_ref().ok_or_else(|| "no parse tree".to_string())?;
    let mut out = Vec::with_capacity(input.len());
    emit_identity(root, input, &mut out);
    if out != input {
        // Locate the first divergence for an accurate report.
        let at = out.iter().zip(input).position(|(a, b)| a != b).unwrap_or(out.len().min(input.len()));
        return Err(format!(
            "identity write-back diverged at offset {at}: wrote {} bytes vs {} input",
            out.len(),
            input.len()
        ));
    }
    Ok(out)
}

// ------------------------------------------------------------ inheritance resolution

/// Lookup function for a parent definition by name (returns its resolved fields).
pub trait ParentLookup {
    fn resolve_parent(&self, name: &str, revision: i64) -> Result<Vec<FieldDef>, Issue>;
}

/// Resolve inheritance: parent fields (fully flattened, top-down) are prepended.
/// Top-level same-name shadowing is rejected; `chain` tracks every visited
/// definition so multi-level cycles are detected.
pub fn resolve_inheritance<L: ParentLookup>(
    spec: &FormatSpec,
    lookup: &L,
    chain: &mut Vec<String>,
) -> Result<FormatSpec, Issue> {
    let key = format!("{}#{}", spec.name, spec.version);
    if chain.iter().any(|c| c == &key) {
        return Err(Issue::spec(
            "inherit_cycle",
            format!("inheritance cycle detected through {key}; chain: {}", chain.join(" -> ")),
        ));
    }
    chain.push(key.clone());

    let mut merged: Vec<FieldDef> = Vec::new();
    let mut names: BTreeSet<String> = BTreeSet::new();

    if let Some(inh) = &spec.inherit {
        let parent_fields = lookup.resolve_parent(&inh.name, inh.revision)?;
        for pf in parent_fields {
            if !names.insert(pf.name.clone()) {
                chain.pop();
                return Err(Issue::spec(
                    "shadow_field",
                    format!("field `{}` repeated in inheritance chain (shadowing rejected)", pf.name),
                ));
            }
            merged.push(pf);
        }
    }
    for f in &spec.fields {
        if !names.insert(f.name.clone()) {
            chain.pop();
            return Err(Issue::spec(
                "shadow_field",
                format!("field `{}` duplicates an inherited field (shadowing rejected)", f.name),
            ));
        }
        merged.push(f.clone());
    }
    chain.pop();
    Ok(FormatSpec {
        name: spec.name.clone(),
        version: spec.version.clone(),
        inherit: spec.inherit.clone(),
        fields: merged,
    })
}
