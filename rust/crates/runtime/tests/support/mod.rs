//! Shared helpers for the runtime integration tests.
//!
//! Every helper keeps state inside a private `/tmp` root that is removed on
//! drop, so no test can touch production state or leave children behind.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use pihub_contracts::{
    Carrier, HubResult, LogRecord, LogSink, RuntimeEvent, RuntimeLaunch, SkillRef, ThinkingLevel,
};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

pub struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    pub fn new(label: &str) -> Self {
        let base = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
        let path = base.join(format!(
            "pihub-runtime-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// An isolated launch spec with every private directory created.
pub fn launch(root: &Path, carrier: Carrier, agent_id: &str) -> RuntimeLaunch {
    let agent_root = root.join("agents").join(agent_id);
    let state_dir = agent_root.join("state");
    let tmp_dir = agent_root.join("tmp");
    for directory in [
        agent_root.join("sessions"),
        agent_root.join("logs"),
        state_dir.clone(),
        tmp_dir.clone(),
        agent_root.join(format!("{carrier}-agent")),
    ] {
        std::fs::create_dir_all(&directory).expect("private dir");
    }
    RuntimeLaunch {
        agent_id: agent_id.to_string(),
        name: format!("agent {agent_id}"),
        cwd: root.join("project"),
        workspace_mode: "shared".to_string(),
        carrier,
        provider: None,
        model: None,
        thinking: None,
        session_id: "33333333-3333-4333-8333-333333333333".to_string(),
        session_dir: agent_root.join("sessions"),
        agent_dir: agent_root.join(format!("{carrier}-agent")),
        logs_dir: agent_root.join("logs"),
        state_dir,
        tmp_dir,
        tools: vec!["read".to_string()],
        skills: Vec::new(),
        extensions: Vec::new(),
        authorization: None,
        prompt: None,
    }
}

pub fn with_thinking(mut launch: RuntimeLaunch, thinking: ThinkingLevel) -> RuntimeLaunch {
    launch.thinking = Some(thinking);
    launch
}

pub fn with_provider(mut launch: RuntimeLaunch, provider: &str, model: &str) -> RuntimeLaunch {
    launch.provider = Some(provider.to_string());
    launch.model = Some(model.to_string());
    launch
}

pub fn skill(root: &Path, id: &str) -> SkillRef {
    let directory = root.join("skills").join(id);
    std::fs::create_dir_all(&directory).expect("skill dir");
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {id}\ndescription: fixture\n---\n"),
    )
    .expect("skill file");
    SkillRef::Grant {
        id: id.to_string(),
        path: directory,
        sha256: "0".repeat(64),
    }
}

/// Create a project directory the launch spec points at.
pub fn project(root: &Path) {
    std::fs::create_dir_all(root.join("project")).expect("project dir");
}

/// Drain events until `predicate` matches, or the timeout elapses.
pub async fn collect_until<F>(
    receiver: &mut mpsc::Receiver<RuntimeEvent>,
    predicate: F,
) -> Vec<RuntimeEvent>
where
    F: Fn(&RuntimeEvent) -> bool,
{
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return collected;
        }
        match tokio::time::timeout(remaining, receiver.recv()).await {
            Ok(Some(event)) => {
                let matched = predicate(&event);
                collected.push(event);
                if matched {
                    return collected;
                }
            }
            Ok(None) => return collected,
            Err(_) => return collected,
        }
    }
}

/// Event types seen in a batch, for readable assertions.
pub fn event_types(events: &[RuntimeEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            RuntimeEvent::Event { event_type, .. } => event_type.clone(),
            RuntimeEvent::ProtocolError { .. } => "protocol_error".to_string(),
            RuntimeEvent::ProcessError { .. } => "process_error".to_string(),
            RuntimeEvent::Exit { .. } => "exit".to_string(),
        })
        .collect()
}

pub fn payload_text(events: &[RuntimeEvent], event_type: &str) -> Option<String> {
    events.iter().find_map(|event| match event {
        RuntimeEvent::Event {
            event_type: kind,
            payload,
        } if kind == event_type => {
            let text = payload
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|content| content.get(0))
                .and_then(|part| part.get("text"))
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            text.or_else(|| payload.get("value").map(|value| value.to_string()))
        }
        _ => None,
    })
}

/// Write an executable POSIX shell script and return its path.
pub fn write_script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, body).expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path.to_path_buf()
}

pub fn node_program() -> PathBuf {
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    pihub_runtime::proc::find_executable(Path::new("node"), &env)
        .expect("node runtime is required to exercise the Pi carrier")
}

/// Parse one header line produced by [`FAKE_CLI_SCRIPT`].
pub fn parse_turn_header(line: &str) -> Option<TurnRecord> {
    let value = line.strip_prefix("carrier=")?;
    let parts: Vec<&str> = value.split('\t').collect();
    let mut record = TurnRecord {
        carrier: parts.first().copied().unwrap_or_default().to_string(),
        ..TurnRecord::default()
    };
    let fields_at = parts
        .iter()
        .position(|part| part.starts_with("home="))
        .unwrap_or(parts.len());
    record.args = parts[1..fields_at]
        .iter()
        .map(|part| (*part).to_string())
        .collect();
    for part in &parts[fields_at..] {
        for (prefix, slot) in [
            ("home=", &mut record.home),
            ("pwd=", &mut record.pwd),
            ("path=", &mut record.path),
            ("hubhome=", &mut record.hub_home),
            ("anthropic=", &mut record.anthropic_key),
            ("codexhome=", &mut record.codex_home),
            ("claudehome=", &mut record.claude_home),
        ] {
            if let Some(value) = part.strip_prefix(prefix) {
                *slot = value.to_string();
            }
        }
    }
    Some(record)
}

/// Every turn the fake CLI recorded, in order, with its prompt attached.
pub fn read_turn_records(path: &Path) -> Vec<TurnRecord> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut records: Vec<TurnRecord> = Vec::new();
    for line in text.lines() {
        if let Some(record) = parse_turn_header(line) {
            records.push(record);
            continue;
        }
        if let Some(prompt) = line.strip_prefix("prompt=") {
            if let Some(record) = records.last_mut() {
                record.prompt = prompt.to_string();
            }
        }
    }
    records
}

/// POSIX shell stand-in for the Codex and Claude CLIs (and for the CC Switch
/// launcher): it records argv/env, answers with the same NDJSON event shapes
/// the real CLIs emit, and can hang or exit without a final result.
pub const FAKE_CLI_SCRIPT: &str = r#"#!/bin/sh
log="${FAKE_LOG:-$HOME/pihub-turn.log}"
if [ "$1" = "start" ]; then carrier="$2"; else carrier=$(basename "$0"); fi
{
  printf 'carrier=%s' "$carrier"
  for a in "$@"; do printf '\t%s' "$a"; done
  printf '\thome=%s' "$HOME"
  printf '\tpwd=%s' "$PWD"
  printf '\tpath=%s' "$PATH"
  printf '\thubhome=%s' "$PIHUB_HOME"
  printf '\tanthropic=%s' "$ANTHROPIC_API_KEY"
  printf '\tcodexhome=%s' "$CODEX_HOME"
  printf '\tclaudehome=%s' "$CLAUDE_CONFIG_DIR"
  printf '\n'
  prompt=$(cat)
  printf 'prompt=%s\n' "$prompt"
} >> "$log"

case "$prompt" in
  hang) exec sleep 30 ;;
  empty) exit 0 ;;
esac

if [ "$carrier" = "codex" ]; then
  printf '{"type":"thread.started","thread_id":"11111111-1111-4111-8111-111111111111"}\n'
  printf '{"type":"item.completed","item":{"type":"agent_message","text":"codex:%s"}}\n' "$prompt"
  printf '{"type":"turn.completed"}\n'
else
  printf '{"type":"system","subtype":"init","session_id":"22222222-2222-4222-8222-222222222222"}\n'
  printf '{"type":"result","session_id":"22222222-2222-4222-8222-222222222222","result":"claude:%s","is_error":false}\n' "$prompt"
fi
"#;

#[derive(Debug, Default)]
pub struct TurnRecord {
    pub carrier: String,
    pub args: Vec<String>,
    pub home: String,
    pub pwd: String,
    pub path: String,
    pub hub_home: String,
    pub anthropic_key: String,
    pub codex_home: String,
    pub claude_home: String,
    pub prompt: String,
}

impl TurnRecord {
    pub fn has_arg(&self, value: &str) -> bool {
        self.args.iter().any(|arg| arg == value)
    }

    pub fn arg_after(&self, flag: &str) -> Option<&str> {
        let index = self.args.iter().position(|arg| arg == flag)?;
        self.args.get(index + 1).map(String::as_str)
    }
}

/// In-memory `LogSink` so a test can inspect exactly what would reach `hub.log`.
#[derive(Default)]
pub struct CaptureSink {
    records: Mutex<Vec<LogRecord>>,
}

impl CaptureSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn records(&self) -> Vec<LogRecord> {
        self.records.lock().expect("sink lock").clone()
    }

    pub fn kinds(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter_map(|record| record.kind)
            .collect()
    }

    /// Everything the sink was handed, flattened into one searchable string.
    pub fn dump(&self) -> String {
        let mut text = String::new();
        for record in self.records() {
            text.push_str(record.kind.as_deref().unwrap_or_default());
            text.push('\t');
            text.push_str(record.direction.as_deref().unwrap_or_default());
            text.push('\t');
            text.push_str(record.message.as_deref().unwrap_or_default());
            text.push('\t');
            if let Some(summary) = record.summary {
                text.push_str(&summary.to_string());
            }
            text.push('\t');
            if let Some(data) = record.data {
                text.push_str(&data.to_string());
            }
            text.push('\n');
        }
        text
    }

    /// Stored data payloads exactly as a sink would write them (strings stay
    /// unquoted so the cap can be asserted on the real content).
    pub fn data_payloads(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter_map(|record| record.data)
            .map(|value| match value {
                serde_json::Value::String(text) => text,
                other => other.to_string(),
            })
            .collect()
    }
}

#[pihub_contracts::async_trait]
impl LogSink for CaptureSink {
    fn record(&self, record: LogRecord) {
        self.records.lock().expect("sink lock").push(record);
    }

    async fn close(&self) -> HubResult<()> {
        Ok(())
    }
}

/// A prompt body that must never reach the Hub log in normal mode.
pub const MARKER_PROMPT: &str = "MARKER-SECRET-TOKEN-1234567890";
