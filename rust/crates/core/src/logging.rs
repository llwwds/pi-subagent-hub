//! Bounded, single-writer Hub diagnostic log (`src/logging.js`).
//!
//! One writer per Hub home, never one file per Agent. Rotation is delegated to
//! `file-rotate` with a byte limit per segment and a retained-file count, so the
//! whole family stays bounded. Streaming deltas are never written here: they are
//! progress, not history (see [`is_ephemeral_stream_event`]).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use file_rotate::compression::Compression;
use file_rotate::suffix::AppendCount;
use file_rotate::{ContentLimit, FileRotate};
use pihub_contracts::{
    now_iso, Carrier, HubError, HubResult, HubPaths, LogLevel, LogRecord, LogSink,
};
use serde_json::{json, Map, Value};

use crate::fsutil;

pub const DEFAULT_LOG_FILE_NAME: &str = "hub.log";
pub const DEFAULT_MAX_SEGMENT_BYTES: usize = 10 * 1024 * 1024;
pub const DEFAULT_MAX_ROTATED_FILES: usize = 5;
pub const DEFAULT_MAX_ENTRY_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_STRING_CHARS: usize = 2048;
pub const DEFAULT_MAX_STDERR_LINE_BYTES: usize = 8 * 1024;

const BOUND_MAX_DEPTH: usize = 8;
const BOUND_MAX_ARRAY: usize = 50;
const BOUND_MAX_KEYS: usize = 50;
const MAX_TRACKED_KEYS: usize = 16;

/// `stateDir/logs`, matching `resolveHubLogDir`.
pub fn resolve_hub_log_dir(paths: &HubPaths) -> PathBuf {
    paths.state_dir.join("logs")
}

#[derive(Debug, Clone)]
pub struct HubLogLimits {
    pub file_name: String,
    pub max_segment_bytes: usize,
    pub max_rotated_files: usize,
    pub max_entry_bytes: usize,
    pub max_string_chars: usize,
    pub max_stderr_line_bytes: usize,
}

impl Default for HubLogLimits {
    fn default() -> Self {
        Self {
            file_name: DEFAULT_LOG_FILE_NAME.to_string(),
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
            max_rotated_files: DEFAULT_MAX_ROTATED_FILES,
            max_entry_bytes: DEFAULT_MAX_ENTRY_BYTES,
            max_string_chars: DEFAULT_MAX_STRING_CHARS,
            max_stderr_line_bytes: DEFAULT_MAX_STDERR_LINE_BYTES,
        }
    }
}

impl HubLogLimits {
    pub fn validate(&self) -> HubResult<()> {
        let plain = !self.file_name.is_empty()
            && self
                .file_name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'));
        if !plain {
            return Err(HubError::new(
                "invalid_log_limits",
                "log fileName must be a plain file name",
            ));
        }
        if self.max_segment_bytes == 0 || self.max_rotated_files == 0 {
            return Err(HubError::new(
                "invalid_log_limits",
                "log segment size and retained file count must be positive",
            ));
        }
        if self.max_entry_bytes < 512 {
            return Err(HubError::new(
                "invalid_log_limits",
                "maxEntryBytes must be at least 512",
            ));
        }
        Ok(())
    }

    /// Total retained capacity of the family (current segment plus rotations).
    pub fn total_capacity_bytes(&self) -> usize {
        self.max_segment_bytes
            .saturating_mul(self.max_rotated_files.saturating_add(1))
    }
}

/// Truncate a string to a JavaScript-style ellipsis marker.
fn truncate_text(value: &str, max_chars: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= max_chars {
        return value.to_string();
    }
    let mut end = 0;
    let mut count = 0;
    for character in value.chars() {
        let width = character.len_utf16();
        if count + width > max_chars {
            break;
        }
        count += width;
        end += character.len_utf8();
    }
    format!(
        "{}…[truncated {} chars]",
        &value[..end],
        units.len() - count
    )
}

fn bound_value(value: &Value, max_string_chars: usize, depth: usize) -> Value {
    match value {
        Value::String(text) => Value::String(truncate_text(text, max_string_chars)),
        Value::Array(items) => {
            if depth >= BOUND_MAX_DEPTH {
                return Value::String("[max-depth]".to_string());
            }
            let mut bound: Vec<Value> = items
                .iter()
                .take(BOUND_MAX_ARRAY)
                .map(|item| bound_value(item, max_string_chars, depth + 1))
                .collect();
            if items.len() > BOUND_MAX_ARRAY {
                bound.push(Value::String(format!(
                    "[{} more items]",
                    items.len() - BOUND_MAX_ARRAY
                )));
            }
            Value::Array(bound)
        }
        Value::Object(map) => {
            if depth >= BOUND_MAX_DEPTH {
                return Value::String("[max-depth]".to_string());
            }
            let mut bound = Map::new();
            for (index, (key, item)) in map.iter().enumerate() {
                if index >= BOUND_MAX_KEYS {
                    break;
                }
                bound.insert(key.clone(), bound_value(item, max_string_chars, depth + 1));
            }
            if map.len() > BOUND_MAX_KEYS {
                bound.insert(
                    "…".to_string(),
                    Value::String(format!("[{} more keys]", map.len() - BOUND_MAX_KEYS)),
                );
            }
            Value::Object(bound)
        }
        other => other.clone(),
    }
}

fn summarize_message(message: &Value, max_string_chars: usize) -> Value {
    let Some(object) = message.as_object() else {
        return json!({ "valueType": js_type_name(message) });
    };
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut block_types: Vec<String> = Vec::new();
    let mut chars_by_type = Map::new();
    let mut tool_calls = 0u64;
    for block in &content {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        if !block_types.iter().any(|existing| existing == &block_type) {
            block_types.push(block_type.clone());
        }
        if matches!(block_type.as_str(), "toolCall" | "tool_call" | "tool_use") {
            tool_calls += 1;
        }
        let text = block
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| block.get("thinking").and_then(Value::as_str))
            .unwrap_or("");
        if !text.is_empty() {
            let chars = text.encode_utf16().count() as u64;
            let previous = chars_by_type
                .get(&block_type)
                .and_then(Value::as_u64)
                .unwrap_or(0);
            chars_by_type.insert(block_type, json!(previous + chars));
        }
    }
    block_types.truncate(12);
    let mut summary = Map::new();
    summary.insert(
        "role".into(),
        object
            .get("role")
            .and_then(Value::as_str)
            .map(|role| json!(truncate_text(role, max_string_chars)))
            .unwrap_or(Value::Null),
    );
    summary.insert(
        "stopReason".into(),
        object
            .get("stopReason")
            .and_then(Value::as_str)
            .map(|reason| json!(truncate_text(reason, max_string_chars)))
            .unwrap_or(Value::Null),
    );
    summary.insert("contentBlocks".into(), json!(content.len()));
    summary.insert("blockTypes".into(), json!(block_types));
    summary.insert("charsByType".into(), Value::Object(chars_by_type));
    summary.insert("toolCalls".into(), json!(tool_calls));
    if let Some(text) = object.get("text").and_then(Value::as_str) {
        summary.insert("textChars".into(), json!(text.encode_utf16().count()));
    }
    Value::Object(summary)
}

fn js_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "object",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "object",
        Value::Object(_) => "object",
    }
}

/// JavaScript `String(value)`: strings stay unquoted, everything else renders
/// through JSON.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn safe_stringify(value: &Value) -> Option<String> {
    serde_json::to_string(value).ok()
}

/// Safe, non-secret view of one protocol value (`summarizeProtocol`).
pub fn summarize_protocol(direction: &str, value: &Value, max_string_chars: usize) -> Value {
    let mut summary = Map::new();
    summary.insert("direction".into(), json!(direction));
    let Some(object) = value.as_object() else {
        summary.insert("valueType".into(), json!(js_type_name(value)));
        if let Some(text) = value.as_str() {
            summary.insert("chars".into(), json!(text.encode_utf16().count()));
        }
        return Value::Object(summary);
    };
    if let Some(raw) = safe_stringify(value) {
        summary.insert("bytes".into(), json!(raw.len()));
    }
    let text_field = |name: &str, target: &mut Map<String, Value>| {
        if let Some(value) = object.get(name) {
            target.insert(
                name.into(),
                json!(truncate_text(&value_text(value), max_string_chars)),
            );
        }
    };
    text_field("type", &mut summary);
    text_field("id", &mut summary);
    text_field("command", &mut summary);
    if let Some(success) = object.get("success").and_then(Value::as_bool) {
        summary.insert("success".into(), json!(success));
    }
    text_field("carrier", &mut summary);
    let keys: Vec<&String> = object.keys().take(20).collect();
    summary.insert("keys".into(), json!(keys));
    if let Some(error) = object.get("error").filter(|value| !value.is_null()) {
        summary.insert(
            "error".into(),
            json!(truncate_text(&value_text(error), max_string_chars)),
        );
    }
    if let Some(message) = object.get("message") {
        summary.insert("message".into(), summarize_message(message, max_string_chars));
    }
    if let Some(inner) = object.get("value").filter(|value| value.is_object()) {
        if let Some(inner_object) = inner.as_object() {
            if let Some(inner_type) = inner_object.get("type") {
                summary.insert(
                    "innerType".into(),
                    json!(truncate_text(&value_text(inner_type), max_string_chars)),
                );
            }
            let inner_keys: Vec<&String> = inner_object.keys().take(20).collect();
            summary.insert("innerKeys".into(), json!(inner_keys));
            if let Some(inner_message) = inner_object.get("message") {
                summary.insert(
                    "innerMessage".into(),
                    summarize_message(inner_message, max_string_chars),
                );
            }
        }
    }
    if let Some(event) = object
        .get("assistantMessageEvent")
        .filter(|value| value.is_object())
    {
        if let Some(event_object) = event.as_object() {
            if let Some(delta_type) = event_object.get("type") {
                summary.insert(
                    "deltaType".into(),
                    json!(truncate_text(&value_text(delta_type), max_string_chars)),
                );
            }
            let delta = event_object
                .get("delta")
                .or_else(|| event_object.get("text"))
                .or_else(|| event_object.get("thinking"))
                .and_then(Value::as_str);
            if let Some(delta) = delta {
                summary.insert("deltaChars".into(), json!(delta.encode_utf16().count()));
            }
        }
    }
    if let Some(data) = object.get("data").filter(|value| value.is_object()) {
        if let Some(data_object) = data.as_object() {
            let data_keys: Vec<&String> = data_object.keys().take(20).collect();
            summary.insert("dataKeys".into(), json!(data_keys));
        }
    }
    Value::Object(summary)
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

/// Only real write failures are counted; there is no queue and therefore no
/// pressure-drop policy to account for.
#[derive(Debug, Default)]
struct DropCounters {
    total: u64,
    by_kind: BTreeMap<String, u64>,
    by_reason: BTreeMap<String, u64>,
}

fn bump(counter: &mut BTreeMap<String, u64>, key: &str) {
    if !counter.contains_key(key) && counter.len() >= MAX_TRACKED_KEYS {
        *counter.entry("other".to_string()).or_insert(0) += 1;
        return;
    }
    *counter.entry(key.to_string()).or_insert(0) += 1;
}

/// Single Hub-level log writer.
///
/// Writes are inline and synchronous: `file-rotate` appends straight to the
/// current segment, so there is no in-memory queue to bound and no
/// pressure-drop policy. The family stays bounded by the byte limit per segment
/// and the retained-file count; an entry is only counted as dropped when the
/// write itself fails, and the failure reason is recorded in `stats()`.
pub struct HubLog {
    limits: HubLogLimits,
    file_path: Option<PathBuf>,
    writer: Mutex<Option<FileRotate<AppendCount>>>,
    written: AtomicU64,
    written_bytes: AtomicU64,
    dropped: Mutex<DropCounters>,
    error: Mutex<Option<String>>,
    closed: AtomicU64,
}

impl std::fmt::Debug for HubLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubLog")
            .field("file_path", &self.file_path)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl HubLog {
    pub fn open(dir: &Path) -> HubResult<Self> {
        Self::with_limits(dir, HubLogLimits::default())
    }

    pub fn with_limits(dir: &Path, limits: HubLogLimits) -> HubResult<Self> {
        limits.validate()?;
        fsutil::ensure_private_dir(dir)?;
        let file_path = dir.join(&limits.file_name);
        let mut options = OpenOptions::new();
        options.read(true).create(true).append(true).mode(0o600);
        let writer = FileRotate::new(
            &file_path,
            AppendCount::new(limits.max_rotated_files),
            ContentLimit::BytesSurpassed(limits.max_segment_bytes),
            Compression::None,
            Some(options),
        );
        if let Err(error) = fsutil::chmod(&file_path, fsutil::PRIVATE_FILE_MODE) {
            return Err(HubError::new(
                "io_error",
                format!("cannot set log file mode: {error}"),
            ));
        }
        Ok(Self {
            limits,
            file_path: Some(file_path),
            writer: Mutex::new(Some(writer)),
            written: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            dropped: Mutex::new(DropCounters::default()),
            error: Mutex::new(None),
            closed: AtomicU64::new(0),
        })
    }

    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    pub fn limits(&self) -> &HubLogLimits {
        &self.limits
    }

    pub fn stats(&self) -> Value {
        let (dropped_total, by_kind, by_reason) = self
            .dropped
            .lock()
            .map(|counters| {
                (
                    counters.total,
                    counters.by_kind.clone(),
                    counters.by_reason.clone(),
                )
            })
            .unwrap_or_default();
        json!({
            "filePath": self.file_path.as_ref().map(|path| path.to_string_lossy().into_owned()),
            "written": self.written.load(Ordering::Relaxed),
            "writtenBytes": self.written_bytes.load(Ordering::Relaxed),
            "dropped": {
                "total": dropped_total,
                "byKind": by_kind,
                "byReason": by_reason,
            },
            "error": self.error.lock().ok().and_then(|value| value.clone()),
            "closed": self.closed.load(Ordering::Relaxed) == 1,
        })
    }

    fn base_record(&self, record: &LogRecord) -> Map<String, Value> {
        let mut line = Map::new();
        line.insert(
            "ts".into(),
            json!(record.at.clone().unwrap_or_else(now_iso)),
        );
        line.insert(
            "level".into(),
            json!(level_name(record.level.unwrap_or(LogLevel::Info))),
        );
        line.insert(
            "kind".into(),
            json!(record.kind.clone().unwrap_or_else(|| "note".to_string())),
        );
        if let Some(agent_id) = &record.agent_id {
            line.insert("agentId".into(), json!(agent_id));
        }
        if let Some(carrier) = record.carrier {
            line.insert("carrier".into(), json!(carrier.as_str()));
        }
        if let Some(source) = &record.source {
            line.insert("source".into(), json!(source));
        }
        if let Some(direction) = &record.direction {
            line.insert("direction".into(), json!(direction));
        }
        if let Some(message) = &record.message {
            line.insert("message".into(), json!(message));
        }
        if let Some(summary) = &record.summary {
            line.insert("summary".into(), summary.clone());
        }
        if let Some(data) = &record.data {
            line.insert("data".into(), data.clone());
        }
        line
    }

    fn format_line(&self, record: &LogRecord) -> String {
        let base = self.base_record(record);
        let bounded = bound_value(
            &Value::Object(base.clone()),
            self.limits.max_string_chars,
            0,
        );
        let mut text = safe_stringify(&bounded).unwrap_or_else(|| {
            safe_stringify(&json!({
                "ts": base.get("ts").cloned().unwrap_or(Value::Null),
                "level": base.get("level").cloned().unwrap_or(Value::Null),
                "kind": base.get("kind").cloned().unwrap_or(Value::Null),
                "message": "unserializable log record",
            }))
            .unwrap_or_else(|| "{}".to_string())
        });
        if text.len() > self.limits.max_entry_bytes {
            // Fall back to a bounded preview of whichever payload was supplied.
            let raw = match (&record.data, &record.message, &record.summary) {
                (Some(data), _, _) => safe_stringify(data),
                (None, Some(message), _) => safe_stringify(&Value::String(message.clone())),
                (None, None, Some(summary)) => safe_stringify(summary),
                _ => None,
            }
            .unwrap_or_default();
            let preview_budget = self.limits.max_entry_bytes.saturating_sub(400).max(256);
            let preview = fsutil::truncate_bytes(&raw, preview_budget);
            let fallback = json!({
                "ts": base.get("ts").cloned().unwrap_or(Value::Null),
                "level": base.get("level").cloned().unwrap_or(Value::Null),
                "kind": base.get("kind").cloned().unwrap_or(Value::Null),
                "agentId": base.get("agentId").cloned().unwrap_or(Value::Null),
                "carrier": base.get("carrier").cloned().unwrap_or(Value::Null),
                "source": base.get("source").cloned().unwrap_or(Value::Null),
                "direction": base.get("direction").cloned().unwrap_or(Value::Null),
                "truncated": true,
                "originalBytes": raw.len(),
                "preview": preview,
            });
            let fallback_text = safe_stringify(&fallback).unwrap_or_default();
            text = if fallback_text.len() <= self.limits.max_entry_bytes {
                fallback_text
            } else {
                fsutil::truncate_bytes(&fallback_text, self.limits.max_entry_bytes)
            };
        }
        format!("{text}\n")
    }

    fn write_line(&self, line: &str) -> bool {
        if self.closed.load(Ordering::Relaxed) == 1 {
            return false;
        }
        if self.error.lock().map(|value| value.is_some()).unwrap_or(true) {
            return false;
        }
        let mut guard = match self.writer.lock() {
            Ok(guard) => guard,
            Err(_) => {
                self.set_error("log writer lock poisoned".to_string());
                return false;
            }
        };
        let Some(writer) = guard.as_mut() else {
            return false;
        };
        match writer.write_all(line.as_bytes()) {
            Ok(()) => {
                self.written.fetch_add(1, Ordering::Relaxed);
                self.written_bytes
                    .fetch_add(line.len() as u64, Ordering::Relaxed);
                true
            }
            Err(error) => {
                self.set_error(error.to_string());
                false
            }
        }
    }

    fn set_error(&self, message: String) {
        if let Ok(mut guard) = self.error.lock() {
            if guard.is_none() {
                *guard = Some(message);
            }
        }
    }

    /// Explicit close: flush and release the segment before the process exits.
    pub fn close_sync(&self) -> HubResult<()> {
        if self.closed.swap(1, Ordering::Relaxed) == 1 {
            return Ok(());
        }
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| HubError::new("log_unavailable", "log writer lock poisoned"))?;
        if let Some(writer) = guard.as_mut() {
            writer
                .flush()
                .map_err(|error| HubError::new("log_flush_failed", error.to_string()))?;
        }
        guard.take();
        Ok(())
    }
}

#[async_trait::async_trait]
impl LogSink for HubLog {
    fn record(&self, record: LogRecord) {
        let line = self.format_line(&record);
        if !self.write_line(&line) {
            if let Ok(mut dropped) = self.dropped.lock() {
                dropped.total += 1;
                let kind = record.kind.clone().unwrap_or_else(|| "note".to_string());
                bump(&mut dropped.by_kind, &kind);
                bump(&mut dropped.by_reason, "write_failed");
            }
        }
    }

    async fn close(&self) -> HubResult<()> {
        self.close_sync()
    }
}

/// Per-Agent adapter: owns no file and no queue, only tags records.
#[derive(Clone)]
pub struct AgentLog {
    sink: std::sync::Arc<dyn LogSink>,
    agent_id: String,
    carrier: Carrier,
    source: String,
}

impl AgentLog {
    pub fn new(
        sink: std::sync::Arc<dyn LogSink>,
        agent_id: impl Into<String>,
        carrier: Carrier,
        source: impl Into<String>,
    ) -> Self {
        Self {
            sink,
            agent_id: agent_id.into(),
            carrier,
            source: source.into(),
        }
    }

    pub fn sink(&self) -> std::sync::Arc<dyn LogSink> {
        self.sink.clone()
    }

    fn base(&self, kind: &str, level: LogLevel) -> LogRecord {
        LogRecord {
            level: Some(level),
            kind: Some(kind.to_string()),
            agent_id: Some(self.agent_id.clone()),
            carrier: Some(self.carrier),
            source: Some(self.source.clone()),
            ..LogRecord::default()
        }
    }

    pub fn note(&self, message: impl Into<String>, data: Option<Value>, level: LogLevel) {
        let mut record = self.base("agent_note", level);
        record.message = Some(message.into());
        record.data = data;
        self.sink.record(record);
    }

    /// Summarised protocol record; raw payload text never reaches the log.
    pub fn protocol(&self, direction: &str, value: &Value) {
        let mut record = self.base("protocol", LogLevel::Debug);
        record.direction = Some(direction.to_string());
        record.summary = Some(summarize_protocol(
            direction,
            value,
            DEFAULT_MAX_STRING_CHARS,
        ));
        self.sink.record(record);
    }

    /// Debug-only raw protocol record.
    pub fn protocol_raw(&self, direction: &str, value: &Value) {
        let mut record = self.base("protocol_raw", LogLevel::Debug);
        record.direction = Some(direction.to_string());
        record.data = Some(value.clone());
        self.sink.record(record);
    }

    pub fn stderr_line(&self, line: &str, truncated: bool) {
        let mut record = self.base("stderr", LogLevel::Warn);
        record.message = Some(truncate_text(line, self.max_stderr_chars()));
        if truncated {
            record.data = Some(json!({ "truncated": true }));
        }
        self.sink.record(record);
    }

    fn max_stderr_chars(&self) -> usize {
        DEFAULT_MAX_STDERR_LINE_BYTES
    }
}

/// Split a byte stream into log lines, decoding multibyte text and capping a
/// runaway partial line, matching the 2.1 `LineSink`.
#[derive(Debug, Default)]
pub struct StderrLineSplitter {
    buffer: String,
    max_line_bytes: usize,
}

impl StderrLineSplitter {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            buffer: String::new(),
            max_line_bytes: if max_line_bytes == 0 {
                DEFAULT_MAX_STDERR_LINE_BYTES
            } else {
                max_line_bytes
            },
        }
    }

    /// Feed decoded text; returns complete lines with a truncation flag.
    pub fn push(&mut self, chunk: &str) -> Vec<(String, bool)> {
        self.buffer.push_str(chunk);
        let mut lines = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let mut line = self.buffer[..index].to_string();
            self.buffer = self.buffer[index + 1..].to_string();
            if line.ends_with('\r') {
                line.pop();
            }
            lines.push((line, false));
        }
        if self.buffer.len() > self.max_line_bytes {
            let truncated = truncate_text(&self.buffer, self.max_line_bytes);
            self.buffer.clear();
            lines.push((truncated, true));
        }
        lines
    }

    /// Flush the trailing partial line.
    pub fn finish(&mut self) -> Option<(String, bool)> {
        if self.buffer.is_empty() {
            return None;
        }
        let mut line = std::mem::take(&mut self.buffer);
        if line.ends_with('\r') {
            line.pop();
        }
        Some((line, false))
    }
}
