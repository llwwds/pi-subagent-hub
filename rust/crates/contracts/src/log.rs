use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::HubResult;
use crate::ids::Carrier;

/// The eight pure streaming delta types. Never persisted and never consume a
/// cursor. Must stay byte-identical to `STREAM_DELTA_TYPES` in 2.1.
pub const STREAM_DELTA_TYPES: [&str; 8] = [
    "message_update",
    "message_delta",
    "text_delta",
    "thinking_delta",
    "content_block_delta",
    "stream_event",
    "tool_execution_update",
    "item.updated",
];

/// Carrier wrappers are unwrapped up to this depth; beyond it, fail closed
/// (treat the event as semantic and persist it).
pub const MAX_CARRIER_UNWRAP_DEPTH: usize = 32;

/// Match 2.1 `isEphemeralStreamEvent`: check `type`, then unwrap
/// `{type:"carrier_event", value:{...}}` up to 32 layers. A non-object or
/// unknown shape is not ephemeral.
pub fn is_ephemeral_stream_event(value: &Value) -> bool {
    let mut current = value;
    for depth in 0..=MAX_CARRIER_UNWRAP_DEPTH {
        let Some(object) = current.as_object() else {
            return false;
        };
        let Some(event_type) = object.get("type").and_then(Value::as_str) else {
            return false;
        };
        if STREAM_DELTA_TYPES.contains(&event_type) {
            return true;
        }
        if event_type != "carrier_event" || depth == MAX_CARRIER_UNWRAP_DEPTH {
            return false;
        }
        let Some(next) = object.get("value") else {
            return false;
        };
        current = next;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Structured log record. Runtime adapters use it for protocol/stderr notes;
/// core also writes lifecycle notes. The sink stamps `at` when omitted.
#[derive(Debug, Clone, Default)]
pub struct LogRecord {
    pub level: Option<LogLevel>,
    /// e.g. `agent_note`, `protocol`, `protocol_raw`, `stderr`.
    pub kind: Option<String>,
    pub agent_id: Option<String>,
    pub carrier: Option<Carrier>,
    pub source: Option<String>,
    pub direction: Option<String>,
    pub message: Option<String>,
    pub summary: Option<Value>,
    pub data: Option<Value>,
    pub at: Option<String>,
}

/// Single Hub file-family writer. `record` is synchronous and must never block
/// on unbounded queues; `close` flushes and releases the stream.
#[async_trait::async_trait]
pub trait LogSink: Send + Sync {
    fn record(&self, record: LogRecord);
    async fn close(&self) -> HubResult<()>;
}
