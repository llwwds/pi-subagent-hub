use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::skill::{SkillGrant, SkillPolicyMode};

/// ISO-8601 with milliseconds and `Z`, matching Node `Date.toISOString()`.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// UUID v4, matching Node `crypto.randomUUID()`.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes.as_ref());
    hex_encode(&hasher.finalize())
}

pub fn sha256_prefixed(bytes: impl AsRef<[u8]>) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Canonical JSON: recursively sorted object keys, array order preserved,
/// scalar encoding through serde_json. This is intentionally independent of
/// `serde_json::Map` ordering (which may be `preserve_order`).
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("json string key"));
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).expect("json scalar")),
    }
}

/// JavaScript `String.length` (UTF-16 code units).
pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Pure token rule shared by skill ids, manager agent keys and mounted skill
/// names: 1..=`max_len` ASCII bytes, first alphanumeric, then
/// alphanumeric/dot/underscore/hyphen. Reserved-name and error-code policy
/// stay at each call site.
pub fn is_safe_token(value: &str, max_len: usize) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > max_len {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Serialize)]
struct AuthorizationDigestInput<'a> {
    mode: SkillPolicyMode,
    grants: Vec<AuthorizationDigestGrant<'a>>,
}

#[derive(Serialize)]
struct AuthorizationDigestGrant<'a> {
    id: &'a str,
    sha256: &'a str,
}

/// Skill authorization digest is *not* canonical JSON. It is exactly
/// `sha256(JSON.stringify({mode,grants:[{id,sha256}]}))` with grants sorted by
/// id, matching `src/skills/registry.js:authorizationDigest`.
pub fn skill_authorization_digest(mode: SkillPolicyMode, grants: &[SkillGrant]) -> String {
    let mut sorted: Vec<&SkillGrant> = grants.iter().collect();
    sorted.sort_by(|left, right| left.id.cmp(&right.id));
    let input = AuthorizationDigestInput {
        mode,
        grants: sorted
            .into_iter()
            .map(|grant| AuthorizationDigestGrant {
                id: &grant.id,
                sha256: &grant.sha256,
            })
            .collect(),
    };
    let text = serde_json::to_string(&input).expect("serializable authorization digest input");
    sha256_prefixed(text)
}
