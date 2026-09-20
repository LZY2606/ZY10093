use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::model::*;
use crate::parser::Node;

#[derive(Debug, Clone)]
pub struct WriteError {
    pub code: String,
    pub path: String,
    pub message: String,
}

impl WriteError {
    fn new(code: &str, path: &str, message: impl Into<String>) -> Self {
        WriteError {
            code: code.to_string(),
            path: path.to_string(),
            message: message.into(),
        }
    }
    pub fn to_json(&self) -> Value {
        json!({"code": self.code, "path": self.path, "message": self.message})
    }
}

#[derive(Debug, Clone)]
pub struct RangeRec {
    pub path: String,
    pub kind: String,
    pub start: usize,
    pub end: usize,
    pub auto: bool,
}

pub struct WriteRequest<'a> {
    pub compiled: &'a Compiled,
    pub tree: &'a [Node],
    pub edits: BTreeMap<String, Value>,
    pub original: Option<&'a [u8]>,
}

pub struct WriteOutcome {
    pub bytes: Vec<u8>,
    pub ranges: Vec<RangeRec>,
    pub auto_fields: BTreeSet<String>,
}

struct Emitter<'a> {
    out: Vec<u8>,
    req: &'a WriteRequest<'a>,
    node_paths: BTreeMap<String, &'a Node>,
    int_types: BTreeMap<String, IntType>,
    ranges: Vec<RangeRec>,
    auto_fields: BTreeSet<String>,
    pending_checksums: Vec<(String, usize, IntType, Algo, ChkRange)>,
    errors: Vec<WriteError>,
}

pub fn write(req: WriteRequest) -> Result<WriteOutcome, Vec<WriteError>> {
    let mut node_paths = BTreeMap::new();
    fn collect<'a>(nodes: &'a [Node], map: &mut BTreeMap<String, &'a Node>) {
        for node in nodes {
            map.insert(node.path.clone(), node);
            collect(&node.children, map);
        }
    }
    collect(req.tree, &mut node_paths);
    let mut int_types = BTreeMap::new();
    collect_int_types(&req.compiled.effective, "", &mut int_types);

    let mut emitter = Emitter {
        out: Vec::new(),
        req: &req,
        node_paths,
        int_types,
        ranges: Vec::new(),
        auto_fields: BTreeSet::new(),
        pending_checksums: Vec::new(),
        errors: Vec::new(),
    };
    let mut cursor = 0usize;
    for magic in &emitter.req.compiled.doc.magic {
        let bytes = crate::util::parse_hex(&magic.hex).unwrap_or_default();
        if magic.offset > cursor {
            emitter.fill_gap(cursor, magic.offset);
            cursor = magic.offset;
        }
        if magic.offset == cursor {
            let start = cursor;
            emitter.push("magic".to_string(), "magic", start, &bytes, false);
            cursor += bytes.len();
        }
    }
    emitter.emit_items(&emitter.req.compiled.effective, "", &mut cursor);

    let original_len = req.original.map(|b| b.len()).unwrap_or(0);
    let trailer = req
        .tree
        .iter()
        .find(|n| n.path == "unidentified.trailer")
        .filter(|n| n.end == original_len);
    if let Some(node) = trailer {
        if let Some(bytes) = req.original {
            emitter.out.extend_from_slice(&bytes[node.start..node.end]);
            emitter.ranges.push(RangeRec {
                path: node.path.clone(),
                kind: "unidentified".to_string(),
                start: node.start,
                end: node.end,
                auto: false,
            });
        }
    }

    emitter.patch_lengths();
    emitter.patch_checksums();
    if emitter.errors.is_empty() {
        Ok(WriteOutcome {
            bytes: emitter.out,
            ranges: emitter.ranges,
            auto_fields: emitter.auto_fields,
        })
    } else {
        Err(emitter.errors)
    }
}

impl<'a> Emitter<'a> {
    fn emit_items(&mut self, items: &[Item], prefix: &str, cursor: &mut usize) {
        for item in items {
            if let Some(abs) = item.offset() {
                if abs < *cursor {
                    self.errors.push(WriteError::new(
                        "overlap",
                        item.name().unwrap_or(""),
                        format!("field at {abs} overlaps emitted cursor {cursor}"),
                    ));
                    continue;
                }
                if abs > *cursor {
                    self.fill_gap(*cursor, abs);
                    *cursor = abs;
                }
            }
            self.emit_one(item, prefix, cursor);
        }
    }

    fn emit_one(&mut self, item: &Item, prefix: &str, cursor: &mut usize) {
        match item {
            Item::Magic(_) => {}
            Item::Int(f) => self.emit_int(f, prefix, cursor, false),
            Item::Bits(f) => self.emit_bits(f, prefix, cursor),
            Item::Align(f) => self.emit_align(f, prefix, cursor),
            Item::Bytes(f) => self.emit_bytes(f, prefix, cursor),
            Item::Branch(f) => self.emit_branch(f, prefix, cursor),
            Item::Checksum(f) => self.emit_checksum_placeholder(f, prefix, cursor),
            Item::Ext(f) => self.emit_ext(f, prefix, cursor),
        }
    }

    fn path_of(prefix: &str, name: &str) -> String {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}.{name}")
        }
    }

    fn node_value(&self, path: &str) -> Option<&Value> {
        if let Some(value) = self.req.edits.get(path) {
            return Some(value);
        }
        self.node_paths.get(path).and_then(|n| n.value.as_ref())
    }

    fn int_value(&self, path: &str) -> Option<i64> {
        self.node_value(path).and_then(|v| v.as_i64())
    }

    fn fill_gap(&mut self, start: usize, end: usize) {
        if let Some(original) = self.req.original {
            if end <= original.len() {
                self.out.extend_from_slice(&original[start..end]);
                return;
            }
        }
        self.out.extend(std::iter::repeat(0u8).take(end - start));
    }

    fn push(&mut self, path: String, kind: impl Into<String>, start: usize, bytes: &[u8], auto: bool) {
        let end = start + bytes.len();
        self.out.extend_from_slice(bytes);
        self.ranges.push(RangeRec {
            path,
            kind: kind.into(),
            start,
            end,
            auto,
        });
    }
}

impl<'a> Emitter<'a> {
    fn emit_int(&mut self, field: &IntField, prefix: &str, cursor: &mut usize, auto: bool) {
        let path = Self::path_of(prefix, &field.name);
        let start = *cursor;
        let Some(value) = self.int_value(&path) else {
            self.errors.push(WriteError::new("missing_value", &path, "no integer value"));
            return;
        };
        if !fits_int(value as i128, field.int) {
            self.errors.push(WriteError::new(
                "value_overflow",
                &path,
                format!("value {value} does not fit {:?}", field.int),
            ));
            return;
        }
        let bytes = encode_int(value as i128, field.int, self.req.compiled.doc.endian);
        self.push(path, "int", start, &bytes, auto);
        *cursor = self.ranges.last().unwrap().end;
        if auto {
            self.auto_fields.insert(field.name.clone());
        }
    }

    fn emit_bits(&mut self, field: &BitsField, prefix: &str, cursor: &mut usize) {
        let path = Self::path_of(prefix, &field.name);
        let start = *cursor;
        let width = (field.int.bytes() * 8) as u32;
        let mut raw: u64 = self
            .node_value(&path)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        for part in &field.parts {
            let part_path = format!("{path}.{}", part.name);
            if let Some(part_value) = self.node_value(&part_path).and_then(|v| v.as_u64()) {
                let mask = if part.bits == 64 { u64::MAX } else { (1u64 << part.bits) - 1 };
                raw &= !(mask << part.lsb);
                raw |= (part_value & mask) << part.lsb;
            }
        }
        let raw = raw as i64;
        if !fits_int(raw as i128, field.int) && !field.int.signed() {
            self.errors.push(WriteError::new("value_overflow", &path, "bits value overflow"));
            return;
        }
        let bytes = encode_int(raw as i128, field.int, self.req.compiled.doc.endian);
        let _ = width;
        self.push(path, "bits", start, &bytes, false);
        *cursor = self.ranges.last().unwrap().end;
    }

    fn emit_align(&mut self, field: &AlignField, prefix: &str, cursor: &mut usize) {
        let path = Self::path_of(prefix, &field.name);
        let start = *cursor;
        let rem = start % field.boundary;
        let pad = if rem == 0 { 0 } else { field.boundary - rem };
        let mut bytes = vec![0u8; pad];
        if field.pad == PadKind::Preserve {
            if let Some(original) = self.req.original {
                if start + pad <= original.len() {
                    bytes = original[start..start + pad].to_vec();
                }
            }
        }
        self.push(path, "align", start, &bytes, false);
        *cursor = self.ranges.last().unwrap().end;
    }
}

impl<'a> Emitter<'a> {
    fn node_hex(&self, path: &str) -> Option<Vec<u8>> {
        let value = self.node_value(path)?;
        let hex = value.get("hex").and_then(|v| v.as_str())?;
        crate::util::parse_hex(hex).ok()
    }

    fn emit_bytes(&mut self, field: &BytesField, prefix: &str, cursor: &mut usize) {
        let path = Self::path_of(prefix, &field.name);
        let start = *cursor;
        let Some(bytes) = self.node_hex(&path) else {
            self.errors.push(WriteError::new("missing_value", &path, "no bytes hex value"));
            return;
        };
        if let Len::Fixed { value } = &field.len {
            if bytes.len() != *value {
                self.errors.push(WriteError::new(
                    "length_mismatch",
                    &path,
                    format!("bytes length {} != fixed length {value}", bytes.len()),
                ));
                return;
            }
        }
        self.push(path, "bytes", start, &bytes, false);
        *cursor = self.ranges.last().unwrap().end;
    }

    fn emit_branch(&mut self, field: &BranchField, prefix: &str, cursor: &mut usize) {
        let path = Self::path_of(prefix, &field.name);
        let node = self.node_paths.get(path.as_str()).copied();
        let Some(arm_idx) = node.and_then(|n| n.arm) else {
            self.errors.push(WriteError::new(
                "missing_branch",
                &path,
                "no parsed branch node to select arm from",
            ));
            return;
        };
        if arm_idx >= field.arms.len() {
            self.errors.push(WriteError::new(
                "invalid_arm",
                &path,
                format!("arm index {arm_idx} out of range"),
            ));
            return;
        }
        let start = *cursor;
        let branch_start_range = self.ranges.len();
        self.emit_items(
            &field.arms[arm_idx].layout.clone(),
            &format!("{path}.arm{arm_idx}"),
            cursor,
        );
        let end = *cursor;
        self.ranges.insert(
            branch_start_range,
            RangeRec {
                path: path.clone(),
                kind: "branch".to_string(),
                start,
                end,
                auto: false,
            },
        );
    }

    fn emit_checksum_placeholder(
        &mut self,
        field: &ChecksumField,
        prefix: &str,
        cursor: &mut usize,
    ) {
        let path = Self::path_of(prefix, &field.name);
        let start = *cursor;
        let len = field.int.bytes();
        let placeholder = vec![0u8; len];
        self.push(path.clone(), "checksum", start, &placeholder, false);
        *cursor = self.ranges.last().unwrap().end;
        self.pending_checksums.push((
            path,
            start,
            field.int,
            field.algo,
            field.range.clone(),
        ));
    }
}

impl<'a> Emitter<'a> {
    fn count_value(&self, count: &Count) -> Option<usize> {
        match count {
            Count::Fixed { value } => Some(*value),
            Count::Field { path } => self.int_value(path).map(|v| v as usize),
        }
    }

    fn emit_ext(&mut self, field: &ExtField, prefix: &str, cursor: &mut usize) {
        let path = Self::path_of(prefix, &field.name);
        let start = *cursor;
        let Some(count) = self.count_value(&field.count) else {
            self.errors.push(WriteError::new("missing_count", &path, "cannot resolve ext count"));
            return;
        };
        let ext_node = self
            .node_paths
            .get(path.as_str())
            .copied()
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for index in 0..count {
            let block_path = format!("{path}[{index}]");
            let Some(block) = ext_node.iter().find(|n| n.path == block_path) else {
                self.errors.push(WriteError::new(
                    "missing_ext_block",
                    &block_path,
                    "no parsed block available for extension",
                ));
                return;
            };
            let tag = block.tag.unwrap_or(0);
            let payload_hex = block
                .value
                .as_ref()
                .and_then(|v| v.get("hex"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let header = &field.header;
            let tag_bytes = encode_int(tag as i128, header.tag, header.endian);
            let known = block.identified && field.known.contains_key(&tag.to_string());
            let block_start = *cursor;
            self.push(
                format!("{block_path}.tag"),
                "int",
                block_start,
                &tag_bytes,
                false,
            );
            *cursor = self.ranges.last().unwrap().end;
            let len_placeholder_pos = self.out.len();
            self.push(
                format!("{block_path}.len"),
                "int",
                *cursor,
                &vec![0u8; header.len.bytes()],
                false,
            );
            *cursor = self.ranges.last().unwrap().end;
            let payload_start = *cursor;
            if known {
                let layout = field.known.get(&tag.to_string()).cloned().unwrap_or_default();
                self.emit_items(&layout, &block_path, cursor);
            } else {
                let payload_bytes = crate::util::parse_hex(payload_hex).unwrap_or_default();
                self.push(
                    format!("{block_path}.payload"),
                    "bytes",
                    payload_start,
                    &payload_bytes,
                    false,
                );
                *cursor = self.ranges.last().unwrap().end;
            }
            let payload_len = *cursor - payload_start;
            let raw_len = payload_len as u64 / header.len_scale.max(1);
            let len_bytes = encode_int(raw_len as i128, header.len, header.endian);
            self.out[len_placeholder_pos..len_placeholder_pos + header.len.bytes()]
                .copy_from_slice(&len_bytes);
            self.ranges.push(RangeRec {
                path: block_path,
                kind: "ext_block".to_string(),
                start: block_start,
                end: *cursor,
                auto: false,
            });
        }
        let end = *cursor;
        self.ranges.push(RangeRec {
            path,
            kind: "ext".to_string(),
            start,
            end,
                auto: false,
            });
    }
}

impl<'a> Emitter<'a> {
    fn range_of(&self, path: &str) -> Option<(usize, usize)> {
        self.ranges
            .iter()
            .filter(|r| r.path == path)
            .map(|r| (r.start, r.end))
            .min_by_key(|(s, _)| *s)
    }

    fn patch_lengths(&mut self) {
        self.patch_byte_lengths(&self.req.compiled.effective.clone());
        self.patch_ext_counts(&self.req.compiled.effective.clone());
    }

    fn patch_byte_lengths(&mut self, items: &[Item]) {
        for item in items {
            match item {
                Item::Bytes(f) => {
                    if let Len::Field { path: source } = &f.len {
                        if let Some((s, e)) = self.range_of(&f.name) {
                            let len = (e - s) as i128;
                            if let Some((ls, le)) = self.range_of(source) {
                            let int = *self
                                .int_types
                                .get(source)
                                .unwrap_or(&IntType::U32);
                                let bytes = encode_int(len, int, self.req.compiled.doc.endian);
                                if le - ls == bytes.len() {
                                    self.out[ls..le].copy_from_slice(&bytes);
                                    if let Some(rec) = self.ranges.iter_mut().find(|r| r.path == *source) {
                                        rec.auto = true;
                                    }
                                    self.auto_fields.insert(source.clone());
                                }
                            }
                        }
                    }
                }
                Item::Branch(f) => {
                    for arm in &f.arms {
                        self.patch_byte_lengths(&arm.layout);
                    }
                }
                Item::Ext(f) => {
                    for layout in f.known.values() {
                        self.patch_byte_lengths(layout);
                    }
                }
                _ => {}
            }
        }
    }

    fn patch_ext_counts(&mut self, items: &[Item]) {
        for item in items {
            if let Item::Ext(f) = item {
                if let Count::Field { path: source } = &f.count {
                    let prefix_dot = format!("{}.", f.name);
                    let prefix_bracket = format!("{}[", f.name);
                    let blocks = self
                        .ranges
                        .iter()
                        .filter(|r| {
                            r.kind == "ext_block"
                                && (r.path.starts_with(&prefix_dot) || r.path.starts_with(&prefix_bracket))
                        })
                        .count();
                    if let Some((ls, le)) = self.range_of(source) {
                        let int = *self
                            .int_types
                            .get(source)
                            .unwrap_or(&IntType::U32);
                        let bytes = encode_int(blocks as i128, int, self.req.compiled.doc.endian);
                        if le - ls == bytes.len() {
                            self.out[ls..le].copy_from_slice(&bytes);
                            if let Some(rec) = self
                                .ranges
                                .iter_mut()
                                .find(|r| r.path == *source)
                            {
                                rec.auto = true;
                            }
                            self.auto_fields.insert(source.clone());
                        }
                    }
                }
                for layout in f.known.values() {
                    self.patch_ext_counts(layout);
                }
            } else if let Item::Branch(f) = item {
                for arm in &f.arms {
                    self.patch_ext_counts(&arm.layout);
                }
            }
        }
    }
}


fn collect_int_types(items: &[Item], prefix: &str, map: &mut BTreeMap<String, IntType>) {
    for item in items {
        match item {
            Item::Int(f) => {
                map.insert(path_str(prefix, &f.name), f.int);
            }
            Item::Bits(f) => {
                map.insert(path_str(prefix, &f.name), f.int);
            }
            Item::Branch(f) => {
                for (idx, arm) in f.arms.iter().enumerate() {
                    let p = path_str(prefix, &f.name);
                    collect_int_types(&arm.layout, &format!("{p}.arm{idx}"), map);
                }
            }
            _ => {}
        }
    }
}

fn path_str(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

impl<'a> Emitter<'a> {
    fn resolve_edge_pos(&self, edge: Option<&Edge>, default_pos: Option<usize>) -> Option<usize> {
        let Some(edge) = edge else {
            return default_pos;
        };
        let base = match &edge.at {
            RangePoint::Start => 0usize,
            RangePoint::End => self.out.len(),
            RangePoint::FieldEnd { path } => self.range_of(path).map(|(_, e)| e)?,
        };
        Some(base.saturating_add_signed(edge.offset))
    }

    fn patch_checksums(&mut self) {
        let pending = std::mem::take(&mut self.pending_checksums);
        for (path, start, int, algo, range) in pending {
            let len = int.bytes();
            let end = start + len;
            let Some(mut range_start) = self.resolve_edge_pos(range.start.as_ref(), Some(0)) else {
                self.errors.push(WriteError::new(
                    "checksum_range_unresolved",
                    &path,
                    "checksum range start unresolved",
                ));
                continue;
            };
            let Some(mut range_end) = self.resolve_edge_pos(range.end.as_ref(), Some(start)) else {
                self.errors.push(WriteError::new(
                    "checksum_range_unresolved",
                    &path,
                    "checksum range end unresolved",
                ));
                continue;
            };
            range_start = range_start.min(self.out.len());
            range_end = range_end.min(self.out.len());
            if range_start <= start && end <= range_end {
                self.errors.push(WriteError::new(
                    "checksum_self_reference",
                    &path,
                    "checksum range covers the checksum field itself",
                ));
                continue;
            }
            if range_start > range_end {
                self.errors.push(WriteError::new(
                    "checksum_range_invalid",
                    &path,
                    "checksum range start after end",
                ));
                continue;
            }
            let value = match algo {
                Algo::Xor8 => crate::util::xor8(&self.out[range_start..range_end]),
                Algo::Sum16 => crate::util::sum16(&self.out[range_start..range_end]),
                Algo::Crc32 => crate::util::crc32_ieee(&self.out[range_start..range_end]),
            };
            let bytes = encode_int(value as i128, int, self.req.compiled.doc.endian);
            self.out[start..end].copy_from_slice(&bytes);
        }
    }
}

pub fn ranges_to_json(ranges: &[RangeRec]) -> Value {
    json!(ranges
        .iter()
        .map(|r| json!({
            "path": r.path,
            "kind": r.kind,
            "start": r.start,
            "end": r.end,
            "auto": r.auto,
        }))
        .collect::<Vec<_>>())
}
