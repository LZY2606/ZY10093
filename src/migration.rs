use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::model::*;
use crate::parser::{node_to_json, Node, Parser};
use crate::writer::{ranges_to_json, write, RangeRec, WriteRequest};
use crate::util::to_hex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleRef {
    pub format: Ref,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "source")]
pub enum Source {
    Field { path: String },
    Constant { value: i64 },
    Default {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub target: String,
    #[serde(flatten)]
    pub source: Source,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDoc {
    pub id: String,
    pub version: u32,
    pub from: Ref,
    pub to: Ref,
    #[serde(default)]
    pub bindings: Vec<Binding>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Equivalence {
    Strict,
    Semantic,
    Lossy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedLoss {
    pub path: String,
    pub rule_id: String,
    pub rule_version: u32,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct DryResult {
    pub forward_bytes: Vec<u8>,
    pub target_tree: Vec<Node>,
    pub ranges: Vec<RangeRec>,
    pub provenance: Vec<Value>,
    pub defaults: Vec<String>,
    pub losses: Vec<String>,
    pub equivalence: Equivalence,
    pub strict_bytes_equal: bool,
    pub semantic_differences: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MigrationError {
    pub code: String,
    pub message: String,
}

pub struct MigrationCtx<'a> {
    pub rule: &'a RuleDoc,
    pub from: &'a Compiled,
    pub to: &'a Compiled,
}

fn flatten_values(nodes: &[Node], map: &mut BTreeMap<String, Value>) {
    for node in nodes {
        if let Some(value) = &node.value {
            map.insert(node.path.clone(), value.clone());
        }
        flatten_values(&node.children, map);
    }
}

fn int_scalar(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    value.as_u64().map(|v| v as i64)
}

fn build_default_tree(items: &[Item], prefix: &str) -> Vec<Node> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Item::Magic(_) => {}
            Item::Int(f) => {
                let path = join(prefix, &f.name);
                out.push(Node {
                    path,
                    kind: "int".into(),
                    start: 0,
                    end: 0,
                    value: Some(json!(0)),
                    children: vec![],
                    arm: None,
                    tag: None,
                    identified: true,
                });
            }
            Item::Bits(f) => {
                let path = join(prefix, &f.name);
                out.push(Node {
                    path,
                    kind: "bits".into(),
                    start: 0,
                    end: 0,
                    value: Some(json!(0)),
                    children: f
                        .parts
                        .iter()
                        .map(|p| Node {
                            path: join(prefix, &format!("{}.{}", f.name, p.name)),
                            kind: "bit".into(),
                            start: 0,
                            end: 0,
                            value: Some(json!(0)),
                            children: vec![],
                            arm: None,
                            tag: None,
                            identified: true,
                        })
                        .collect(),
                    arm: None,
                    tag: None,
                    identified: true,
                });
            }
            Item::Align(f) => {
                out.push(Node {
                    path: join(prefix, &f.name),
                    kind: "align".into(),
                    start: 0,
                    end: 0,
                    value: Some(json!({ "pad": 0 })),
                    children: vec![],
                    arm: None,
                    tag: None,
                    identified: true,
                });
            }
            Item::Bytes(f) => {
                out.push(Node {
                    path: join(prefix, &f.name),
                    kind: "bytes".into(),
                    start: 0,
                    end: 0,
                    value: Some(json!({ "hex": "", "len": 0 })),
                    children: vec![],
                    arm: None,
                    tag: None,
                    identified: true,
                });
            }
            Item::Branch(f) => {
                let path = join(prefix, &f.name);
                let arm_idx = 0;
                let mut node = Node {
                    path,
                    kind: "branch".into(),
                    start: 0,
                    end: 0,
                    value: Some(json!({ "arm": arm_idx })),
                    children: build_default_tree(
                        &f.arms.first().map(|a| a.layout.clone()).unwrap_or_default(),
                        &format!("{}.arm{}", join(prefix, &f.name), arm_idx),
                    ),
                    arm: Some(arm_idx),
                    tag: None,
                    identified: true,
                };
                let _ = &mut node;
                out.push(node);
            }
            Item::Checksum(f) => {
                out.push(Node {
                    path: join(prefix, &f.name),
                    kind: "checksum".into(),
                    start: 0,
                    end: 0,
                    value: Some(json!(0)),
                    children: vec![],
                    arm: None,
                    tag: None,
                    identified: true,
                });
            }
            Item::Ext(f) => {
                let count = match &f.count {
                    Count::Fixed { value } => *value,
                    Count::Field { .. } => 0,
                };
                out.push(Node {
                    path: join(prefix, &f.name),
                    kind: "ext".into(),
                    start: 0,
                    end: 0,
                    value: Some(json!({ "count": count })),
                    children: vec![],
                    arm: None,
                    tag: None,
                    identified: true,
                });
            }
        }
    }
    out
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn collect_leaf_targets(items: &[Item], prefix: &str, out: &mut BTreeSet<String>) {
    for item in items {
        match item {
            Item::Magic(_) => {}
            Item::Int(f) => {
                out.insert(join(prefix, &f.name));
            }
            Item::Bits(f) => {
                for part in &f.parts {
                    out.insert(join(prefix, &format!("{}.{}", f.name, part.name)));
                }
            }
            Item::Align(_) | Item::Checksum(_) => {}
            Item::Bytes(f) => {
                out.insert(join(prefix, &f.name));
            }
            Item::Branch(f) => {
                for (idx, arm) in f.arms.iter().enumerate() {
                    collect_leaf_targets(&arm.layout, &format!("{}.arm{}", join(prefix, &f.name), idx), out);
                }
            }
            Item::Ext(_) => {}
        }
    }
}

pub fn dry_run(ctx: &MigrationCtx, source_bytes: &[u8]) -> Result<DryResult, Vec<MigrationError>> {
    let source_outcome = Parser::parse(ctx.from, source_bytes);
    if !source_outcome.errors.is_empty() {
        return Err(source_outcome
            .errors
            .iter()
            .map(|e| MigrationError {
                code: e.code.clone(),
                message: format!("source parse: {}", e.message),
            })
            .collect());
    }
    let mut source_values = BTreeMap::new();
    flatten_values(&source_outcome.tree, &mut source_values);

    let mut target_tree = build_default_tree(&ctx.to.effective, "");
    let mut edits: BTreeMap<String, Value> = BTreeMap::new();
    let mut provenance: Vec<Value> = Vec::new();
    let mut defaults: Vec<String> = Vec::new();
    let mut bound: BTreeSet<String> = BTreeSet::new();

    for binding in &ctx.rule.bindings {
        let (value, source_kind, source_desc) = match &binding.source {
            Source::Field { path } => {
                let Some(value) = source_values.get(path) else {
                    return Err(vec![MigrationError {
                        code: "missing_source".into(),
                        message: format!("binding target {} references missing source {path}", binding.target),
                    }]);
                };
                (value.clone(), "field", path.clone())
            }
            Source::Constant { value } => (json!(*value), "constant", value.to_string()),
            Source::Default {} => (Value::Null, "default", "default".to_string()),
        };
        bound.insert(binding.target.clone());
        if matches!(binding.source, Source::Default {}) {
            defaults.push(binding.target.clone());
            provenance.push(json!({
                "target": binding.target,
                "source": "default",
                "detail": "default",
                "note": binding.note,
            }));
            continue;
        }
        let scalar = int_scalar(&value);
        if let Some(n) = scalar {
            edits.insert(binding.target.clone(), json!(n));
        } else if let Some(hex) = value.get("hex").and_then(|v| v.as_str()) {
            edits.insert(binding.target.clone(), json!({ "hex": hex }));
        }
        provenance.push(json!({
            "target": binding.target,
            "source": source_kind,
            "detail": source_desc,
            "note": binding.note,
        }));
    }

    let mut target_leaves = BTreeSet::new();
    collect_leaf_targets(&ctx.to.effective, "", &mut target_leaves);
    for leaf in &target_leaves {
        if !bound.contains(leaf) {
            defaults.push(leaf.clone());
            provenance.push(json!({
                "target": leaf,
                "source": "default",
                "detail": "implicit-zero",
                "note": "",
            }));
        }
    }

    let write_result = write(WriteRequest {
        compiled: ctx.to,
        tree: &target_tree,
        edits: edits.clone(),
        original: None,
    })
    .map_err(|errs| {
        errs.into_iter()
            .map(|e| MigrationError {
                code: e.code,
                message: format!("target write ({}): {}", e.path, e.message),
            })
            .collect::<Vec<_>>()
    })?;

    let target_parsed = Parser::parse(ctx.to, &write_result.bytes);
    if !target_parsed.errors.is_empty() {
        return Err(target_parsed
            .errors
            .iter()
            .map(|e| MigrationError {
                code: e.code.clone(),
                message: format!("target parse: {}", e.message),
            })
            .collect());
    }
    target_tree = target_parsed.tree;

    let mut target_values = BTreeMap::new();
    flatten_values(&target_tree, &mut target_values);

    let mut source_leaves = BTreeSet::new();
    collect_leaf_targets(&ctx.from.effective, "", &mut source_leaves);

    let mut losses = Vec::new();
    let mut semantic_differences = Vec::new();
    let mut source_carried: BTreeSet<String> = BTreeSet::new();
    for binding in &ctx.rule.bindings {
        if let Source::Field { path } = &binding.source {
            source_carried.insert(path.clone());
        }
    }
    for leaf in &source_leaves {
        if !source_carried.contains(leaf) {
            losses.push(leaf.clone());
        }
    }
    for binding in &ctx.rule.bindings {
        if let Source::Field { path } = &binding.source {
            let source_val = source_values.get(path);
            let target_val = target_values.get(&binding.target);
            if let (Some(a), Some(b)) = (source_val, target_val) {
                if !semantic_equal(a, b) {
                    semantic_differences.push(format!("{} <- {path}: value changed", binding.target));
                }
            }
        }
    }

    let strict_bytes_equal = source_bytes == write_result.bytes.as_slice();
    let equivalence = if strict_bytes_equal {
        Equivalence::Strict
    } else if losses.is_empty() && semantic_differences.is_empty() {
        Equivalence::Semantic
    } else {
        Equivalence::Lossy
    };

    Ok(DryResult {
        forward_bytes: write_result.bytes,
        target_tree,
        ranges: write_result.ranges,
        provenance,
        defaults,
        losses,
        equivalence,
        strict_bytes_equal,
        semantic_differences,
    })
}

fn semantic_equal(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (
        a.get("hex").and_then(|v| v.as_str()),
        b.get("hex").and_then(|v| v.as_str()),
    ) {
        return crate::util::parse_hex(x).ok() == crate::util::parse_hex(y).ok();
    }
    a == b
}

pub fn dry_to_json(result: &DryResult) -> Value {
    json!({
        "output_hex": to_hex(&result.forward_bytes),
        "output_len": result.forward_bytes.len(),
        "tree": result.target_tree.iter().map(node_to_json).collect::<Vec<_>>(),
        "write_ranges": ranges_to_json(&result.ranges),
        "provenance": result.provenance,
        "defaults": result.defaults,
        "losses": result.losses,
        "semantic_differences": result.semantic_differences,
        "equivalence": match result.equivalence {
            Equivalence::Strict => "strict",
            Equivalence::Semantic => "semantic",
            Equivalence::Lossy => "lossy",
        },
        "strict_bytes_equal": result.strict_bytes_equal,
    })
}

pub fn accepted_losses_cover(result: &DryResult, accepted: &[AcceptedLoss], rule: &RuleDoc) -> Result<(), Value> {
    let mut missing = Vec::new();
    for loss in &result.losses {
        let covered = accepted.iter().any(|a| {
            a.path == *loss && a.rule_id == rule.id && a.rule_version == rule.version
        });
        if !covered {
            missing.push(loss.clone());
        }
    }
    let unknown: Vec<String> = accepted
        .iter()
        .filter(|a| !result.losses.iter().any(|l| l == &a.path))
        .map(|a| a.path.clone())
        .collect();
    if missing.is_empty() && unknown.is_empty() {
        Ok(())
    } else {
        Err(json!({
            "unaccepted_losses": missing,
            "stale_acceptances": unknown,
        }))
    }
}
