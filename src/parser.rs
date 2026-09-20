use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::model::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub path: String,
    pub kind: String,
    pub start: usize,
    pub end: usize,
    pub value: Option<Value>,
    pub children: Vec<Node>,
    pub arm: Option<usize>,
    pub tag: Option<u64>,
    pub identified: bool,
}

impl Node {
    fn leaf(
        path: impl Into<String>,
        kind: impl Into<String>,
        start: usize,
        end: usize,
        value: Option<Value>,
    ) -> Self {
        Node {
            path: path.into(),
            kind: kind.into(),
            start,
            end,
            value,
            children: Vec::new(),
            arm: None,
            tag: None,
            identified: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub code: String,
    pub offset: usize,
    pub path: String,
    pub message: String,
}

impl ParseError {
    fn new(code: &str, offset: usize, path: &str, message: impl Into<String>) -> Self {
        ParseError {
            code: code.to_string(),
            offset,
            path: path.to_string(),
            message: message.into(),
        }
    }
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "offset": self.offset,
            "path": self.path,
            "message": self.message,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Warning {
    pub code: String,
    pub offset: usize,
    pub path: String,
    pub message: String,
    pub expected: Option<u64>,
    pub actual: Option<u64>,
}

impl Warning {
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "offset": self.offset,
            "path": self.path,
            "message": self.message,
            "expected": self.expected,
            "actual": self.actual,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ParseOutcome {
    pub tree: Vec<Node>,
    pub warnings: Vec<Warning>,
    pub errors: Vec<ParseError>,
}

#[derive(Clone)]
struct Scope {
    end: Option<usize>,
}

pub struct Parser<'a> {
    data: &'a [u8],
    endian: Endian,
    layout: Vec<Item>,
    values: BTreeMap<String, Value>,
    errors: Vec<ParseError>,
    warnings: Vec<Warning>,
}

impl<'a> Parser<'a> {
    pub fn parse(compiled: &Compiled, data: &[u8]) -> ParseOutcome {
        let mut parser = Parser {
            data,
            endian: compiled.doc.endian,
            layout: compiled.effective.clone(),
            values: BTreeMap::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
        };
        let mut tree = Vec::new();
        parser.check_magics(&compiled.doc.magic);
        let mut cursor = 0usize;
        parser.parse_items(
            &compiled.effective,
            "",
            &Scope { end: Some(data.len()) },
            &mut cursor,
            &mut tree,
        );
        if cursor < data.len() {
            tree.push(Node {
                path: "unidentified.trailer".to_string(),
                kind: "unidentified".to_string(),
                start: cursor,
                end: data.len(),
                value: Some(json!({ "hex": crate::util::to_hex(&data[cursor..]) })),
                children: Vec::new(),
                arm: None,
                tag: None,
                identified: false,
            });
        }
        parser.verify_checksums(&tree);
        ParseOutcome {
            tree,
            warnings: parser.warnings,
            errors: parser.errors,
        }
    }

    fn fail(&mut self, code: &str, offset: usize, path: &str, message: impl Into<String>) {
        self.errors.push(ParseError::new(code, offset, path, message));
    }

    fn child_path(prefix: &str, name: &str) -> String {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}.{name}")
        }
    }

    fn gap_node(prev_end: usize, start: usize, label: &str) -> Node {
        Node {
            path: format!("unidentified.gap.{label}"),
            kind: "unidentified".to_string(),
            start: prev_end,
            end: start,
            value: Some(json!({ "hex": crate::util::to_hex(&[]) })),
            children: Vec::new(),
            arm: None,
            tag: None,
            identified: false,
        }
    }
}

impl<'a> Parser<'a> {
    fn parse_items(
        &mut self,
        items: &[Item],
        prefix: &str,
        scope: &Scope,
        cursor: &mut usize,
        out: &mut Vec<Node>,
    ) {
        let mut last_leaf_end: Option<usize> = None;
        for item in items {
            if let Some(abs) = item.offset() {
                if abs < *cursor {
                    self.fail(
                        "overlap",
                        abs,
                        item.name().unwrap_or(""),
                        format!("field at {abs} overlaps current cursor at {}", cursor),
                    );
                    continue;
                }
                if abs > *cursor {
                    let gap = self.data.get(*cursor..abs).map(|b| b.to_vec()).unwrap_or_default();
                    if !gap.is_empty() {
                        let mut node = Self::gap_node(*cursor, abs, &format!("at{abs}"));
                        node.value = Some(json!({ "hex": crate::util::to_hex(&gap) }));
                        out.push(node);
                    }
                    *cursor = abs;
                }
            }
            self.parse_one(item, prefix, scope, cursor, out, &mut last_leaf_end);
        }
    }

    fn parse_one(
        &mut self,
        item: &Item,
        prefix: &str,
        scope: &Scope,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last_leaf_end: &mut Option<usize>,
    ) {
        match item {
            Item::Magic(_) => {}
            Item::Int(f) => self.parse_int(f, prefix, cursor, out, last_leaf_end),
            Item::Bits(f) => self.parse_bits(f, prefix, cursor, out, last_leaf_end),
            Item::Align(f) => self.parse_align(f, prefix, scope, cursor, out, last_leaf_end),
            Item::Bytes(f) => self.parse_bytes(f, prefix, scope, cursor, out, last_leaf_end),
            Item::Branch(f) => self.parse_branch(f, prefix, scope, cursor, out, last_leaf_end),
            Item::Checksum(f) => self.parse_checksum(f, prefix, cursor, out, last_leaf_end),
            Item::Ext(f) => self.parse_ext(f, prefix, scope, cursor, out, last_leaf_end),
        }
    }

    fn read_leaf(&mut self, start: usize, len: usize, scope: &Scope, path: &str) -> Option<Vec<u8>> {
        if let Some(end) = scope.end {
            if start + len > end {
                self.fail(
                    "length_out_of_bounds",
                    start,
                    path,
                    format!("leaf ends at {} past scope end {end}", start + len),
                );
                return None;
            }
        }
        if start + len > self.data.len() {
            self.fail(
                "short_read",
                start,
                path,
                format!("short read: need {len} bytes at offset {start}"),
            );
            return None;
        }
        Some(self.data[start..start + len].to_vec())
    }

    fn mark_leaf(&self, start: usize, end: usize, last: &mut Option<usize>) -> bool {
        if let Some(prev) = *last {
            if start < prev {
                return false;
            }
        }
        *last = Some(end);
        true
    }
}

impl<'a> Parser<'a> {
    fn parse_int(
        &mut self,
        field: &IntField,
        prefix: &str,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last: &mut Option<usize>,
    ) {
        let path = Self::child_path(prefix, &field.name);
        let start = *cursor;
        let len = field.int.bytes();
        let Some(bytes) = self.read_leaf(start, len, &Scope { end: None }, &path) else {
            return;
        };
        let end = start + len;
        if !self.mark_leaf(start, end, last) {
            self.fail("overlap", start, &path, "overlapping field");
            return;
        }
        let value = decode_int(&bytes, field.int, self.endian);
        self.values.insert(path.clone(), json!(value));
        *cursor = end;
        out.push(Node::leaf(path, "int", start, end, Some(json!(value))));
    }

    fn parse_bits(
        &mut self,
        field: &BitsField,
        prefix: &str,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last: &mut Option<usize>,
    ) {
        let path = Self::child_path(prefix, &field.name);
        let start = *cursor;
        let len = field.int.bytes();
        let Some(bytes) = self.read_leaf(start, len, &Scope { end: None }, &path) else {
            return;
        };
        let end = start + len;
        if !self.mark_leaf(start, end, last) {
            self.fail("overlap", start, &path, "overlapping bit field");
            return;
        }
        let raw = decode_int(&bytes, field.int, self.endian) as u64;
        let width = (len * 8) as u32;
        let mut children = Vec::new();
        for part in &field.parts {
            if part.bits == 0 || part.lsb + part.bits > width {
                self.fail(
                    "bitfield_out_of_bounds",
                    start,
                    &format!("{path}.{}", part.name),
                    format!("bit part lsb={} bits={} exceeds {width}", part.lsb, part.bits),
                );
                return;
            }
            let mask = if part.bits == 64 { u64::MAX } else { (1u64 << part.bits) - 1 };
            let part_value = (raw >> part.lsb) & mask;
            let part_path = format!("{path}.{}", part.name);
            self.values.insert(part_path.clone(), json!(part_value));
            children.push(Node::leaf(part_path, "bit", start, end, Some(json!(part_value))));
        }
        self.values.insert(path.clone(), json!(raw));
        *cursor = end;
        out.push(Node {
            path,
            kind: "bits".to_string(),
            start,
            end,
            value: Some(json!(raw)),
            children,
            arm: None,
            tag: None,
            identified: true,
        });
    }
}

impl<'a> Parser<'a> {
    fn parse_align(
        &mut self,
        field: &AlignField,
        prefix: &str,
        scope: &Scope,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last: &mut Option<usize>,
    ) {
        let path = Self::child_path(prefix, &field.name);
        let start = *cursor;
        let rem = start % field.boundary;
        let pad = if rem == 0 { 0 } else { field.boundary - rem };
        if pad > 0 {
            if self.read_leaf(start, pad, scope, &path).is_none() {
                return;
            }
            if field.pad == PadKind::Zero {
                for (i, b) in self.data[start..start + pad].iter().enumerate() {
                    if *b != 0 {
                        self.fail(
                            "alignment_nonzero",
                            start + i,
                            &path,
                            "zero-aligned region contains a non-zero byte",
                        );
                        return;
                    }
                }
            }
        }
        let end = start + pad;
        self.mark_leaf(start, end, last);
        *cursor = end;
        out.push(Node::leaf(path, "align", start, end, Some(json!({ "pad": pad }))));
    }

    fn resolve_len(&self, len: &Len) -> Option<usize> {
        match len {
            Len::Fixed { value } => Some(*value),
            Len::Field { path } => {
                let n = self.values.get(path).and_then(|v| v.as_i64())?;
                if n < 0 { None } else { Some(n as usize) }
            }
        }
    }

    fn parse_bytes(
        &mut self,
        field: &BytesField,
        prefix: &str,
        scope: &Scope,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last: &mut Option<usize>,
    ) {
        let path = Self::child_path(prefix, &field.name);
        let start = *cursor;
        let Some(len) = self.resolve_len(&field.len) else {
            self.fail("invalid_length", start, &path, "could not resolve bytes length");
            return;
        };
        if self.read_leaf(start, len, scope, &path).is_none() {
            return;
        }
        let end = start + len;
        self.mark_leaf(start, end, last);
        let hex = crate::util::to_hex(&self.data[start..end]);
        self.values.insert(path.clone(), json!({ "len": len }));
        *cursor = end;
        out.push(Node::leaf(path, "bytes", start, end, Some(json!({ "hex": hex, "len": len }))));
    }
}

impl<'a> Parser<'a> {
    fn eval_cond(&self, cond: &Cond) -> bool {
        let Some(current) = self.values.get(&cond.path).and_then(|v| v.as_i64()) else {
            return false;
        };
        let target = cond.value;
        match cond.cmp {
            Cmp::Eq => current == target,
            Cmp::Ne => current != target,
            Cmp::Gt => current > target,
            Cmp::Lt => current < target,
            Cmp::Ge => current >= target,
            Cmp::Le => current <= target,
            Cmp::HasBit => target >= 0 && (current as u64 & (1u64 << target as u32) != 0),
        }
    }

    fn select_arm(&self, arms: &[Arm]) -> Option<usize> {
        let mut fallback = None;
        for (idx, arm) in arms.iter().enumerate() {
            match &arm.when {
                None => fallback = Some(idx),
                Some(cond) if self.eval_cond(cond) => return Some(idx),
                Some(_) => {}
            }
        }
        fallback
    }

    fn parse_branch(
        &mut self,
        field: &BranchField,
        prefix: &str,
        scope: &Scope,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last: &mut Option<usize>,
    ) {
        let path = Self::child_path(prefix, &field.name);
        let Some(arm_idx) = self.select_arm(&field.arms) else {
            self.fail(
                "unmatched_branch",
                *cursor,
                &path,
                "no branch arm matched and no default arm exists",
            );
            return;
        };
        let start = *cursor;
        let mut children = Vec::new();
        self.parse_items(
            &field.arms[arm_idx].layout,
            &format!("{path}.arm{arm_idx}"),
            scope,
            cursor,
            &mut children,
        );
        let end = *cursor;
        self.mark_leaf(start, end, last);
        out.push(Node {
            path,
            kind: "branch".to_string(),
            start,
            end,
            value: Some(json!({ "arm": arm_idx })),
            children,
            arm: Some(arm_idx),
            tag: None,
            identified: true,
        });
    }

    fn parse_checksum(
        &mut self,
        field: &ChecksumField,
        prefix: &str,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last: &mut Option<usize>,
    ) {
        let path = Self::child_path(prefix, &field.name);
        let start = *cursor;
        let len = field.int.bytes();
        let Some(bytes) = self.read_leaf(start, len, &Scope { end: None }, &path) else {
            return;
        };
        let end = start + len;
        if !self.mark_leaf(start, end, last) {
            self.fail("overlap", start, &path, "overlapping checksum field");
            return;
        }
        let stored = decode_int(&bytes, field.int, self.endian) as u64;
        self.values.insert(path.clone(), json!(stored as i64));
        *cursor = end;
        out.push(Node::leaf(path, "checksum", start, end, Some(json!(stored))));
    }
}

impl<'a> Parser<'a> {
    fn resolve_count(&self, count: &Count) -> Option<usize> {
        match count {
            Count::Fixed { value } => Some(*value),
            Count::Field { path } => {
                let n = self.values.get(path).and_then(|v| v.as_i64())?;
                if n < 0 { None } else { Some(n as usize) }
            }
        }
    }

    fn parse_ext(
        &mut self,
        field: &ExtField,
        prefix: &str,
        scope: &Scope,
        cursor: &mut usize,
        out: &mut Vec<Node>,
        last: &mut Option<usize>,
    ) {
        let path = Self::child_path(prefix, &field.name);
        let ext_start = *cursor;
        let Some(count) = self.resolve_count(&field.count) else {
            self.fail("invalid_count", ext_start, &path, "could not resolve extension count");
            return;
        };
        let mut blocks = Vec::new();
        for index in 0..count {
            if self.parse_ext_block(field, &path, index, scope, cursor, &mut blocks).is_err() {
                return;
            }
        }
        let ext_end = *cursor;
        self.mark_leaf(ext_start, ext_end, last);
        out.push(Node {
            path,
            kind: "ext".to_string(),
            start: ext_start,
            end: ext_end,
            value: Some(json!({ "count": count })),
            children: blocks,
            arm: None,
            tag: None,
            identified: true,
        });
    }

    fn parse_ext_block(
        &mut self,
        field: &ExtField,
        path: &str,
        index: usize,
        scope: &Scope,
        cursor: &mut usize,
        blocks: &mut Vec<Node>,
    ) -> Result<(), ()> {
        let header = &field.header;
        let block_path = format!("{path}[{index}]");
        let tag_start = *cursor;
        let tag_bytes = self
            .read_leaf(tag_start, header.tag.bytes(), scope, &block_path)
            .ok_or(())?;
        let tag = decode_int(&tag_bytes, header.tag, header.endian) as u64;
        *cursor = tag_start + header.tag.bytes();
        let len_start = *cursor;
        let len_bytes = self
            .read_leaf(len_start, header.len.bytes(), scope, &block_path)
            .ok_or(())?;
        let len_raw = decode_int(&len_bytes, header.len, header.endian) as u64;
        let payload_len = len_raw.checked_mul(header.len_scale).unwrap_or(u64::MAX) as usize;
        *cursor = len_start + header.len.bytes();
        let payload_start = *cursor;
        self.read_leaf(payload_start, payload_len, scope, &block_path).ok_or(())?;
        let payload_end = payload_start + payload_len;
        let tag_key = tag.checked_mul(header.tag_scale).unwrap_or(tag).to_string();
        if let Some(layout) = field.known.get(&tag_key) {
            let mut children = Vec::new();
            let mut payload_cursor = payload_start;
            self.parse_items(
                layout,
                &block_path,
                &Scope { end: Some(payload_end) },
                &mut payload_cursor,
                &mut children,
            );
            *cursor = payload_end;
            blocks.push(Node {
                path: block_path,
                kind: "ext_block".to_string(),
                start: tag_start,
                end: payload_end,
                value: Some(json!({ "tag": tag, "len": payload_len, "known": true })),
                children,
                arm: None,
                tag: Some(tag),
                identified: true,
            });
        } else {
            let hex = crate::util::to_hex(&self.data[payload_start..payload_end]);
            *cursor = payload_end;
            blocks.push(Node {
                path: block_path.clone(),
                kind: "ext_block".to_string(),
                start: tag_start,
                end: payload_end,
                value: Some(json!({ "tag": tag, "len": payload_len, "known": false, "hex": hex })),
                children: vec![Node::leaf(
                    format!("{block_path}.payload"),
                    "bytes",
                    payload_start,
                    payload_end,
                    Some(json!({ "hex": hex })),
                )],
                arm: None,
                tag: Some(tag),
                identified: false,
            });
        }
        Ok(())
    }
}

impl<'a> Parser<'a> {
    fn check_magics(&mut self, magics: &[Magic]) {
        for magic in magics {
            let Ok(expected) = crate::util::parse_hex(&magic.hex) else {
                self.fail("bad_magic_hex", magic.offset, "magic", "invalid magic hex");
                continue;
            };
            let start = magic.offset;
            if start + expected.len() > self.data.len() {
                self.fail(
                    "short_read",
                    start,
                    "magic",
                    format!("magic needs {} bytes at {start}", expected.len()),
                );
                continue;
            }
            if self.data[start..start + expected.len()] != expected[..] {
                self.warnings.push(Warning {
                    code: "bad_magic".to_string(),
                    offset: start,
                    path: "magic".to_string(),
                    message: "magic bytes do not match".to_string(),
                    expected: None,
                    actual: None,
                });
            }
        }
    }

    fn collect_checksum_items(items: &[Item], map: &mut BTreeMap<String, ChecksumField>) {
        for item in items {
            match item {
                Item::Checksum(f) => {
                    map.insert(f.name.clone(), f.clone());
                }
                Item::Branch(f) => {
                    for arm in &f.arms {
                        Self::collect_checksum_items(&arm.layout, map);
                    }
                }
                Item::Ext(f) => {
                    for layout in f.known.values() {
                        Self::collect_checksum_items(layout, map);
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_nodes<'n>(nodes: &'n [Node], map: &mut BTreeMap<String, &'n Node>) {
        for node in nodes {
            map.insert(node.path.clone(), node);
            Self::collect_nodes(&node.children, map);
        }
    }

    fn resolve_edge(
        &mut self,
        edge: Option<&Edge>,
        default_at_checksum: Option<usize>,
        node_map: &BTreeMap<String, &Node>,
    ) -> Option<usize> {
        let Some(edge) = edge else {
            return default_at_checksum;
        };
        let data_len = self.data.len();
        let base = match &edge.at {
            RangePoint::Start => 0usize,
            RangePoint::End => data_len,
            RangePoint::FieldEnd { path } => node_map.get(path)?.end,
        };
        Some(base.saturating_add_signed(edge.offset))
    }

    fn compute_checksum(&self, algo: Algo, range: &[u8]) -> u64 {
        match algo {
            Algo::Xor8 => crate::util::xor8(range),
            Algo::Sum16 => crate::util::sum16(range),
            Algo::Crc32 => crate::util::crc32_ieee(range),
        }
    }

    fn verify_checksums(&mut self, tree: &[Node]) {
        let mut fields = BTreeMap::new();
        Self::collect_checksum_items(&self.layout.clone(), &mut fields);
        let mut node_map: BTreeMap<String, &Node> = BTreeMap::new();
        Self::collect_nodes(tree, &mut node_map);
        for field in fields.values() {
            let Some(node) = node_map.get(&field.name).copied() else {
                continue;
            };
            let Some(start) = self.resolve_edge(field.range.start.as_ref(), Some(0), &node_map) else {
                self.fail(
                    "checksum_range_unresolved",
                    node.start,
                    &field.name,
                    "checksum range start references an unknown field",
                );
                continue;
            };
            let Some(end) = self
                .resolve_edge(field.range.end.as_ref(), Some(node.start), &node_map)
            else {
                self.fail(
                    "checksum_range_unresolved",
                    node.start,
                    &field.name,
                    "checksum range end references an unknown field",
                );
                continue;
            };
            if start > end {
                self.fail(
                    "checksum_range_invalid",
                    start,
                    &field.name,
                    "checksum range start is after its end",
                );
                continue;
            }
            let start = start.min(self.data.len());
            let end = end.min(self.data.len());
            if start <= node.start && node.end <= end {
                self.fail(
                    "checksum_self_reference",
                    node.start,
                    &field.name,
                    "checksum range covers the checksum field itself",
                );
                continue;
            }
            let actual = self.compute_checksum(field.algo, &self.data[start..end]);
            let stored = node.value.as_ref().and_then(|v| v.as_u64()).unwrap_or(0);
            if actual != stored {
                self.warnings.push(Warning {
                    code: "checksum_mismatch".to_string(),
                    offset: node.start,
                    path: field.name.clone(),
                    message: format!("expected {actual}, stored {stored}, range [{start},{end})"),
                    expected: Some(actual),
                    actual: Some(stored),
                });
            }
        }
    }
}

pub fn node_to_json(node: &Node) -> Value {
    json!({
        "path": node.path,
        "kind": node.kind,
        "start": node.start,
        "end": node.end,
        "value": node.value,
        "arm": node.arm,
        "tag": node.tag,
        "identified": node.identified,
        "children": node.children.iter().map(node_to_json).collect::<Vec<_>>(),
    })
}

pub fn outcome_to_json(outcome: &ParseOutcome, ok: bool) -> Value {
    json!({
        "ok": ok,
        "tree": outcome.tree.iter().map(node_to_json).collect::<Vec<_>>(),
        "warnings": outcome.warnings.iter().map(Warning::to_json).collect::<Vec<_>>(),
        "errors": outcome.errors.iter().map(ParseError::to_json).collect::<Vec<_>>(),
    })
}
