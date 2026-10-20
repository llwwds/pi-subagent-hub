//! Offline / local operations owned by the app layer.
//!
//! `LocalOps` is the contract boundary for everything that must work without a
//! running daemon: `config` init/validate/paths, `doctor`, and daemon
//! start/stop/status. These helpers only touch the Hub's own files under
//! `PIHUB_HOME`; they never open the SQLite store.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use pihub_contracts::{
    DaemonLockRecord, DaemonMetadata, DaemonStatus, Health, HubError, HubPaths, HubResult,
    LocalOps, APP_NAME, APP_VERSION, API_VERSION,
};
use serde_json::{json, Value};

use crate::client::{daemon_not_running, HubClient};
use crate::daemon::DAEMON_SUBCOMMAND;

/// Supported `models.json` protocol identifiers (mirrors `config-files.js`).
const SUPPORTED_APIS: [&str; 4] = [
    "anthropic-messages",
    "google-generative-ai",
    "openai-completions",
    "openai-responses",
];

/// Poll budget for `daemon start`: 100 × 100 ms, the 2.1 window.
const START_ATTEMPTS: u32 = 100;
const START_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct LocalOperations {
    paths: HubPaths,
}

impl LocalOperations {
    pub fn new(paths: HubPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &HubPaths {
        &self.paths
    }
}

#[pihub_contracts::async_trait]
impl LocalOps for LocalOperations {
    async fn config_init(&self) -> HubResult<Value> {
        self.assert_state_home_outside_app()?;
        ensure_hub_layout(&self.paths)?;
        ensure_config_files(&self.paths)?;
        inspect_config(&self.paths)
    }

    async fn config_validate(&self) -> HubResult<Value> {
        inspect_config(&self.paths)
    }

    async fn config_paths(&self) -> HubResult<Value> {
        Ok(paths_value(&self.paths))
    }

    async fn doctor(&self) -> HubResult<Value> {
        self.doctor_report().await
    }

    async fn daemon_start(&self) -> HubResult<Value> {
        let status = self.daemon_status().await?;
        if status.running {
            return Ok(start_payload(false, &status));
        }
        self.assert_state_home_outside_app()?;
        if self.paths.daemon_path.exists() {
            let _ = fs::remove_file(&self.paths.daemon_path);
        }
        ensure_hub_layout(&self.paths)?;
        ensure_config_files(&self.paths)?;

        let executable = std::env::current_exe().map_err(|error| {
            HubError::new("daemon_spawn_failed", format!("cannot resolve own executable: {error}"))
        })?;
        let stdout = open_log(&self.paths.daemon_stdout_path)?;
        let stderr = open_log(&self.paths.daemon_stderr_path)?;
        let mut command = Command::new(executable);
        command
            .arg(DAEMON_SUBCOMMAND)
            .arg("--home")
            .arg(&self.paths.home)
            .env("PIHUB_HOME", &self.paths.home)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        detach(&mut command);
        let mut child = command.spawn().map_err(|error| {
            HubError::new("daemon_spawn_failed", format!("daemon spawn failed: {error}"))
        })?;

        for _ in 0..START_ATTEMPTS {
            tokio::time::sleep(START_INTERVAL).await;
            let status = self.daemon_status().await?;
            if status.running {
                return Ok(start_payload(true, &status));
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
        }
        let tail = read_tail(&self.paths.daemon_stderr_path, 20);
        Err(HubError::new(
            "daemon_start_failed",
            match tail.trim() {
                "" => "daemon failed to start".to_string(),
                tail => format!("daemon failed to start\n{tail}"),
            },
        ))
    }

    async fn daemon_stop(&self) -> HubResult<Value> {
        let status = self.daemon_status().await?;
        if !status.running {
            return Ok(json!({ "stopped": false, "reason": "not running" }));
        }
        let client = HubClient::connect(&self.paths)?;
        client.control_request("/v1/daemon/stop", Some(&json!({}))).await
    }

    async fn daemon_status(&self) -> HubResult<DaemonStatus> {
        let metadata: Option<DaemonMetadata> = read_json(&self.paths.daemon_path)?;
        let Some(metadata) = metadata else {
            return Ok(DaemonStatus {
                running: false,
                metadata: None,
                health: None,
                error: None,
            });
        };
        if metadata.pid == 0 || !is_process_alive(metadata.pid) {
            return Ok(DaemonStatus {
                running: false,
                metadata: Some(metadata),
                health: None,
                error: None,
            });
        }
        let metadata_pid = metadata.pid;
        match HubClient::connect(&self.paths) {
            Ok(client) => match client.health().await {
                Ok(value) => Ok(DaemonStatus {
                    running: true,
                    metadata: Some(metadata),
                    health: Some(Health {
                        name: value
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(APP_NAME)
                            .to_string(),
                        version: value
                            .get("version")
                            .and_then(Value::as_str)
                            .unwrap_or(APP_VERSION)
                            .to_string(),
                        api_version: value
                            .get("apiVersion")
                            .and_then(Value::as_u64)
                            .unwrap_or(API_VERSION as u64) as u32,
                        pid: value
                            .get("pid")
                            .and_then(Value::as_u64)
                            .unwrap_or(metadata_pid as u64) as u32,
                    }),
                    error: None,
                }),
                Err(error) => Ok(DaemonStatus {
                    running: false,
                    metadata: Some(metadata),
                    health: None,
                    error: Some(error.message),
                }),
            },
            Err(error) => Ok(DaemonStatus {
                running: false,
                metadata: Some(metadata),
                health: None,
                error: Some(error.message),
            }),
        }
    }
}

impl LocalOperations {
    fn assert_state_home_outside_app(&self) -> HubResult<()> {
        if self.paths.is_state_home_outside_app() {
            Ok(())
        } else {
            Err(HubError::new(
                "unsafe_home",
                format!(
                    "PIHUB_HOME must be outside the source tree: {}",
                    self.paths.app_root.display()
                ),
            ))
        }
    }

    /// The full doctor report. Kept separate from [`LocalOps::doctor`] so tests
    /// can call it without the trait object.
    pub async fn doctor_report(&self) -> HubResult<Value> {
        let mut checks: Vec<Value> = Vec::new();
        let mut push = |name: &str, ok: bool, detail: Value| {
            checks.push(json!({ "name": name, "ok": ok, "detail": detail }));
        };

        // The pinned Pi carrier ships as a Node bundle, so "is a usable Node
        // present" stays the first prerequisite check under its 2.1 name.
        match run_node_probe() {
            Ok(version) => {
                let major = version
                    .trim_start_matches('v')
                    .split('.')
                    .next()
                    .and_then(|major| major.parse::<u64>().ok())
                    .unwrap_or(0);
                push("node", major >= 22, Value::String(version));
            }
            Err(message) => push("node", false, Value::String(message)),
        }

        let lock: Option<Value> = read_json(&self.paths.pi_lock_path)?;
        let lock_ok = lock
            .as_ref()
            .and_then(|value| value.get("tag"))
            .is_some()
            && lock
                .as_ref()
                .and_then(|value| value.get("commit"))
                .is_some();
        push(
            "pi-lock",
            lock_ok,
            lock.clone().unwrap_or(Value::String("missing".to_string())),
        );

        let pi_cli = self.paths.pi_cli_path.clone();
        push(
            "pi-cli",
            pi_cli.exists(),
            Value::String(pi_cli.display().to_string()),
        );

        if pi_cli.exists() {
            match run_version_probe(&pi_cli) {
                Ok(version) => {
                    let expected = lock
                        .as_ref()
                        .and_then(|value| value.get("tag"))
                        .and_then(Value::as_str)
                        .map(|tag| tag.trim_start_matches('v').to_string());
                    let ok = expected
                        .map(|expected| version.contains(&expected))
                        .unwrap_or(false);
                    push("pi-version", ok, Value::String(version));
                }
                Err(error) => push("pi-version", false, Value::String(error)),
            }
        }

        let vendor_git = self.paths.app_root.join("vendor").join("pi").join(".git");
        let lock_commit = lock
            .as_ref()
            .and_then(|value| value.get("commit"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if vendor_git.exists()
            && let Some(expected) = lock_commit.clone()
        {
            match git_head(&self.paths.app_root.join("vendor").join("pi")) {
                Ok(head) => push("pi-commit", head == expected, Value::String(head)),
                Err(error) => push("pi-commit", false, Value::String(error)),
            }
        } else {
            push(
                "pi-commit",
                lock_commit.is_some(),
                Value::String("deployment uses lock file; source .git not present".to_string()),
            );
        }

        push(
            "deployment-boundary",
            self.paths.is_state_home_outside_app(),
            Value::String(format!(
                "app={}; state={}",
                self.paths.app_root.display(),
                self.paths.home.display()
            )),
        );

        match inspect_config(&self.paths) {
            Ok(config) => push("ai-api-config", true, config),
            Err(error) => push("ai-api-config", false, Value::String(error.message)),
        }

        let daemon = self.daemon_status().await?;
        // 2.1 reports the flat health object (`{name,version,apiVersion,pid}`)
        // when the daemon answers, and the literal `"stopped"` otherwise. The
        // nested status (`{running,metadata,...}`) is not part of that shape.
        let detail = match (daemon.running, daemon.health.as_ref()) {
            (true, Some(health)) => serde_json::to_value(health).unwrap_or(Value::Null),
            _ => Value::String("stopped".to_string()),
        };
        push("daemon", daemon.running, detail);

        let ok = checks
            .iter()
            .filter(|check| check.get("name").and_then(Value::as_str) != Some("daemon"))
            .all(|check| check.get("ok").and_then(Value::as_bool).unwrap_or(false));
        Ok(json!({ "ok": ok, "checks": checks }))
    }
}

/// `pihub config paths` payload: camelCase keys matching `src/paths.js`.
pub fn paths_value(paths: &HubPaths) -> Value {
    let as_string = |path: &Path| Value::String(path.display().to_string());
    json!({
        "appRoot": as_string(&paths.app_root),
        "home": as_string(&paths.home),
        "configDir": as_string(&paths.config_dir),
        "sharedPiDir": as_string(&paths.shared_pi_dir),
        "modelsPath": as_string(&paths.models_path),
        "authPath": as_string(&paths.auth_path),
        "profilePath": as_string(&paths.profile_path),
        "stateDir": as_string(&paths.state_dir),
        "databasePath": as_string(&paths.database_path),
        "daemonPath": as_string(&paths.daemon_path),
        "controlPath": as_string(&paths.control_path),
        "lockPath": as_string(&paths.lock_path),
        "daemonStdoutPath": as_string(&paths.daemon_stdout_path),
        "daemonStderrPath": as_string(&paths.daemon_stderr_path),
        "agentsDir": as_string(&paths.agents_dir),
        "codexCliPath": as_string(&paths.codex_cli_path),
        "claudeCliPath": as_string(&paths.claude_cli_path),
        "ccSwitchCliPath": as_string(&paths.cc_switch_cli_path),
        "ccSwitchConfigDir": as_string(&paths.cc_switch_config_dir),
        "ccSwitchHome": as_string(&paths.cc_switch_home),
        "ccSwitchPiDir": as_string(&paths.cc_switch_pi_dir),
        "piLockPath": as_string(&paths.pi_lock_path),
        "piCliPath": as_string(&paths.pi_cli_path),
    })
}

/// Create the private Hub layout (`0700`).
pub fn ensure_hub_layout(paths: &HubPaths) -> HubResult<()> {
    for directory in [
        &paths.home,
        &paths.config_dir,
        &paths.shared_pi_dir,
        &paths.state_dir,
        &paths.agents_dir,
    ] {
        create_private_dir(directory)?;
    }
    Ok(())
}

/// Create `models.json` / `auth.json` when absent and force mode `0600`.
pub fn ensure_config_files(paths: &HubPaths) -> HubResult<()> {
    ensure_hub_layout(paths)?;
    if !paths.models_path.exists() {
        atomic_write_json(&paths.models_path, &json!({ "providers": {} }))?;
    }
    if !paths.auth_path.exists() {
        atomic_write_json(&paths.auth_path, &json!({}))?;
    }
    set_private_file_mode(&paths.models_path)?;
    set_private_file_mode(&paths.auth_path)?;
    Ok(())
}

/// Validate the AI API configuration exactly like `inspectPiConfig`.
pub fn inspect_config(paths: &HubPaths) -> HubResult<Value> {
    if !paths.models_path.exists() {
        return Err(HubError::new(
            "ai_api_config_missing",
            format!(
                "missing AI API configuration: {}",
                paths.models_path.display()
            ),
        ));
    }
    if !paths.auth_path.exists() {
        return Err(HubError::new(
            "ai_api_config_missing",
            format!(
                "missing Pi authentication state: {}",
                paths.auth_path.display()
            ),
        ));
    }
    let models_raw = read_json::<Value>(&paths.models_path)?
        .ok_or_else(|| HubError::new("ai_api_config_missing", "models.json is empty"))?;
    let providers = validate_models_document(&models_raw)?;
    let auth_raw = read_json::<Value>(&paths.auth_path)?
        .unwrap_or_else(|| json!({}));
    let Some(auth) = auth_raw.as_object() else {
        return Err(HubError::new(
            "ai_api_config_invalid",
            "auth.json must be a JSON object",
        ));
    };
    let models_mode = file_mode(&paths.models_path)?;
    let auth_mode = file_mode(&paths.auth_path)?;
    if cfg!(unix) && (models_mode != "600" || auth_mode != "600") {
        return Err(HubError::new(
            "ai_api_config_insecure",
            format!(
                "AI configuration files must use mode 600 (models={models_mode}, auth={auth_mode}); run pihub config init to repair permissions"
            ),
        ));
    }
    let mut auth_providers: Vec<String> = auth.keys().cloned().collect();
    auth_providers.sort();
    Ok(json!({
        "modelsPath": paths.models_path.display().to_string(),
        "authPath": paths.auth_path.display().to_string(),
        "modelsMode": models_mode,
        "authMode": auth_mode,
        "providers": providers,
        "authProviders": auth_providers,
    }))
}

fn validate_models_document(document: &Value) -> HubResult<Vec<Value>> {
    let Some(root) = document.as_object() else {
        return Err(HubError::new(
            "ai_api_config_invalid",
            "models.json must be a JSON object",
        ));
    };
    let providers = match root.get("providers") {
        None | Some(Value::Null) => &serde_json::Map::new(),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(HubError::new(
                "ai_api_config_invalid",
                "models.json providers must be a JSON object",
            ));
        }
    };
    let mut summary = Vec::new();
    for (provider_id, provider) in providers {
        if provider_id.trim().is_empty() {
            return Err(HubError::new(
                "ai_api_config_invalid",
                "models.json provider IDs must be non-empty",
            ));
        }
        let Some(provider) = provider.as_object() else {
            return Err(HubError::new(
                "ai_api_config_invalid",
                format!("provider {provider_id} must be a JSON object"),
            ));
        };
        if let Some(api) = provider.get("api").and_then(Value::as_str)
            && !SUPPORTED_APIS.contains(&api)
        {
            return Err(HubError::new(
                "ai_api_config_invalid",
                format!("provider {provider_id} uses unsupported api: {api}"),
            ));
        }
        let models = match provider.get("models") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut ids = Vec::new();
                for (index, model) in items.iter().enumerate() {
                    let Some(model) = model.as_object() else {
                        return Err(HubError::new(
                            "ai_api_config_invalid",
                            format!("provider {provider_id} model {index} must be a JSON object"),
                        ));
                    };
                    let id = model.get("id").and_then(Value::as_str).unwrap_or("");
                    if id.trim().is_empty() {
                        return Err(HubError::new(
                            "ai_api_config_invalid",
                            format!("provider {provider_id} model {index} requires a non-empty id"),
                        ));
                    }
                    if let Some(api) = model.get("api").and_then(Value::as_str)
                        && !SUPPORTED_APIS.contains(&api)
                    {
                        return Err(HubError::new(
                            "ai_api_config_invalid",
                            format!("provider {provider_id} model {id} uses unsupported api: {api}"),
                        ));
                    }
                    ids.push(Value::String(id.to_string()));
                }
                ids
            }
            Some(_) => {
                return Err(HubError::new(
                    "ai_api_config_invalid",
                    format!("provider {provider_id} models must be an array"),
                ));
            }
        };
        summary.push(json!({
            "id": provider_id,
            "api": provider.get("api").and_then(Value::as_str),
            "hasBaseUrl": provider.get("baseUrl").and_then(Value::as_str).map(|value| !value.is_empty()).unwrap_or(false),
            "hasCredentialReference": provider.get("apiKey").and_then(Value::as_str).map(|value| !value.is_empty()).unwrap_or(false),
            "hasCustomHeaders": provider.get("headers").and_then(Value::as_object).map(|headers| !headers.is_empty()).unwrap_or(false),
            "models": models,
        }));
    }
    Ok(summary)
}

fn start_payload(started: bool, status: &DaemonStatus) -> Value {
    json!({
        "started": started,
        "running": status.running,
        "metadata": status.metadata,
        "health": status.health,
    })
}

/// `kill(pid, 0)` equivalent without a libc dependency.
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_node_probe() -> Result<String, String> {
    let output = Command::new("node")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("node unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!("node --version exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_version_probe(pi_cli: &Path) -> Result<String, String> {
    let output = Command::new("node")
        .arg(pi_cli)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("node unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!("pi --version exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_head(repo: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("HEAD")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("git unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!("git rev-parse exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // New process group so the daemon survives the spawning shell.
    command.process_group(0);
}

#[cfg(not(unix))]
fn detach(_command: &mut Command) {}

fn read_tail(path: &Path, lines: usize) -> String {
    let Ok(raw) = fs::read_to_string(path) else {
        return String::new();
    };
    let collected: Vec<&str> = raw.lines().collect();
    let start = collected.len().saturating_sub(lines);
    collected[start..].join("\n")
}

fn open_log(path: &Path) -> HubResult<fs::File> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(HubError::from)?;
    set_private_file_mode(path)?;
    Ok(file)
}

pub fn atomic_write_json(path: &Path, value: &Value) -> HubResult<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let serialized = format!("{}\n", serde_json::to_string_pretty(value)?);
    {
        let mut file = fs::File::create(&temporary).map_err(HubError::from)?;
        file.write_all(serialized.as_bytes())
            .map_err(HubError::from)?;
        file.sync_all().map_err(HubError::from)?;
    }
    set_private_file_mode(&temporary)?;
    fs::rename(&temporary, path).map_err(HubError::from)?;
    set_private_file_mode(path)?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> HubResult<Option<T>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(HubError::from(error)),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&raw)?))
}

pub fn create_private_dir(path: &Path) -> HubResult<()> {
    fs::create_dir_all(path).map_err(HubError::from)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub fn set_private_file_mode(path: &Path) -> HubResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(HubError::from)?;
    }
    Ok(())
}

fn file_mode(path: &Path) -> HubResult<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).map_err(HubError::from)?.permissions().mode() & 0o777;
        return Ok(format!("{mode:o}"));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok("600".to_string())
    }
}

/// Lock record written by the daemon on startup.
pub fn lock_record() -> DaemonLockRecord {
    DaemonLockRecord {
        pid: std::process::id(),
        started_at: pihub_contracts::now_iso(),
    }
}

pub fn daemon_metadata(host: &str, port: u16, app_root: &Path) -> DaemonMetadata {
    DaemonMetadata {
        pid: std::process::id(),
        host: host.to_string(),
        port,
        version: APP_VERSION.to_string(),
        started_at: pihub_contracts::now_iso(),
        app_root: PathBuf::from(app_root),
    }
}

/// Re-exported for the CLI so `daemon stop` reports the 2.1 message.
pub fn not_running_error() -> HubError {
    daemon_not_running()
}
