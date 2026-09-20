use serde_json::Value;

pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() % 2 != 0 {
        return Err(format!("hex has odd length: {s}"));
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Canonical JSON: object keys sorted, no extra whitespace, BTreeMap-backed Value sorts keys.
pub fn canonical_json(value: &Value) -> String {
    value.to_string()
}

pub fn fnv_fingerprint(value: &Value) -> String {
    format!("{:016x}", fnv1a64(canonical_json(value).as_bytes()))
}

pub fn xor8(data: &[u8]) -> u64 {
    data.iter().fold(0u8, |a, b| a ^ b) as u64
}

pub fn sum16(data: &[u8]) -> u64 {
    data.iter().map(|b| *b as u64).sum::<u64>() & 0xffff
}

pub fn crc32_ieee(data: &[u8]) -> u64 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB88320 & mask);
        }
    }
    (!crc) as u64
}
