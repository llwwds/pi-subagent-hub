//! Pi carrier: a long-lived `--mode rpc` child speaking NDJSON on
//! stdin/stdout, exactly like `src/rpc-process.js`.
//!
//! Design notes:
//! - Requests are correlated by id and each has a bounded timeout. Normal
//!   event consumption keeps responses fast. Events are delivered into core's
//!   bounded channel; if the consumer stops draining, the reader blocks on
//!   event delivery, stdout is no longer drained, and a later response
//!   (including `get_state`) can hit its declared timeout. This is deliberate
//!   bounded backpressure, not a no-starvation guarantee.
//! - There is no unbounded event buffer. The child's pipe and the bounded
//!   channel provide backpressure; stop still terminates the process tree.
//! - On child exit every pending request is rejected with the 2.1 wording and a
//!   single `Exit` signal is emitted; the runtime never writes the store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use pihub_contracts::{
    Carrier, HubError, HubResult, LogRecord, LogSink, ModelRef, Runtime, RuntimeEvent,
    RuntimeLaunch, RuntimeRequestKind, RuntimeState, now_iso,
};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::env::RuntimeEnv;
use crate::jsonl::{JsonlReader, JsonlRecord};
use crate::proc;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const STATE_TIMEOUT: Duration = Duration::from_secs(15);
pub const CLEAR_QUEUE_TIMEOUT: Duration = Duration::from_secs(2);
pub const ABORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-verb timeout, matching the 2.1 call sites.
pub fn request_timeout(kind: RuntimeRequestKind) -> Duration {
    match kind {
        RuntimeRequestKind::GetState => STATE_TIMEOUT,
        RuntimeRequestKind::ClearQueue => CLEAR_QUEUE_TIMEOUT,
        RuntimeRequestKind::Abort => ABORT_TIMEOUT,
        _ => DEFAULT_REQUEST_TIMEOUT,
    }
}

/// Everything needed to launch the Pi RPC child. `env` is already the isolated
/// whitelist computed by `carriers`, so no ambient variable leaks in here.
#[derive(Debug, Clone)]
pub struct PiSpec {
    pub launch: RuntimeLaunch,
    /// Node runtime that executes the pinned Pi bundle (argv[0] in 2.1 is
    /// `process.execPath`).
    pub program: PathBuf,
    /// `.../pi-coding-agent/dist/bundle/cli.js`.
    pub cli_path: PathBuf,
    pub env: RuntimeEnv,
    /// `PIHUB_LOG_DEBUG=1` opt-in for bounded raw protocol frames.
    pub protocol_debug: bool,
}

#[derive(Default)]
struct Inner {
    stdin: Option<ChildStdin>,
    pending: HashMap<String, oneshot::Sender<HubResult<Value>>>,
    started: bool,
}

pub struct PiRpcProcess {
    spec: Arc<PiSpec>,
    events: mpsc::Sender<RuntimeEvent>,
    log: Option<Arc<dyn LogSink>>,
    inner: Arc<Mutex<Inner>>,
    alive: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
}

impl PiRpcProcess {
    pub fn new(
        spec: PiSpec,
        events: mpsc::Sender<RuntimeEvent>,
        log: Option<Arc<dyn LogSink>>,
    ) -> Self {
        Self {
            spec: Arc::new(spec),
            events,
            log,
            inner: Arc::new(Mutex::new(Inner::default())),
            alive: Arc::new(AtomicBool::new(false)),
            pid: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn launch(&self) -> &RuntimeLaunch {
        &self.spec.launch
    }

    fn args(&self) -> Vec<String> {
        let launch = &self.spec.launch;
        let mut args = vec![
            "--mode".to_string(),
            "rpc".to_string(),
            "--session-dir".to_string(),
            launch.session_dir.to_string_lossy().into_owned(),
            "--session-id".to_string(),
            launch.session_id.clone(),
            "--name".to_string(),
            launch.name.clone(),
            "--no-skills".to_string(),
            "--no-extensions".to_string(),
            "--tools".to_string(),
            launch.tools.join(","),
            "--no-approve".to_string(),
        ];
        for skill in &launch.skills {
            args.push("--skill".to_string());
            args.push(match skill {
                pihub_contracts::SkillRef::Grant { path, .. } => path.to_string_lossy().into_owned(),
                pihub_contracts::SkillRef::Profile { path, .. } => {
                    path.to_string_lossy().into_owned()
                }
            });
        }
        for extension in &launch.extensions {
            args.push("--extension".to_string());
            args.push(extension.path.to_string_lossy().into_owned());
        }
        if let Some(provider) = &launch.provider {
            args.push("--provider".to_string());
            args.push(provider.clone());
        }
        if let Some(model) = &launch.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        if let Some(thinking) = launch.thinking {
            args.push("--thinking".to_string());
            args.push(thinking.as_str().to_string());
        }
        // Append-only prompt injection; the carrier keeps its own constraints.
        args.extend(crate::prompt_args::prompt_launch_args(
            Carrier::Pi,
            launch.prompt.as_ref(),
        ));
        args
    }

    fn spawn_env(&self) -> Vec<(&'static str, String)> {
        let launch = &self.spec.launch;
        vec![
            (
                "PI_CODING_AGENT_DIR",
                launch.agent_dir.to_string_lossy().into_owned(),
            ),
            (
                "PI_CODING_AGENT_SESSION_DIR",
                launch.session_dir.to_string_lossy().into_owned(),
            ),
            ("PIHUB_AGENT_ID", launch.agent_id.clone()),
            (
                "PIHUB_AGENT_STATE_DIR",
                launch.state_dir.to_string_lossy().into_owned(),
            ),
            ("TMPDIR", launch.tmp_dir.to_string_lossy().into_owned()),
            ("PI_TELEMETRY", "0".to_string()),
            ("PI_SKIP_VERSION_CHECK", "1".to_string()),
        ]
    }


    /// Normal logging keeps a structural summary only; raw frames require the
    /// explicit debug opt-in and stay bounded.
    fn log_protocol(&self, direction: &str, value: &Value) {
        crate::protocol_log::write(
            &self.log,
            &self.spec.launch.agent_id,
            Carrier::Pi,
            "pi-rpc",
            direction,
            value,
            self.spec.protocol_debug,
        );
    }
}

#[async_trait::async_trait]
impl Runtime for PiRpcProcess {
    fn pid(&self) -> Option<u32> {
        let pid = self.pid.load(Ordering::SeqCst);
        (pid != 0).then_some(pid)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && self.pid.load(Ordering::SeqCst) != 0
    }

    async fn start(&self) -> HubResult<RuntimeState> {
        {
            let mut inner = self.inner.lock().await;
            if inner.started {
                return Err(HubError::new(
                    "pi_already_started",
                    "Pi process already started",
                ));
            }
            inner.started = true;
        }

        let args = self.args();

        let mut command = Command::new(&self.spec.program);
        command.arg(&self.spec.cli_path);
        command.args(&args);
        command.current_dir(&self.spec.launch.cwd);
        command.env_clear();
        for (key, value) in self.spec.env.iter() {
            command.env(key, value);
        }
        for (key, value) in self.spawn_env() {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let mut inner = self.inner.lock().await;
                inner.started = false;
                return Err(HubError::new(
                    "pi_spawn_failed",
                    format!("cannot start Pi RPC process: {error}"),
                ));
            }
        };

        let pid = child.id().unwrap_or(0);
        self.pid.store(pid, Ordering::SeqCst);
        self.alive.store(true, Ordering::SeqCst);

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        {
            let mut inner = self.inner.lock().await;
            inner.stdin = stdin;
        }
        if let Some(stdout) = stdout {
            tokio::spawn(read_loop(
                stdout,
                Arc::clone(&self.inner),
                self.events.clone(),
                self.log.clone(),
                self.spec.launch.agent_id.clone(),
                self.spec.protocol_debug,
            ));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(stderr_loop(
                stderr,
                self.log.clone(),
                self.spec.launch.agent_id.clone(),
            ));
        }
        tokio::spawn(monitor(
            child,
            Arc::clone(&self.inner),
            self.events.clone(),
            Arc::clone(&self.alive),
            Arc::clone(&self.pid),
            self.log.clone(),
            self.spec.launch.agent_id.clone(),
        ));

        let response = self
            .request(RuntimeRequestKind::GetState, Value::Object(Map::new()))
            .await?;
        Ok(parse_state(response.get("data")))
    }

    async fn request(&self, kind: RuntimeRequestKind, payload: Value) -> HubResult<Value> {
        if !self.is_alive() {
            return Err(HubError::new("pi_not_running", "Pi process is not running"));
        }
        let mut command = Map::new();
        command.insert("id".to_string(), Value::String(pihub_contracts::new_id()));
        command.insert("type".to_string(), Value::String(kind.as_str().to_string()));
        if let Value::Object(extra) = payload {
            for (key, value) in extra {
                command.insert(key, value);
            }
        }
        let command = Value::Object(command);
        self.log_protocol("in", &command);
        let id = command
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut text = serde_json::to_string(&command)?;
        text.push('\n');

        let (sender, receiver) = oneshot::channel();
        {
            let mut inner = self.inner.lock().await;
            if inner.stdin.is_none() {
                return Err(HubError::new("pi_not_running", "Pi process is not running"));
            }
            inner.pending.insert(id.clone(), sender);
            let write_result = match inner.stdin.as_mut() {
                Some(stdin) => {
                    let mut outcome = stdin.write_all(text.as_bytes()).await;
                    if outcome.is_ok() {
                        outcome = stdin.flush().await;
                    }
                    outcome
                }
                None => {
                    inner.pending.remove(&id);
                    return Err(HubError::new("pi_not_running", "Pi process is not running"));
                }
            };
            if let Err(error) = write_result {
                inner.pending.remove(&id);
                return Err(HubError::new("pi_rpc_write_failed", error.to_string()));
            }
        }

        match tokio::time::timeout(request_timeout(kind), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(HubError::new(
                "pi_rpc_cancelled",
                format!("Pi RPC channel closed for {}", kind.as_str()),
            )),
            Err(_) => {
                self.inner.lock().await.pending.remove(&id);
                Err(HubError::new(
                    "pi_rpc_timeout",
                    format!("Pi RPC timeout for {}", kind.as_str()),
                ))
            }
        }
    }

    async fn stop(&self) -> HubResult<()> {
        if !self.is_alive() {
            return Ok(());
        }
        // Graceful first: drain the queue, then abort the active turn.
        let _ = self
            .request(RuntimeRequestKind::ClearQueue, Value::Object(Map::new()))
            .await;
        let _ = self
            .request(RuntimeRequestKind::Abort, Value::Object(Map::new()))
            .await;
        if !self.is_alive() {
            return Ok(());
        }
        let pid = self.pid.load(Ordering::SeqCst);
        let alive = Arc::clone(&self.alive);
        proc::terminate(pid, false, move || alive.load(Ordering::SeqCst)).await;
        Ok(())
    }
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    inner: Arc<Mutex<Inner>>,
    events: mpsc::Sender<RuntimeEvent>,
    log: Option<Arc<dyn LogSink>>,
    agent_id: String,
    protocol_debug: bool,
) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    let mut jsonl = JsonlReader::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        for record in jsonl.push(&buffer) {
            if !handle_record(record, &inner, &events, &log, &agent_id, protocol_debug).await {
                return;
            }
        }
    }
    if let Some(record) = jsonl.finish() {
        handle_record(record, &inner, &events, &log, &agent_id, protocol_debug).await;
    }
}

/// Returns false when the consumer is gone and the loop should stop.
async fn handle_record(
    record: JsonlRecord,
    inner: &Arc<Mutex<Inner>>,
    events: &mpsc::Sender<RuntimeEvent>,
    log: &Option<Arc<dyn LogSink>>,
    agent_id: &str,
    protocol_debug: bool,
) -> bool {
    match record {
        JsonlRecord::Error { message, line } => {
            record_protocol_error(log, agent_id, &message, &line);
            events
                .send(RuntimeEvent::ProtocolError {
                    message,
                    line: Some(line),
                })
                .await
                .is_ok()
        }
        JsonlRecord::Value(value) => {
            crate::protocol_log::write(
                log,
                agent_id,
                Carrier::Pi,
                "pi-rpc",
                "out",
                &value,
                protocol_debug,
            );
            if value.get("type").and_then(Value::as_str) == Some("response") {
                if let Some(id) = value.get("id").and_then(Value::as_str) {
                    let removed = inner.lock().await.pending.remove(id);
                    if let Some(sender) = removed {
                        let success = value
                            .get("success")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let result = if success {
                            Ok(value)
                        } else {
                            Err(HubError::new("pi_rpc_command_failed", failure_message(&value)))
                        };
                        let _ = sender.send(result);
                        return true;
                    }
                }
            }
            let event_type = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("pi_event")
                .to_string();
            events
                .send(RuntimeEvent::Event {
                    event_type,
                    payload: value,
                })
                .await
                .is_ok()
        }
    }
}

fn record_protocol_error(
    log: &Option<Arc<dyn LogSink>>,
    agent_id: &str,
    message: &str,
    line: &str,
) {
    if let Some(log) = log {
        log.record(LogRecord {
            level: Some(pihub_contracts::LogLevel::Error),
            kind: Some("protocol".to_string()),
            agent_id: Some(agent_id.to_string()),
            carrier: Some(Carrier::Pi),
            source: Some("pi-rpc".to_string()),
            direction: Some("error".to_string()),
            message: Some(message.to_string()),
            data: Some(Value::String(line.to_string())),
            ..LogRecord::default()
        });
    }
}

/// 2.1 rejects a failed response with `value.error || "Pi RPC command failed: <command>"`.
fn failure_message(value: &Value) -> String {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return error.to_string();
    }
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!("Pi RPC command failed: {command}")
}

async fn stderr_loop(
    stderr: tokio::process::ChildStderr,
    log: Option<Arc<dyn LogSink>>,
    agent_id: String,
) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if let Some(log) = &log {
            log.record(LogRecord {
                level: Some(pihub_contracts::LogLevel::Debug),
                kind: Some("stderr".to_string()),
                agent_id: Some(agent_id.clone()),
                carrier: Some(Carrier::Pi),
                source: Some("pi-rpc".to_string()),
                message: Some(crate::protocol_log::bounded_text(
                    String::from_utf8_lossy(&buffer).trim_end(),
                    crate::protocol_log::RAW_MAX_CHARS,
                    crate::protocol_log::RAW_MAX_BYTES,
                )),
                ..LogRecord::default()
            });
        }
    }
}

async fn monitor(
    mut child: Child,
    inner: Arc<Mutex<Inner>>,
    events: mpsc::Sender<RuntimeEvent>,
    alive: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
    log: Option<Arc<dyn LogSink>>,
    agent_id: String,
) {
    let status = child.wait().await;
    alive.store(false, Ordering::SeqCst);
    pid.store(0, Ordering::SeqCst);
    let (code, signal) = match status {
        Ok(status) => {
            let (code, signal) = proc::decode_exit(status);
            (code, signal)
        }
        Err(_) => (None, None),
    };

    let drained = {
        let mut guard = inner.lock().await;
        guard.stdin = None;
        std::mem::take(&mut guard.pending)
    };
    let message = format!(
        "Pi process exited before response (code={}, signal={})",
        proc::js_optional(code),
        proc::js_optional_str(signal.as_deref())
    );
    for (_, sender) in drained {
        let _ = sender.send(Err(HubError::new(
            "pi_process_exited",
            message.clone(),
        )));
    }

    if let Some(log) = &log {
        log.record(LogRecord {
            kind: Some("process_exit".to_string()),
            agent_id: Some(agent_id),
            carrier: Some(Carrier::Pi),
            source: Some("pi-rpc".to_string()),
            summary: Some(serde_json::json!({"code": code, "signal": signal})),
            ..LogRecord::default()
        });
    }
    let _ = events
        .send(RuntimeEvent::Exit {
            code,
            signal,
            at: now_iso(),
        })
        .await;
}

fn parse_state(data: Option<&Value>) -> RuntimeState {
    let Some(data) = data else {
        return RuntimeState::default();
    };
    RuntimeState {
        session_id: data
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        session_file: data
            .get("sessionFile")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: data.get("model").and_then(|model| {
            let provider = model.get("provider")?.as_str()?;
            let id = model.get("id")?.as_str()?;
            Some(ModelRef {
                provider: provider.to_string(),
                id: id.to_string(),
            })
        }),
        thinking: data
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok()),
        is_streaming: data
            .get("isStreaming")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::{failure_message, parse_state, request_timeout};
    use pihub_contracts::RuntimeRequestKind;
    use serde_json::json;

    #[test]
    fn request_timeouts_match_the_2_1_call_sites() {
        assert_eq!(request_timeout(RuntimeRequestKind::GetState).as_secs(), 15);
        assert_eq!(request_timeout(RuntimeRequestKind::ClearQueue).as_secs(), 2);
        assert_eq!(request_timeout(RuntimeRequestKind::Abort).as_secs(), 5);
        assert_eq!(request_timeout(RuntimeRequestKind::Prompt).as_secs(), 30);
        assert_eq!(request_timeout(RuntimeRequestKind::Steer).as_secs(), 30);
    }

    #[test]
    fn state_is_parsed_from_the_get_state_payload() {
        let state = parse_state(Some(&json!({
            "sessionId": "fake-session",
            "sessionFile": "/tmp/s.json",
            "model": {"provider": "fake", "id": "fake-model"},
            "thinkingLevel": "off",
            "isStreaming": false
        })));
        assert_eq!(state.session_id.as_deref(), Some("fake-session"));
        assert_eq!(state.session_file.as_deref(), Some("/tmp/s.json"));
        let model = state.model.expect("model");
        assert_eq!(model.provider, "fake");
        assert_eq!(model.id, "fake-model");
        assert!(!state.is_streaming);

        let empty = parse_state(None);
        assert!(empty.session_id.is_none());
        assert!(empty.model.is_none());

        // `thinkingLevel` outside the known set is dropped, not faked.
        let unknown = parse_state(Some(&json!({"thinkingLevel": "turbo"})));
        assert!(unknown.thinking.is_none());
    }

    #[test]
    fn failed_responses_use_the_2_1_message_shape() {
        assert_eq!(
            failure_message(&json!({"error": "boom", "command": "prompt"})),
            "boom"
        );
        assert_eq!(
            failure_message(&json!({"command": "prompt"})),
            "Pi RPC command failed: prompt"
        );
        assert_eq!(
            failure_message(&json!({})),
            "Pi RPC command failed: "
        );
    }
}
