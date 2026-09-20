//! Domain model: versioned binary format definitions and migration rules.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Raw byte buffer serialized as a hex string (accepts hex strings or int arrays on input).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HexBytes(pub Vec<u8>);

impl HexBytes {
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for HexBytes {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&hex::encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for HexBytes {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = HexBytes;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("hex string or array of bytes")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<HexBytes, E> {
                let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
                let v = hex::decode(&cleaned).map_err(serde::de::Error::custom)?;
                Ok(HexBytes(v))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<HexBytes, A::Error> {
                let mut out = Vec::new();
                while let Some(b) = seq.next_element::<u64>()? {
                    if b > 255 {
                        return Err(serde::de::Error::custom("byte value out of range"));
                    }
                    out.push(b as u8);
                }
                Ok(HexBytes(out))
            }
        }
        de.deserialize_any(V)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    Little,
    Big,
}

impl Default for Endian {
    fn default() -> Self {
        Endian::Big
    }
}

/// Reference to a parent format for inheritance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritRef {
    pub name: String,
    /// Pinned revision of the parent definition (fingerprint stability).
    pub revision: i64,
}

/// Byte range covered or excluded by a checksum, expressed as expressions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChecksumRange {
    /// Start offset expression (default 0).
    #[serde(default)]
    pub start: String,
    /// End offset expression (exclusive). Empty means "end of file".
    #[serde(default)]
    pub end: String,
    /// Additional exclusion ranges [start,end); useful when nesting checksums.
    #[serde(default)]
    pub exclude: Vec<[String; 2]>,
}

/// A field / layout construct. One flattened structure for every kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    /// magic | int | fixed | bytes | pad | bitfield | struct | branch | array | ext_container | checksum
    pub kind: String,

    // ---- int ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endian: Option<Endian>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed: Option<bool>,

    // ---- magic / fixed ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<HexBytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ascii: Option<String>,

    // ---- bytes / pad ----
    /// Integer literal or expression referencing earlier scalar fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<String>,
    /// Pad kind: "zero" (always zeros on write) or "raw" (reproduce input bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,

    // ---- bitfield ----
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<BitMember>,

    // ---- struct / branch ----
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldDef>,

    // ---- branch ----
    /// Field path (relative to the branch parent) whose integer value selects a case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Map of selector value (as string) -> nested fields.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cases: BTreeMap<String, Vec<FieldDef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_case: Option<String>,

    // ---- array ----
    /// Element count expression referencing earlier scalar fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<String>,
    /// Element layout for `array` (struct fields; a single scalar field gives scalar arrays).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub element: Vec<FieldDef>,

    // ---- ext_container ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_width: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len_width: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext_endian: Option<Endian>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<ExtIdDef>,

    // ---- checksum ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ChecksumRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitMember {
    pub name: String,
    /// Lowest significant bit (0-based).
    pub lsb: usize,
    pub bits: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtIdDef {
    pub tag: i64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldDef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatSpec {
    pub name: String,
    /// Free-form version label (e.g. "v1").
    pub version: String,
    #[serde(default)]
    pub inherit: Option<InheritRef>,
    #[serde(default)]
    pub fields: Vec<FieldDef>,
}

// ---------------------------------------------------------------- migration rules

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Mapping {
    /// Copy a source leaf path to a target leaf path.
    Copy {
        from: String,
        to: String,
        /// Semantic transforms; currently: "u16_to_u32" (numeric widen),
        /// "bytes_truncate_<n>", "bytes_pad_<n>_<hexbyte>".
        #[serde(default)]
        transform: Option<String>,
    },
    /// Insert a constant default value (hex for bytes, number or string for ints/enums).
    Constant {
        to: String,
        value: serde_json::Value,
    },
    /// Drop a source leaf; the target has no replacement (always lossy).
    Drop {
        from: String,
    },
    /// Parse fields out of a known extension block (source) into flat target leaves.
    FromExtension {
        from: String,
        to: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSpec {
    pub name: String,
    pub from_format: String,
    pub from_revision: i64,
    pub to_format: String,
    pub to_revision: i64,
    #[serde(default)]
    pub mappings: Vec<Mapping>,
    /// Known source extension tags whose whole block is intentionally removed.
    #[serde(default)]
    pub drop_extensions: Vec<i64>,
    #[serde(default)]
    pub description: String,
}

/// Leaf value used while migrating: integer or raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeafValue {
    Int(i128),
    Bytes(Vec<u8>),
}

impl LeafValue {
    pub fn as_int(&self) -> Option<i128> {
        match self {
            LeafValue::Int(v) => Some(*v),
            LeafValue::Bytes(b) if b.len() <= 16 => {
                let mut v: i128 = 0;
                for x in b {
                    v = (v << 8) | (*x as i128);
                }
                Some(v)
            }
            _ => None,
        }
    }
}
