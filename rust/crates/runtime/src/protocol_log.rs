//! Bounded protocol logging for the carrier runtimes (CR-CORE-2).
//!
//! `hub.log` is a shared, rotating file family: it must never accumulate prompt
//! or model output. Normal protocol logging therefore records only a *summary*
//! (event type, ids, command, counts and key names — never a body) in
//! `LogRecord.summary`, and never sets `LogRecord.data`.
//!
//! Raw protocol frames are available only when the operator opts in with
//! `PIHUB_LOG_DEBUG=1`; they are written as `kind=protocol_raw` with a hard
//! cap on characters *and* bytes, so a long prompt or stream delta cannot
//! bloat the log family either.

use pihub_contracts::{Carrier, LogLevel, LogRecord, LogSink};
use serde_json::{Map, Value, json};

/// Maximum characters kept from a raw protocol frame.
pub const RAW_MAX_CHARS: usize = 512;
/// Maximum bytes kept from a raw protocol frame.
pub const RAW_MAX_BYTES: usize = 16 * 1024;
/// Maximum key names reported in a summary.
pub const SUMMARY_MAX_KEYS: usize = 16;

/// `PIHUB_LOG_DEBUG=1` enables raw protocol frames.
pub fn debug_enabled() -> bool {
    matches!(std::env::var("PIHUB_LOG_DEBUG").as_deref(), Ok("1"))
}

/// Truncate to both a character and a byte budget, marking the cut.
pub fn bounded_text(text: &str, max_chars: usize, max_bytes: usize) -> String {
    if text.chars().count() <= max_chars && text.len() <= max_bytes {
        return text.to_string();
    }
    let mut output = String::new();
    for (count, character) in text.chars().enumerate() {
        if count >= max_chars || output.len() + character.len_utf8() > max_bytes {
            break;
        }
        output.push(character);
    }
    output.push_str("…(truncated)");
    output
}

/// Structural summary of one protocol frame. Bodies are never copied: only the
/// frame's identity, its top-level key names, and its size are reported.
pub fn summary(value: &Value) -> Value {
    let mut summary = Map::new();
    for key in ["type", "command", "id", "success", "subtype"] {
        let Some(field) = value.get(key) else {
            continue;
        };
        match field {
            Value::String(text) => {
                summary.insert(key.to_string(), json!(bounded_text(text, 120, 256)));
            }
            Value::Bool(flag) => {
                summary.insert(key.to_string(), json!(flag));
            }
            _ => {}
        }
    }
    let serialized = value.to_string();
    summary.insert("chars".to_string(), json!(serialized.chars().count()));
    summary.insert("bytes".to_string(), json!(serialized.len()));
    if let Some(object) = value.as_object() {
        let mut keys: Vec<String> = object.keys().cloned().collect();
        keys.sort();
        keys.truncate(SUMMARY_MAX_KEYS);
        summary.insert("keys".to_string(), json!(keys));
    }
    Value::Object(summary)
}

/// Build the protocol record for one frame.
///
/// `debug` is resolved once per runtime from [`debug_enabled`]; keeping it a
/// parameter makes the two modes directly testable.
pub fn record(
    agent_id: &str,
    carrier: Carrier,
    source: &str,
    direction: &str,
    value: &Value,
    debug: bool,
) -> LogRecord {
    let mut base = LogRecord {
        agent_id: Some(agent_id.to_string()),
        carrier: Some(carrier),
        source: Some(source.to_string()),
        direction: Some(direction.to_string()),
        ..LogRecord::default()
    };
    if debug {
        base.level = Some(LogLevel::Debug);
        base.kind = Some("protocol_raw".to_string());
        base.data = Some(Value::String(bounded_text(
            &value.to_string(),
            RAW_MAX_CHARS,
            RAW_MAX_BYTES,
        )));
        return base;
    }
    base.kind = Some("protocol".to_string());
    base.summary = Some(summary(value));
    base
}

/// Convenience used by both carriers: one bounded record per frame.
pub fn write(
    sink: &Option<std::sync::Arc<dyn LogSink>>,
    agent_id: &str,
    carrier: Carrier,
    source: &str,
    direction: &str,
    value: &Value,
    debug: bool,
) {
    if let Some(sink) = sink {
        sink.record(record(agent_id, carrier, source, direction, value, debug));
    }
}

#[cfg(test)]
mod tests {
    use super::{RAW_MAX_BYTES, RAW_MAX_CHARS, bounded_text, record as protocol_record, summary};
    use pihub_contracts::Carrier;
    use serde_json::json;

    const MARKER: &str = "MARKER-SECRET-TOKEN-1234567890";

    fn frame() -> serde_json::Value {
        json!({
            "id": "11111111-1111-4111-8111-111111111111",
            "type": "prompt",
            "message": MARKER,
            "content": [{"type": "text", "text": MARKER}],
            "delta": MARKER,
        })
    }

    #[test]
    fn summary_never_copies_a_body_and_never_sets_data() {
        let value = frame();
        let summary = summary(&value);
        let rendered = summary.to_string();
        assert!(!rendered.contains(MARKER), "{rendered}");
        // Only *key names* may appear; no field value is ever copied.
        assert!(!rendered.contains("MARKER"), "{rendered}");
        assert!(!rendered.contains("text\":\""), "{rendered}");

        let record = protocol_record("agent-1", Carrier::Pi, "pi-rpc", "in", &value, false);
        assert_eq!(record.kind.as_deref(), Some("protocol"));
        assert!(record.data.is_none(), "normal logging never sets data");
        assert!(record.summary.is_some());
        assert_eq!(record.direction.as_deref(), Some("in"));
        assert_eq!(record.agent_id.as_deref(), Some("agent-1"));
        let rendered = record.summary.unwrap().to_string();
        assert!(!rendered.contains(MARKER));
        // Identity and size are what an operator actually needs.
        assert!(rendered.contains("\"type\":\"prompt\""), "{rendered}");
        assert!(rendered.contains("\"command\"") == false, "{rendered}");
        assert!(rendered.contains("\"bytes\":"), "{rendered}");
        assert!(rendered.contains("\"chars\":"), "{rendered}");
        assert!(rendered.contains("\"keys\":"), "{rendered}");
    }

    #[test]
    fn raw_mode_is_opt_in_bounded_and_marked() {
        let value = frame();
        let record = protocol_record("agent-1", Carrier::Claude, "cli-runtime", "out", &value, true);
        assert_eq!(record.kind.as_deref(), Some("protocol_raw"));
        assert!(record.summary.is_none());
        let data = record.data.expect("raw payload");
        let text = data.as_str().expect("raw payload is a string");
        assert!(text.contains(MARKER), "debug mode keeps the frame");

        // A frame far beyond the cap is truncated on both axes.
        let huge = json!({"type": "message_update", "delta": "x".repeat(100_000)});
        let record = protocol_record("agent-1", Carrier::Pi, "pi-rpc", "out", &huge, true);
        let text = record
            .data
            .expect("raw")
            .as_str()
            .expect("string")
            .to_string();
        // Bounded on both axes; the truncation marker is the only overflow.
        assert!(text.chars().count() <= RAW_MAX_CHARS + 16, "{}", text.chars().count());
        assert!(text.len() <= RAW_MAX_BYTES + 64, "{}", text.len());
        assert!(text.ends_with("…(truncated)"), "{text}");
    }

    #[test]
    fn bounded_text_keeps_short_values_verbatim_and_respects_both_limits() {
        assert_eq!(bounded_text("short", 8, 32), "short");
        assert_eq!(bounded_text("abcdef", 3, 32), "abc…(truncated)");
        // Multi-byte characters are never split in half.
        let text = bounded_text("中文中文", 3, 64);
        assert!(text.starts_with("中文中"));
        let text = bounded_text("aaaaaaaaaa", 100, 5);
        assert!(text.len() <= 5 + "…(truncated)".len());
    }
}
