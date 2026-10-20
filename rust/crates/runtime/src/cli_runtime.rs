//! Codex / Claude CLI carriers: one child process per turn, resuming a private
//! session so the platform keeps running between turns (`src/carriers/cli-runtime.js`).
//!
//! Each turn is a single process; the runtime itself stays alive so the manager
//! can resume the same private vendor session without exposing vendor state.
//! Terminal verdicts, error wording and argv order are kept identical to 2.1.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pihub_contracts::{
    Carrier, HubError, HubResult, LogRecord, LogSink, ModelRef, Runtime, RuntimeEvent,
    RuntimeLaunch, RuntimeRequestKind, RuntimeState, ThinkingLevel, now_iso,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::env::RuntimeEnv;
use crate::fsx;
use crate::jsonl::{JsonlReader, JsonlRecord};
use crate::mount::{mount_skills, prepare_private_hub_cli};
use crate::proc::{self, SIGTERM};

/// dontAsk denies any tool that would prompt; approve the normal coding tools
/// explicitly so a headless Claude turn can edit and run checks.
pub const CLAUDE_ALLOWED_TOOLS: &str =
    "Read,Edit,Write,Glob,Grep,Bash,NotebookEdit,WebFetch,WebSearch,Skill";

/// Environment keys a CC Switch turn may inherit from the daemon.
const SWITCH_ENV_WHITELIST: [&str; 10] = [
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "NODE_EXTRA_CA_CERTS",
];

/// Private CC Switch route for one Agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcSwitchRoute {
    pub executable: PathBuf,
    pub config_dir: PathBuf,
    pub home: PathBuf,
    pub pi_dir: PathBuf,
    pub codex_cli_path: PathBuf,
    pub claude_cli_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CliSpec {
    pub launch: RuntimeLaunch,
    /// Configured Codex/Claude executable (absolute path or PATH name).
    pub executable: PathBuf,
    /// Daemon environment snapshot; only whitelisted keys reach the child.
    pub env: RuntimeEnv,
    pub hub_home: Option<PathBuf>,
    /// Absolute path of the platform `pihub` entry point.
    pub hub_cli: Option<PathBuf>,
    /// Directory of the Node runtime, so JS CLI shims can find their shebang.
    pub node_dir: Option<PathBuf>,
    pub cc_switch: Option<CcSwitchRoute>,
    /// `PIHUB_LOG_DEBUG=1` opt-in for bounded raw protocol frames.
    pub protocol_debug: bool,
}

struct Prepared {
    private_home: PathBuf,
    session_path: PathBuf,
    hub_bin: Option<PathBuf>,
    skill_mount_root: Option<PathBuf>,
    env: RuntimeEnv,
}

struct Turn {
    pid: u32,
    aborted: AtomicBool,
    saw_final: AtomicBool,
    error: Mutex<Option<String>>,
    launch_error: Mutex<Option<String>>,
}

impl Turn {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            aborted: AtomicBool::new(false),
            saw_final: AtomicBool::new(false),
            error: Mutex::new(None),
            launch_error: Mutex::new(None),
        }
    }

    async fn record_error(&self, message: impl Into<String>) {
        let mut guard = self.error.lock().await;
        if guard.is_none() {
            *guard = Some(message.into());
        }
    }
}

pub struct CliRuntime {
    spec: Arc<CliSpec>,
    carrier: Carrier,
    events: mpsc::Sender<RuntimeEvent>,
    log: Option<Arc<dyn LogSink>>,
    executable: Mutex<PathBuf>,
    started: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    turn: Arc<Mutex<Option<Arc<Turn>>>>,
    session: Arc<Mutex<Option<String>>>,
    prepared: Mutex<Option<Prepared>>,
}

impl CliRuntime {
    pub fn new(
        carrier: Carrier,
        spec: CliSpec,
        events: mpsc::Sender<RuntimeEvent>,
        log: Option<Arc<dyn LogSink>>,
    ) -> HubResult<Self> {
        if !matches!(carrier, Carrier::Codex | Carrier::Claude) {
            return Err(HubError::new(
                "invalid_carrier",
                format!("unsupported CLI carrier: {carrier}"),
            ));
        }
        Ok(Self {
            spec: Arc::new(spec),
            carrier,
            events,
            log,
            executable: Mutex::new(PathBuf::new()),
            started: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            turn: Arc::new(Mutex::new(None)),
            session: Arc::new(Mutex::new(None)),
            prepared: Mutex::new(None),
        })
    }

    fn launch(&self) -> &RuntimeLaunch {
        &self.spec.launch
    }

    fn default_provider(&self) -> &'static str {
        match self.carrier {
            Carrier::Codex => "openai",
            _ => "anthropic",
        }
    }

    fn uses_process_group(&self) -> bool {
        self.spec.cc_switch.is_some()
    }


    /// Normal logging keeps a structural summary only; raw frames require the
    /// explicit debug opt-in and stay bounded.
    fn log_protocol(&self, direction: &str, value: &Value) {
        crate::protocol_log::write(
            &self.log,
            &self.launch().agent_id,
            self.carrier,
            "cli-runtime",
            direction,
            value,
            self.spec.protocol_debug,
        );
    }

    async fn emit(&self, event: RuntimeEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    async fn emit_event(&self, payload: Value) {
        let event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("carrier_event")
            .to_string();
        self.emit(RuntimeEvent::Event {
            event_type,
            payload,
        })
        .await;
    }

    fn prepared_error(&self, message: impl Into<String>) -> HubError {
        HubError::new("cli_carrier_setup_failed", message)
    }

    /// Create the private HOME, the platform `pihub` route and the frozen skill
    /// mounts. All three are fixed for the lifetime of the Agent.
    async fn prepare(&self) -> HubResult<Prepared> {
        let launch = self.launch();
        let private_home = launch.state_dir.join(format!("{}-home", self.carrier));
        fsx::ensure_private_dir(&private_home)
            .map_err(|error| self.prepared_error(format!("cannot prepare private home: {error}")))?;
        let hub_bin = prepare_private_hub_cli(
            &private_home,
            self.spec.hub_home.as_deref(),
            self.spec.hub_cli.as_deref(),
        )?;
        fsx::ensure_private_dir(&launch.tmp_dir)
            .map_err(|error| self.prepared_error(format!("cannot prepare tmp dir: {error}")))?;

        let mut skill_mount_root = None;
        if self.carrier == Carrier::Codex {
            fsx::ensure_private_dir(&private_home.join(".codex"))
                .map_err(|error| self.prepared_error(format!("cannot prepare .codex: {error}")))?;
            let workspace_note = private_home.join("AGENTS.md");
            let note = format!(
                "# Hub workspace\n\nThe user-selected project directory is {}. Read its instructions and do project work there.\n",
                serde_json::to_string(&launch.cwd.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "\"\"".to_string())
            );
            match std::fs::read_to_string(&workspace_note) {
                Ok(existing) => {
                    if existing != note {
                        return Err(self.prepared_error(
                            "Codex private workspace note differs from the frozen project path",
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fsx::write_text_atomic(&workspace_note, &note, 0o600).map_err(|error| {
                        self.prepared_error(format!("cannot write workspace note: {error}"))
                    })?;
                }
                Err(error) => {
                    return Err(self.prepared_error(format!(
                        "cannot read workspace note: {error}"
                    )));
                }
            }
            mount_skills(&launch.skills, &private_home.join(".agents").join("skills"))?;
        } else {
            fsx::ensure_private_dir(&private_home.join(".claude"))
                .map_err(|error| self.prepared_error(format!("cannot prepare .claude: {error}")))?;
            // In bare mode Claude only discovers `<added-dir>/.claude/skills/*`, so
            // the mount root stays separate from the private credential home.
            let root = launch.state_dir.join("claude-skill-mount");
            mount_skills(&launch.skills, &root.join(".claude").join("skills"))?;
            skill_mount_root = Some(root);
        }

        if let Some(route) = &self.spec.cc_switch {
            for path in [
                launch.state_dir.clone(),
                private_home.join(".codex"),
                private_home.join(".claude"),
                route.config_dir.clone(),
                route.home.clone(),
                route.pi_dir.clone(),
            ] {
                fsx::ensure_private_dir(&path).map_err(|error| {
                    self.prepared_error(format!("cannot prepare {}: {error}", path.display()))
                })?;
            }
        }

        let env = self.child_env(&private_home, hub_bin.as_deref());
        Ok(Prepared {
            session_path: launch
                .state_dir
                .join(format!("{}-session.json", self.carrier)),
            private_home,
            hub_bin,
            skill_mount_root,
            env,
        })
    }

    /// 2.1 `#env`: a private HOME plus (for CC Switch turns) a whitelisted,
    /// PATH-controlled environment that never inherits a host API key.
    fn child_env(&self, private_home: &Path, hub_bin: Option<&Path>) -> RuntimeEnv {
        let launch = self.launch();
        let base = &self.spec.env;
        let mut env = match &self.spec.cc_switch {
            Some(route) => {
                let mut env = base.pick(&SWITCH_ENV_WHITELIST);
                let mut entries: Vec<PathBuf> = Vec::new();
                let mut push = |path: Option<PathBuf>| {
                    if let Some(path) = path {
                        if !entries.contains(&path) {
                            entries.push(path);
                        }
                    }
                };
                push(hub_bin.map(Path::to_path_buf));
                push(route.codex_cli_path.parent().map(Path::to_path_buf));
                push(route.claude_cli_path.parent().map(Path::to_path_buf));
                push(self.spec.node_dir.clone());
                for fixed in ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
                    push(Some(PathBuf::from(fixed)));
                }
                env.insert("PATH", join_path_entries(&entries));
                env.insert(
                    "CC_SWITCH_CONFIG_DIR",
                    route.config_dir.to_string_lossy().into_owned(),
                );
                env.insert("CC_SWITCH_TEST_HOME", route.home.to_string_lossy().into_owned());
                env.insert("PI_CODING_AGENT_DIR", route.pi_dir.to_string_lossy().into_owned());
                env.insert(
                    "XDG_CONFIG_HOME",
                    private_home.join(".config").to_string_lossy().into_owned(),
                );
                env.insert(
                    "XDG_DATA_HOME",
                    private_home.join(".local/share").to_string_lossy().into_owned(),
                );
                env.insert(
                    "XDG_CACHE_HOME",
                    private_home.join(".cache").to_string_lossy().into_owned(),
                );
                env.insert(
                    "XDG_STATE_HOME",
                    private_home.join(".local/state").to_string_lossy().into_owned(),
                );
                env
            }
            None => base.clone(),
        };

        env.insert("HOME", private_home.to_string_lossy().into_owned());
        if let Some(hub_home) = &self.spec.hub_home {
            env.insert("PIHUB_HOME", hub_home.to_string_lossy().into_owned());
            if self.spec.cc_switch.is_none() {
                let mut entries: Vec<PathBuf> = Vec::new();
                if let Some(bin) = hub_bin {
                    entries.push(bin.to_path_buf());
                }
                if let Some(node_dir) = &self.spec.node_dir {
                    entries.push(node_dir.clone());
                }
                match base.get_str("PATH") {
                    Some(existing) => entries.extend(std::env::split_paths(&existing)),
                    None => entries.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]),
                }
                env.insert("PATH", join_path_entries(&entries));
            }
        }
        env.insert(
            "TMPDIR",
            launch.tmp_dir.to_string_lossy().into_owned(),
        );
        env.insert("PIHUB_AGENT_ID", launch.agent_id.clone());
        env.insert(
            "CODEX_HOME",
            private_home.join(".codex").to_string_lossy().into_owned(),
        );
        env.insert(
            "CLAUDE_CONFIG_DIR",
            private_home.join(".claude").to_string_lossy().into_owned(),
        );
        env
    }

    fn validate_thinking(&self) -> HubResult<()> {
        let Some(thinking) = self.launch().thinking else {
            return Ok(());
        };
        let valid = match self.carrier {
            Carrier::Codex => matches!(
                thinking,
                ThinkingLevel::Minimal
                    | ThinkingLevel::Low
                    | ThinkingLevel::Medium
                    | ThinkingLevel::High
                    | ThinkingLevel::Xhigh
                    | ThinkingLevel::Max
            ),
            _ => matches!(
                thinking,
                ThinkingLevel::Low
                    | ThinkingLevel::Medium
                    | ThinkingLevel::High
                    | ThinkingLevel::Xhigh
                    | ThinkingLevel::Max
            ),
        };
        if !valid {
            return Err(HubError::new(
                "invalid_thinking",
                format!(
                    "{} carrier does not support thinking level {}",
                    self.carrier,
                    thinking.as_str()
                ),
            ));
        }
        Ok(())
    }

    async fn current_session(&self) -> Option<String> {
        self.session.lock().await.clone()
    }

    async fn state(&self) -> RuntimeState {
        let launch = self.launch();
        let session = self.session.lock().await.clone();
        let prepared = self.prepared.lock().await;
        let is_streaming = self.turn.lock().await.is_some();
        RuntimeState {
            session_id: session.or_else(|| Some(launch.session_id.clone())),
            session_file: prepared
                .as_ref()
                .map(|value| value.session_path.to_string_lossy().into_owned()),
            model: Some(ModelRef {
                provider: launch
                    .provider
                    .clone()
                    .unwrap_or_else(|| self.default_provider().to_string()),
                id: launch.model.clone().unwrap_or_else(|| "unknown".to_string()),
            }),
            thinking: launch.thinking,
            is_streaming,
        }
    }

    fn args(&self, prepared: &Prepared, session: Option<&str>) -> Vec<String> {
        let launch = self.launch();
        let mut args: Vec<String> = Vec::new();
        if self.carrier == Carrier::Codex {
            args.extend(
                [
                    "--sandbox",
                    "workspace-write",
                    "--ask-for-approval",
                    "never",
                    "--add-dir",
                ]
                .iter()
                .map(|value| value.to_string()),
            );
            args.push(launch.cwd.to_string_lossy().into_owned());
            args.push("exec".to_string());
            if session.is_some() {
                args.push("resume".to_string());
            }
            args.push("--json".to_string());
            if self.spec.cc_switch.is_none() {
                args.push("--ignore-user-config".to_string());
            }
            args.push("--skip-git-repo-check".to_string());
            if let Some(model) = &launch.model {
                args.push("--model".to_string());
                args.push(model.clone());
            }
            if let Some(thinking) = launch.thinking {
                args.push("-c".to_string());
                args.push(format!(
                    "model_reasoning_effort=\"{}\"",
                    thinking.as_str()
                ));
            }
            args.extend(crate::prompt_args::prompt_launch_args(
                self.carrier,
                launch.prompt.as_ref(),
            ));
            if let Some(session) = session {
                args.push(session.to_string());
            }
            args.push("-".to_string());
            match &self.spec.cc_switch {
                Some(_) => {
                    let mut wrapped = vec![
                        "start".to_string(),
                        "codex".to_string(),
                        launch.provider.clone().unwrap_or_default(),
                        "--shared-sessions".to_string(),
                        "--".to_string(),
                    ];
                    wrapped.extend(args);
                    wrapped
                }
                None => args,
            }
        } else {
            args.extend(["--bare", "--permission-mode", "dontAsk", "--allowedTools"].iter().map(|value| value.to_string()));
            args.push(CLAUDE_ALLOWED_TOOLS.to_string());
            if !launch.skills.is_empty() {
                if let Some(root) = &prepared.skill_mount_root {
                    args.push("--add-dir".to_string());
                    args.push(root.to_string_lossy().into_owned());
                }
            }
            if let Some(model) = &launch.model {
                args.push("--model".to_string());
                args.push(model.clone());
            }
            if let Some(thinking) = launch.thinking {
                args.push("--effort".to_string());
                args.push(thinking.as_str().to_string());
            }
            args.extend(crate::prompt_args::prompt_launch_args(
                self.carrier,
                launch.prompt.as_ref(),
            ));
            match session {
                Some(session) => {
                    args.push("--resume".to_string());
                    args.push(session.to_string());
                }
                None => {
                    args.push("--session-id".to_string());
                    args.push(launch.session_id.clone());
                }
            }
            args.extend(
                ["--output-format", "stream-json", "--verbose", "-p"]
                    .iter()
                    .map(|value| value.to_string()),
            );
            match &self.spec.cc_switch {
                Some(_) => {
                    let mut wrapped = vec![
                        "start".to_string(),
                        "claude".to_string(),
                        launch.provider.clone().unwrap_or_default(),
                        "--".to_string(),
                    ];
                    wrapped.extend(args);
                    wrapped
                }
                None => args,
            }
        }
    }

    async fn prompt(&self, message: &str) -> HubResult<Value> {
        if message.is_empty() {
            return Err(HubError::new("invalid_message", "message must be non-empty"));
        }
        if self.turn.lock().await.is_some() {
            return Err(HubError::new(
                "carrier_busy",
                format!("{} carrier is already processing a turn", self.carrier),
            ));
        }
        let launch = self.launch().clone();
        let prepared = {
            let guard = self.prepared.lock().await;
            guard
                .as_ref()
                .map(|value| Prepared {
                    private_home: value.private_home.clone(),
                    session_path: value.session_path.clone(),
                    hub_bin: value.hub_bin.clone(),
                    skill_mount_root: value.skill_mount_root.clone(),
                    env: value.env.clone(),
                })
                .ok_or_else(|| {
                    HubError::new(
                        "cli_carrier_not_running",
                        format!("{} runtime is not running", self.carrier),
                    )
                })?
        };
        let session = self.current_session().await;
        let args = self.args(&prepared, session.as_deref());
        let response_id = pihub_contracts::new_id();
        self.log_protocol(
            "in",
            &json!({"id": response_id, "type": "prompt", "message": message}),
        );

        let program = match &self.spec.cc_switch {
            Some(route) => route.executable.clone(),
            None => self.executable.lock().await.clone(),
        };
        let mut command = Command::new(&program);
        command.args(&args);
        command.current_dir(if self.carrier == Carrier::Codex {
            prepared.private_home.clone()
        } else {
            launch.cwd.clone()
        });
        command.env_clear();
        for (key, value) in prepared.env.iter() {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        if self.uses_process_group() {
            command.process_group(0);
        }

        // Admit exactly one turn at a time: the slot is taken before spawning so
        // two concurrent prompts cannot both pass the check above.
        let mut slot = self.turn.lock().await;
        if slot.is_some() {
            return Err(HubError::new(
                "carrier_busy",
                format!("{} carrier is already processing a turn", self.carrier),
            ));
        }
        let mut child = command.spawn().map_err(|error| {
            HubError::new(
                "cli_spawn_failed",
                format!("cannot start {} turn: {error}", self.carrier),
            )
        })?;
        let pid = child.id().unwrap_or(0);
        let turn = Arc::new(Turn::new(pid));
        *slot = Some(Arc::clone(&turn));
        drop(slot);

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if let Some(mut stdin) = stdin {
            let payload = message.to_string();
            tokio::spawn(async move {
                let _ = stdin.write_all(payload.as_bytes()).await;
                let _ = stdin.flush().await;
            });
        }
        let handle = self.clone_handle();
        tokio::spawn(async move {
            handle.run_turn(child, turn, stdout, stderr, prepared).await;
        });

        self.emit_event(json!({
            "type": "agent_start",
            "carrier": self.carrier.as_str(),
            "at": now_iso(),
        }))
        .await;

        Ok(json!({
            "id": response_id,
            "type": "response",
            "command": "prompt",
            "success": true,
            "data": {"accepted": true},
        }))
    }

    fn clone_handle(&self) -> CliHandle {
        CliHandle {
            carrier: self.carrier,
            events: self.events.clone(),
            log: self.log.clone(),
            turn: Arc::clone(&self.turn),
            started: Arc::clone(&self.started),
            stopping: Arc::clone(&self.stopping),
            session: Arc::clone(&self.session),
            agent_id: self.launch().agent_id.clone(),
            protocol_debug: self.spec.protocol_debug,
        }
    }

    async fn emit_exit(&self, code: Option<i32>, signal: Option<String>) {
        if !self.started.swap(false, Ordering::SeqCst) {
            return;
        }
        self.stopping.store(false, Ordering::SeqCst);
        self.emit(RuntimeEvent::Exit {
            code,
            signal,
            at: now_iso(),
        })
        .await;
    }
}

/// State the detached turn task needs; keeps the task independent of `&self`.
struct CliHandle {
    carrier: Carrier,
    events: mpsc::Sender<RuntimeEvent>,
    log: Option<Arc<dyn LogSink>>,
    turn: Arc<Mutex<Option<Arc<Turn>>>>,
    started: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    session: Arc<Mutex<Option<String>>>,
    agent_id: String,
    protocol_debug: bool,
}

impl CliHandle {
    async fn emit(&self, event: RuntimeEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    async fn emit_event(&self, payload: Value) {
        let event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("carrier_event")
            .to_string();
        self.emit(RuntimeEvent::Event {
            event_type,
            payload,
        })
        .await;
    }

    /// The requested-stop path: one exit signal, no extra verdict event.
    async fn emit_exit(&self, code: Option<i32>, signal: Option<String>) {
        if !self.started.swap(false, Ordering::SeqCst) {
            return;
        }
        self.stopping.store(false, Ordering::SeqCst);
        self.emit(RuntimeEvent::Exit {
            code,
            signal,
            at: now_iso(),
        })
        .await;
    }

    fn log_protocol(&self, direction: &str, value: &Value) {
        crate::protocol_log::write(
            &self.log,
            &self.agent_id,
            self.carrier,
            "cli-runtime",
            direction,
            value,
            self.protocol_debug,
        );
    }

    async fn set_session(&self, prepared: &Prepared, session_id: &str) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        if let Some(existing) = guard.as_ref() {
            if existing != session_id {
                return Err(format!(
                    "{} session id changed during a turn",
                    self.carrier
                ));
            }
        }
        *guard = Some(session_id.to_string());
        fsx::write_json_atomic(
            &prepared.session_path,
            &json!({"carrier": self.carrier.as_str(), "sessionId": session_id}),
        )
        .map_err(|error| format!("cannot persist session: {error}"))
    }

    async fn handle_output(&self, value: Value, turn: &Turn, prepared: &Prepared) {
        self.log_protocol("out", &value);
        self.emit_event(json!({
            "type": "carrier_event",
            "carrier": self.carrier.as_str(),
            "value": value,
        }))
        .await;

        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if self.carrier == Carrier::Codex {
            if event_type == "thread.started" {
                if let Some(thread_id) = value.get("thread_id").and_then(Value::as_str) {
                    if let Err(message) = self.set_session(prepared, thread_id).await {
                        turn.record_error(message).await;
                    }
                }
            }
            if event_type == "item.completed" {
                let item = value.get("item");
                let is_message = item
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("agent_message");
                if is_message {
                    if let Some(text) = agent_message_text(item) {
                        self.emit_event(message_end(&text, self.carrier)).await;
                    }
                }
            }
            if event_type == "turn.completed" {
                turn.saw_final.store(true, Ordering::SeqCst);
            }
            if event_type == "turn.failed" {
                let message = error_message(value.get("error"));
                turn.record_error(message).await;
            }
        } else {
            let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or_default();
            if event_type == "system" && subtype == "init" {
                if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                    if let Err(message) = self.set_session(prepared, session_id).await {
                        turn.record_error(message).await;
                    }
                }
            }
            if event_type == "result" {
                turn.saw_final.store(true, Ordering::SeqCst);
                if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                    if let Err(message) = self.set_session(prepared, session_id).await {
                        turn.record_error(message).await;
                    }
                }
                if let Some(text) = value.get("result").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.emit_event(message_end(text, self.carrier)).await;
                    }
                }
                let is_error = value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    let message = match value.get("result") {
                        Some(Value::String(text)) if !text.is_empty() => text.clone(),
                        _ => error_message(value.get("error")),
                    };
                    turn.record_error(message).await;
                }
            }
        }
    }

    async fn run_turn(
        &self,
        mut child: Child,
        turn: Arc<Turn>,
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
        prepared: Prepared,
    ) {
        if let Some(stderr) = stderr {
            let log = self.log.clone();
            let agent_id = self.agent_id.clone();
            let carrier = self.carrier;
            tokio::spawn(async move {
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
                            carrier: Some(carrier),
                            source: Some("cli-runtime".to_string()),
                            message: Some(crate::protocol_log::bounded_text(
                                String::from_utf8_lossy(&buffer).trim_end(),
                                crate::protocol_log::RAW_MAX_CHARS,
                                crate::protocol_log::RAW_MAX_BYTES,
                            )),
                            ..LogRecord::default()
                        });
                    }
                }
            });
        }

        if let Some(stdout) = stdout {
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
                    self.dispatch(record, &turn, &prepared).await;
                }
            }
            if let Some(record) = jsonl.finish() {
                self.dispatch(record, &turn, &prepared).await;
            }
        }

        let status = child.wait().await;
        let (code, signal) = match status {
            Ok(status) => proc::decode_exit(status),
            Err(_) => (None, None),
        };
        {
            let mut guard = self.turn.lock().await;
            guard.take();
        }
        self.conclude(code, signal, &turn).await;
    }

    async fn dispatch(&self, record: JsonlRecord, turn: &Turn, prepared: &Prepared) {
        match record {
            JsonlRecord::Error { message, line } => {
                turn.record_error(format!(
                    "invalid {} JSONL: {message}",
                    self.carrier
                ))
                .await;
                self.log_protocol("error", &json!({"type": "protocol_error", "line": line}));
                self.emit(RuntimeEvent::ProtocolError {
                    message,
                    line: Some(line),
                })
                .await;
            }
            JsonlRecord::Value(value) => self.handle_output(value, turn, prepared).await,
        }
    }

    async fn conclude(&self, code: Option<i32>, signal: Option<String>, turn: &Turn) {
        let stopping = self.stopping.load(Ordering::SeqCst);
        let aborted = turn.aborted.load(Ordering::SeqCst);
        let saw_final = turn.saw_final.load(Ordering::SeqCst);
        let error = turn.error.lock().await.clone();
        let launch_error = turn.launch_error.lock().await.clone();

        if stopping {
            // A requested stop owns the exit signal, whatever the turn did.
            self.emit_exit(code, signal).await;
            return;
        }
        if aborted {
            self.emit_event(json!({
                "type": "agent_settled",
                "carrier": self.carrier.as_str(),
                "aborted": true,
                "at": now_iso(),
            }))
            .await;
            return;
        }
        if code == Some(0) && saw_final && error.is_none() && launch_error.is_none() {
            self.emit_event(json!({
                "type": "agent_settled",
                "carrier": self.carrier.as_str(),
                "at": now_iso(),
            }))
            .await;
            return;
        }
        let message = launch_error.or(error).unwrap_or_else(|| {
            format!(
                "{} turn failed or returned no final result (code={}, signal={})",
                self.carrier,
                proc::js_optional(code),
                proc::js_optional_str(signal.as_deref())
            )
        });
        if let Some(log) = &self.log {
            log.record(LogRecord {
                level: Some(pihub_contracts::LogLevel::Error),
                kind: Some("process_error".to_string()),
                agent_id: Some(self.agent_id.clone()),
                carrier: Some(self.carrier),
                source: Some("cli-runtime".to_string()),
                message: Some(message.clone()),
                ..LogRecord::default()
            });
        }
        self.emit(RuntimeEvent::ProcessError { message }).await;
        // The turn is gone but the runtime itself stops: report the exit too.
        self.started.store(false, Ordering::SeqCst);
        self.emit(RuntimeEvent::Exit {
            code,
            signal,
            at: now_iso(),
        })
        .await;
    }
}

#[async_trait::async_trait]
impl Runtime for CliRuntime {
    fn pid(&self) -> Option<u32> {
        // `try_lock` keeps `pid()` synchronous; a held lock only happens
        // mid-transition, where reporting no pid is the safe answer.
        match self.turn.try_lock() {
            Ok(guard) => guard.as_ref().map(|turn| turn.pid),
            Err(_) => None,
        }
    }

    fn is_alive(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    async fn start(&self) -> HubResult<RuntimeState> {
        if self.started.load(Ordering::SeqCst) {
            return Err(HubError::new(
                "carrier_already_started",
                format!("{} runtime already started", self.carrier),
            ));
        }
        let resolved = proc::find_executable(&self.spec.executable, &self.spec.env.path_map())
            .ok_or_else(|| {
                HubError::new(
                    "carrier_unavailable",
                    format!(
                        "{} CLI executable is unavailable: {}",
                        self.carrier,
                        self.spec.executable.display()
                    ),
                )
            })?;
        *self.executable.lock().await = resolved;

        match &self.spec.cc_switch {
            Some(_) => {
                if self.launch().provider.is_none() {
                    return Err(HubError::new(
                        "provider_required",
                        format!("{} requires a CC Switch provider selection", self.carrier),
                    ));
                }
            }
            None => {
                if let Some(provider) = &self.launch().provider {
                    if provider != self.default_provider() {
                        return Err(HubError::new(
                            "invalid_provider",
                            format!(
                                "{} carrier does not support provider {provider}",
                                self.carrier
                            ),
                        ));
                    }
                }
            }
        }
        self.validate_thinking()?;
        if self.spec.cc_switch.is_some()
            && self.carrier == Carrier::Codex
            && self.launch().thinking.is_some()
        {
            return Err(HubError::new(
                "invalid_thinking",
                "CC Switch Codex shared sessions configure reasoning effort in the provider profile; per-agent thinking is unavailable",
            ));
        }

        let prepared = self.prepare().await?;
        // A persisted session from an earlier turn is resumed verbatim.
        let stored = fsx::read_json(&prepared.session_path)
            .ok()
            .flatten()
            .and_then(|value| {
                value
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        *self.session.lock().await = stored;
        *self.prepared.lock().await = Some(Prepared {
            private_home: prepared.private_home.clone(),
            session_path: prepared.session_path.clone(),
            hub_bin: prepared.hub_bin.clone(),
            skill_mount_root: prepared.skill_mount_root.clone(),
            env: prepared.env.clone(),
        });
        self.started.store(true, Ordering::SeqCst);
        self.stopping.store(false, Ordering::SeqCst);
        Ok(self.state().await)
    }

    async fn request(&self, kind: RuntimeRequestKind, payload: Value) -> HubResult<Value> {
        if !self.is_alive() {
            return Err(HubError::new(
                "cli_carrier_not_running",
                format!("{} runtime is not running", self.carrier),
            ));
        }
        match kind {
            RuntimeRequestKind::GetState => {
                let state = self.state().await;
                Ok(json!({
                    "id": pihub_contracts::new_id(),
                    "type": "response",
                    "command": "get_state",
                    "success": true,
                    "data": {
                        "sessionId": state.session_id,
                        "sessionFile": state.session_file,
                        "model": state.model.map(|model| json!({"provider": model.provider, "id": model.id})),
                        "thinkingLevel": state.thinking.map(|level| level.as_str()),
                        "isStreaming": state.is_streaming,
                    },
                }))
            }
            RuntimeRequestKind::Prompt => {
                let message = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.prompt(&message).await
            }
            RuntimeRequestKind::ClearQueue => Ok(json!({
                "id": pihub_contracts::new_id(),
                "type": "response",
                "command": "clear_queue",
                "success": true,
                "data": {"cleared": 0},
            })),
            RuntimeRequestKind::Abort => {
                let turn = self.turn.lock().await.clone();
                if let Some(turn) = turn.as_ref() {
                    turn.aborted.store(true, Ordering::SeqCst);
                    let _ = proc::signal_process(
                        turn.pid,
                        SIGTERM,
                        self.uses_process_group(),
                    );
                }
                Ok(json!({
                    "id": pihub_contracts::new_id(),
                    "type": "response",
                    "command": "abort",
                    "success": true,
                    "data": {"aborted": turn.is_some()},
                }))
            }
            other => Err(HubError::new(
                "unsupported_request",
                format!(
                    "{} CLI carrier does not support {}; use prompt after the current turn settles",
                    self.carrier,
                    other.as_str()
                ),
            )),
        }
    }

    async fn stop(&self) -> HubResult<()> {
        if !self.started.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.stopping.store(true, Ordering::SeqCst);
        let turn = self.turn.lock().await.clone();
        let Some(turn) = turn else {
            self.emit_exit(None, None).await;
            return Ok(());
        };
        let _ = proc::signal_process(turn.pid, SIGTERM, self.uses_process_group());
        for _ in 0..proc::TERMINATE_POLLS {
            if self.turn.lock().await.is_none() {
                break;
            }
            tokio::time::sleep(proc::TERMINATE_POLL).await;
        }
        if self.turn.lock().await.is_some() {
            let _ = proc::signal_process(turn.pid, proc::SIGKILL, self.uses_process_group());
        }
        Ok(())
    }
}

fn join_path_entries(entries: &[PathBuf]) -> String {
    match std::env::join_paths(entries.iter().map(|path| path.as_os_str())) {
        Ok(value) => value.to_string_lossy().into_owned(),
        Err(_) => entries
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":"),
    }
}

fn agent_message_text(item: Option<&Value>) -> Option<String> {
    let item = item?;
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let content = item.get("content")?.as_array()?;
    let joined = content
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!joined.is_empty()).then_some(joined)
}

fn message_end(text: &str, carrier: Carrier) -> Value {
    json!({
        "type": "message_end",
        "carrier": carrier.as_str(),
        "at": now_iso(),
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
        },
    })
}

/// 2.1 `errorMessage`: string as-is, `{message}` when present, else JSON.
fn error_message(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(map)) => match map.get("message") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => Value::Object(map.clone()).to_string(),
        },
        Some(other) => other.to_string(),
        None => "unknown error".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{error_message, join_path_entries, message_end, CcSwitchRoute, CliRuntime, CliSpec};
    use pihub_contracts::{Carrier, RuntimeLaunch};
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn carrier_messages_are_normalized_for_the_hub_feed() {
        let payload = message_end("hello", Carrier::Claude);
        assert_eq!(payload["type"], "message_end");
        assert_eq!(payload["carrier"], "claude");
        assert_eq!(payload["message"]["role"], "assistant");
        assert_eq!(payload["message"]["content"][0]["type"], "text");
        assert_eq!(payload["message"]["content"][0]["text"], "hello");
    }

    #[test]
    fn error_values_are_rendered_like_the_2_1_helper() {
        assert_eq!(error_message(Some(&json!("boom"))), "boom");
        assert_eq!(error_message(Some(&json!({"message": "nested"}))), "nested");
        assert_eq!(error_message(Some(&json!({"message": 7}))), "7");
        assert_eq!(error_message(Some(&json!({"code": 1}))), r#"{"code":1}"#);
        assert_eq!(error_message(None), "unknown error");
    }

    #[test]
    fn path_entries_are_joined_and_deduplicated_by_the_caller() {
        let joined = join_path_entries(&[
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ]);
        let parts: Vec<&str> = joined.split(':').collect();
        assert_eq!(parts, vec!["/usr/bin", "/bin"]);
    }

    #[test]
    fn cli_runtime_rejects_the_pi_carrier() {
        let launch = RuntimeLaunch {
            agent_id: "agent-test".to_string(),
            name: "test".to_string(),
            cwd: PathBuf::from("/tmp"),
            workspace_mode: "shared".to_string(),
            carrier: Carrier::Pi,
            provider: None,
            model: None,
            thinking: None,
            session_id: "s".to_string(),
            session_dir: PathBuf::from("/tmp/sessions"),
            agent_dir: PathBuf::from("/tmp/agent"),
            logs_dir: PathBuf::from("/tmp/logs"),
            state_dir: PathBuf::from("/tmp/state"),
            tmp_dir: PathBuf::from("/tmp/tmp"),
            tools: Vec::new(),
            skills: Vec::new(),
            extensions: Vec::new(),
            authorization: None,
            prompt: None,
        };
        let (events, _receiver) = tokio::sync::mpsc::channel(4);
        let spec = CliSpec {
            launch,
            executable: PathBuf::from("codex"),
            env: crate::env::RuntimeEnv::default(),
            hub_home: None,
            hub_cli: None,
            node_dir: None,
            protocol_debug: false,
            cc_switch: Some(CcSwitchRoute {
                executable: PathBuf::from("/bin/sh"),
                config_dir: PathBuf::from("/tmp/cfg"),
                home: PathBuf::from("/tmp/home"),
                pi_dir: PathBuf::from("/tmp/pi"),
                codex_cli_path: PathBuf::from("/bin/sh"),
                claude_cli_path: PathBuf::from("/bin/sh"),
            }),
        };
        assert!(CliRuntime::new(Carrier::Pi, spec, events, None).is_err());
    }
}
