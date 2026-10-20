//! Minimal `application/x-www-form-urlencoded` and query-string handling.
//!
//! The locked axum build only enables the `query` feature, and the SSR panel
//! needs to read both query strings and POSTed HTML forms without adding a
//! dependency or a feature, so the two decoders live here. They intentionally
//! mirror `URLSearchParams` semantics used by the 2.1 browser code.

/// Parsed form/query fields, order-preserving and duplicate-preserving.
#[derive(Debug, Default, Clone)]
pub struct Fields {
    entries: Vec<(String, String)>,
}

impl Fields {
    pub fn parse(encoded: &str) -> Self {
        let mut entries = Vec::new();
        for pair in encoded.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (raw_key, raw_value) = match pair.split_once('=') {
                Some((key, value)) => (key, value),
                None => (pair, ""),
            };
            entries.push((percent_decode(raw_key), percent_decode(raw_value)));
        }
        Self { entries }
    }

    /// First value for `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Every value for `key`, preserving duplicates (multi-checkbox groups).
    pub fn get_all<'a>(&'a self, key: &str) -> Vec<&'a str> {
        self.entries
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub fn get_trimmed(&self, key: &str) -> Option<&str> {
        self.get(key).map(str::trim).filter(|value| !value.is_empty())
    }

    pub fn is_set(&self, key: &str) -> bool {
        self.entries.iter().any(|(name, _)| name == key)
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }
}

/// Percent-decoding with `+` as space, matching `URLSearchParams`.
pub fn percent_decode(value: &str) -> String {
    if !value.contains('%') && !value.contains('+') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encoding for query values produced by the app itself.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duplicates_and_escapes() {
        let fields = Fields::parse("a=1&b=x+y&c=%E4%B8%AD%E6%96%87&a=2&flag");
        assert_eq!(fields.get("a"), Some("1"));
        assert_eq!(fields.get_all("a"), vec!["1", "2"]);
        assert_eq!(fields.get("b"), Some("x y"));
        assert_eq!(fields.get("c"), Some("中文"));
        assert!(fields.is_set("flag"));
        assert_eq!(fields.get("flag"), Some(""));
        assert_eq!(fields.get("missing"), None);
    }

    #[test]
    fn round_trips_encoding() {
        let raw = "a b&c/d?e=中";
        let encoded = percent_encode(raw);
        assert_eq!(percent_decode(&encoded), raw);
        assert!(!encoded.contains(' '));
    }
}
