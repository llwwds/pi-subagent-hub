//! CC Switch provider service.
//!
//! The upstream CC Switch CLI is an external, *app-private* binary. Everything
//! it reads or writes is redirected into the Hub home: a private
//! `CC_SWITCH_CONFIG_DIR`, a private `HOME`, private carrier config dirs and a
//! private temp dir. The service wraps it in three ways:
//!
//! 1. **Path safety** — the whole private tree is screened for symlinks before
//!    and after every command, and tightened back to 0700/0600.
//! 2. **Credential containment** — the child gets a fixed, whitelisted
//!    environment; a host `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` is never
//!    inherited, and no key ever reaches stdout/stderr handling or an error.
//! 3. **Protocol fidelity** — the only stdout the service parses is the
//!    documented `--version` banner (`cc-switch <x.y.z>`, bounded to 1024
//!    bytes). `provider add/switch` are treated as commands whose success is
//!    the exit status plus the re-read private state, exactly like 2.1.
//!
//! Provider rows are read straight from the private SQLite database in
//! read-only mode; writes only ever go through the CLI.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use pihub_contracts::{
    AuthMode, HubError, HubPaths, HubResult, Provider, ProviderApp, ProviderDiagnostics,
    ProviderInput, ProviderService, ProviderSnapshot,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Only this CLI build is verified against the wrapper.
pub const CC_SWITCH_VERSION: &str = "5.10.5";
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
pub const LOCK_POLL: Duration = Duration::from_millis(75);
/// 2.1 caps the captured `--version` output at 1024 bytes.
pub const OUTPUT_LIMIT: usize = 1024;
const TREE_SCAN_LIMIT: usize = 20_000;
const MAX_ID_LEN: usize = 64;
const MAX_TEXT_LEN: usize = 256;
const MAX_KEY_LEN: usize = 8192;

fn fail(code: &str, message: impl Into<String>) -> HubError {
    HubError::new(code, message)
}

fn path_unsafe(message: impl Into<String>) -> HubError {
    fail("CC_SWITCH_PATH_UNSAFE", message)
}

fn state_invalid(message: impl Into<String>) -> HubError {
    fail("CC_SWITCH_STATE_INVALID", message)
}

fn invalid_input(message: impl Into<String>) -> HubError {
    fail("CC_SWITCH_INVALID_INPUT", message)
}

/// A held `operation.lock`; releasing it removes the file.
struct OperationLock {
    path: PathBuf,
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct CcSwitchService {
    home: PathBuf,
    root: PathBuf,
    claude_dir: PathBuf,
    codex_dir: PathBuf,
    tmp_dir: PathBuf,
    db_path: PathBuf,
    index_path: PathBuf,
    lock_path: PathBuf,
    settings_path: PathBuf,
    binary: PathBuf,
    config_dir: PathBuf,
    switch_home: PathBuf,
    pi_dir: PathBuf,
    command_timeout: Duration,
}

impl CcSwitchService {
    pub fn new(paths: &HubPaths) -> HubResult<Self> {
        if !paths.home.is_absolute() {
            return Err(path_unsafe("PIHUB_HOME must be an absolute path"));
        }
        let root = paths.home.join("cc-switch");
        let expected_binary = paths
            .home
            .join("toolchains")
            .join("cc-switch-cli")
            .join("cc-switch");
        if pihub_contracts::absolute_normalized(&paths.cc_switch_cli_path)
            != pihub_contracts::absolute_normalized(&expected_binary)
        {
            return Err(path_unsafe(
                "CC Switch binary must live in the app-private toolchain",
            ));
        }
        for path in [
            &paths.cc_switch_config_dir,
            &paths.cc_switch_home,
            &paths.cc_switch_pi_dir,
        ] {
            if !crate::fsx::is_inside(&paths.home, path) || !crate::fsx::is_inside(&root, path) {
                return Err(path_unsafe(
                    "CC Switch config directory escapes the app-private home",
                ));
            }
        }
        Ok(Self {
            home: paths.home.clone(),
            root: root.clone(),
            claude_dir: paths.cc_switch_home.join(".claude"),
            codex_dir: paths.cc_switch_home.join(".codex"),
            tmp_dir: root.join("tmp"),
            db_path: paths.cc_switch_config_dir.join("cc-switch.db"),
            index_path: root.join("models-index.json"),
            lock_path: root.join("operation.lock"),
            settings_path: paths.cc_switch_config_dir.join("settings.json"),
            binary: paths.cc_switch_cli_path.clone(),
            config_dir: paths.cc_switch_config_dir.clone(),
            switch_home: paths.cc_switch_home.clone(),
            pi_dir: paths.cc_switch_pi_dir.clone(),
            command_timeout: COMMAND_TIMEOUT,
        })
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    fn check_paths(&self) -> HubResult<()> {
        for path in [
            &self.home,
            &self.binary,
            &self.root,
            &self.config_dir,
            &self.switch_home,
            &self.pi_dir,
        ] {
            assert_no_symlink_components(path)?;
        }
        assert_no_symlinks_in_tree(&self.root)
    }

    fn prepare(&self) -> HubResult<()> {
        self.check_paths()?;
        for path in [
            &self.root,
            &self.config_dir,
            &self.switch_home,
            &self.pi_dir,
            &self.claude_dir,
            &self.codex_dir,
            &self.tmp_dir,
        ] {
            crate::fsx::ensure_private_dir(path)
                .map_err(|error| path_unsafe(format!("cannot prepare {}: {error}", path.display())))?;
        }
        self.check_paths()?;
        if self.settings_path.exists() {
            let settings = read_json_file(&self.settings_path)?;
            for key in ["claudeConfigDir", "codexConfigDir", "piConfigDir"] {
                let overridden = settings
                    .get(key)
                    .map(|value| !value.is_null() && value.as_str() != Some(""))
                    .unwrap_or(false);
                if overridden {
                    return Err(path_unsafe(format!(
                        "CC Switch {key} override would break per-agent private directories"
                    )));
                }
            }
        }
        self.check_paths()?;
        tighten_private_tree(&self.root)
    }

    /// The complete child environment: no inherited credentials, fixed PATH.
    fn cli_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.switch_home.to_string_lossy().into_owned()),
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".to_string()),
            ("LANG", "C.UTF-8".to_string()),
            ("TMPDIR", self.tmp_dir.to_string_lossy().into_owned()),
            (
                "XDG_CONFIG_HOME",
                self.switch_home.join(".config").to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME",
                self.switch_home
                    .join(".local/share")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "CC_SWITCH_CONFIG_DIR",
                self.config_dir.to_string_lossy().into_owned(),
            ),
            (
                "CC_SWITCH_TEST_HOME",
                self.switch_home.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR",
                self.claude_dir.to_string_lossy().into_owned(),
            ),
            (
                "CODEX_HOME",
                self.codex_dir.to_string_lossy().into_owned(),
            ),
            (
                "PI_CODING_AGENT_DIR",
                self.pi_dir.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// Run the private CLI. `read_output` captures (bounded) stdout; stderr is
    /// always discarded so a key echoed by a third-party binary cannot leak.
    async fn run(&self, args: &[String], read_output: bool) -> HubResult<String> {
        self.prepare()?;
        let metadata = std::fs::metadata(&self.binary)
            .map_err(|_| fail("CC_SWITCH_MISSING", "app-private CC Switch CLI is not installed"))?;
        if !metadata.is_file() {
            return Err(fail(
                "CC_SWITCH_MISSING",
                "app-private CC Switch CLI is not installed",
            ));
        }

        let mut command = Command::new(&self.binary);
        command.args(args);
        command.current_dir(&self.switch_home);
        command.env_clear();
        for (key, value) in self.cli_env() {
            command.env(key, value);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|_| fail("CC_SWITCH_EXEC_FAILED", "cannot execute app-private CC Switch CLI"))?;

        let capture = read_output;
        let stdout = child.stdout.take();
        let reader = tokio::spawn(async move {
            let mut collected = Vec::new();
            if let Some(mut stdout) = stdout {
                // Bounded read: never buffer an unbounded vendor stream.
                let mut buffer = [0u8; 4096];
                loop {
                    match stdout.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if collected.len() < OUTPUT_LIMIT * 4 {
                                collected.extend_from_slice(&buffer[..read]);
                            }
                        }
                    }
                }
            }
            if capture { collected } else { Vec::new() }
        });

        let status = match tokio::time::timeout(self.command_timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(_)) => {
                let _ = child.start_kill();
                let _ = reader.await;
                return Err(fail(
                    "CC_SWITCH_EXEC_FAILED",
                    "cannot execute app-private CC Switch CLI",
                ));
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = reader.await;
                // Tidy up even on failure, but never mask the timeout.
                let _ = tighten_private_tree(&self.root);
                return Err(fail("CC_SWITCH_TIMEOUT", "CC Switch CLI timed out"));
            }
        };

        let output = reader.await.unwrap_or_default();
        // Every command re-tightens the private tree, even after a failure.
        tighten_private_tree(&self.root)
            .map_err(|_| path_unsafe("CC Switch private files failed the path/permission check"))?;

        let (code, signal) = crate::proc::decode_exit(status);
        if signal.as_deref() == Some("SIGKILL") {
            return Err(fail("CC_SWITCH_TIMEOUT", "CC Switch CLI timed out"));
        }
        if code != Some(0) {
            return Err(fail(
                "CC_SWITCH_COMMAND_FAILED",
                "CC Switch CLI operation failed; check the private configuration",
            ));
        }
        let text = String::from_utf8_lossy(&output).trim().to_string();
        Ok(truncate_chars(&text, OUTPUT_LIMIT))
    }

    async fn version(&self) -> HubResult<String> {
        let output = self.run(&["--version".to_string()], true).await?;
        let parsed = parse_version(&output).ok_or_else(|| {
            fail(
                "CC_SWITCH_VERSION_UNSUPPORTED",
                format!("app-private CC Switch CLI {CC_SWITCH_VERSION} is required"),
            )
        })?;
        if parsed != CC_SWITCH_VERSION {
            return Err(fail(
                "CC_SWITCH_VERSION_UNSUPPORTED",
                format!("app-private CC Switch CLI {CC_SWITCH_VERSION} is required"),
            ));
        }
        Ok(parsed)
    }

    async fn acquire_lock(&self) -> HubResult<OperationLock> {
        self.prepare()?;
        let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match open_lock_file(&self.lock_path) {
                Ok(()) => return Ok(OperationLock { path: self.lock_path.clone() }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(fail(
                            "CC_SWITCH_BUSY",
                            "another CC Switch configuration operation is running",
                        ));
                    }
                    tokio::time::sleep(LOCK_POLL).await;
                }
                Err(error) => {
                    return Err(fail("CC_SWITCH_EXEC_FAILED", error.to_string()));
                }
            }
        }
    }

    fn read_rows(&self, app: ProviderApp) -> HubResult<Vec<ProviderRow>> {
        self.check_paths()?;
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let connection = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| state_invalid("cannot read the private CC Switch provider state"))?;
        let mut statement = connection
            .prepare(
                "SELECT id, name, settings_config, is_current FROM providers \
                 WHERE app_type = ?1 ORDER BY sort_index, created_at, id",
            )
            .map_err(|_| state_invalid("cannot read the private CC Switch provider state"))?;
        let rows = statement
            .query_map([app.as_str()], |row| {
                Ok(ProviderRow {
                    id: row.get::<_, String>(0)?,
                    name: row.get::<_, String>(1)?,
                    settings_config: row.get::<_, String>(2)?,
                    is_current: row.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
                })
            })
            .map_err(|_| state_invalid("cannot read the private CC Switch provider state"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| state_invalid("cannot read the private CC Switch provider state"))?;
        Ok(rows)
    }

    fn pi_default(&self) -> HubResult<(Option<String>, Option<String>)> {
        let path = self.pi_dir.join("settings.json");
        if !path.exists() {
            return Ok((None, None));
        }
        self.check_paths()?;
        let settings = read_json_file(&path)?;
        Ok((
            settings
                .get("defaultProvider")
                .and_then(Value::as_str)
                .map(str::to_owned),
            settings
                .get("defaultModel")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ))
    }

    fn read_index(&self) -> HubResult<Value> {
        if !self.index_path.exists() {
            return Ok(json!({"version": 1, "providers": {"claude": {}, "codex": {}, "pi": {}}}));
        }
        let index = read_json_file(&self.index_path)?;
        let valid = index.get("version").and_then(Value::as_u64) == Some(1)
            && index.get("providers").map(Value::is_object).unwrap_or(false);
        if !valid {
            return Err(state_invalid("CC Switch model index is malformed"));
        }
        Ok(index)
    }

    fn list_sync(&self, app: ProviderApp) -> HubResult<Vec<Provider>> {
        let index = self.read_index()?;
        let (pi_provider, pi_model) = if app == ProviderApp::Pi {
            self.pi_default()?
        } else {
            (None, None)
        };
        let rows = self.read_rows(app)?;
        let mut providers = Vec::with_capacity(rows.len());
        for row in rows {
            providers.push(build_provider(
                app,
                &row,
                &index,
                pi_provider.as_deref(),
                pi_model.as_deref(),
            )?);
        }
        Ok(providers)
    }

    async fn require_provider(&self, app: ProviderApp, id: &str) -> HubResult<Provider> {
        validate_id(id)?;
        match self.get(app, id).await? {
            Some(provider) => Ok(provider),
            None => Err(fail(
                "CC_SWITCH_PROVIDER_NOT_FOUND",
                "the requested provider does not exist",
            )),
        }
    }
}

struct ProviderRow {
    id: String,
    name: String,
    settings_config: String,
    is_current: bool,
}

#[async_trait::async_trait]
impl ProviderService for CcSwitchService {
    async fn diagnose(&self) -> ProviderDiagnostics {
        let mut diagnostics = ProviderDiagnostics {
            available: false,
            version: None,
            binary_path: self.binary.clone(),
            config_dir: self.config_dir.clone(),
            error_code: None,
            reason: None,
        };
        match self.version().await {
            Ok(version) => {
                diagnostics.available = true;
                diagnostics.version = Some(version);
            }
            Err(error) => {
                diagnostics.error_code = Some(error.code.clone());
                diagnostics.reason = Some(error.message.clone());
            }
        }
        diagnostics
    }

    async fn list(&self, app: ProviderApp) -> HubResult<Vec<Provider>> {
        self.version().await?;
        self.list_sync(app)
    }

    async fn list_all(&self) -> HubResult<ProviderSnapshot> {
        let switch = self.diagnose().await;
        if !switch.available {
            let code = switch
                .error_code
                .clone()
                .unwrap_or_else(|| "CC_SWITCH_MISSING".to_string());
            return Err(fail(
                &code,
                switch
                    .reason
                    .clone()
                    .unwrap_or_else(|| "CC Switch is unavailable".to_string()),
            ));
        }
        let claude = self.list(ProviderApp::Claude).await?;
        let codex = self.list(ProviderApp::Codex).await?;
        let pi = self.list(ProviderApp::Pi).await?;
        Ok(ProviderSnapshot {
            switch,
            apps: pihub_contracts::ProviderApps { claude, codex, pi },
        })
    }

    async fn get(&self, app: ProviderApp, id: &str) -> HubResult<Option<Provider>> {
        validate_id(id)?;
        Ok(self
            .list(app)
            .await?
            .into_iter()
            .find(|provider| provider.id == id))
    }

    async fn add(&self, input: ProviderInput) -> HubResult<Provider> {
        let app = input.app;
        validate_id(&input.id)?;
        let name = non_empty(&input.name, "provider name", MAX_TEXT_LEN)?;
        let base_url = validate_base_url(&input.base_url)?;
        let api_key = non_empty(&input.api_key, "API key", MAX_KEY_LEN)?;
        let (model_list, default_model) = validate_models(&input.model_list, &input.default_model)?;
        let config = config_for(
            app,
            &name,
            &base_url,
            &api_key,
            &model_list,
            &default_model,
            input.api_format.as_deref(),
            input.auth_mode,
        )?;

        let _lock = self.acquire_lock().await?;
        self.version().await?;
        if self.read_rows(app)?.iter().any(|row| row.id == input.id) {
            return Err(fail(
                "CC_SWITCH_PROVIDER_EXISTS",
                "the provider id already exists",
            ));
        }
        let directory = self
            .tmp_dir
            .join(format!("add-{}", pihub_contracts::new_id()));
        crate::fsx::ensure_private_dir(&directory)
            .map_err(|error| path_unsafe(format!("cannot create staging dir: {error}")))?;
        let config_path = directory.join("settings.json");
        let result = self
            .add_in_staging(app, &input, &config, &config_path)
            .await;
        let _ = std::fs::remove_dir_all(&directory);
        result
    }

    async fn switch(&self, app: ProviderApp, id: &str) -> HubResult<Provider> {
        validate_id(id)?;
        if app == ProviderApp::Pi {
            return Err(fail(
                "CC_SWITCH_UNSUPPORTED",
                "Pi manages its default model in settings.json; CC Switch only enables model entries",
            ));
        }
        let _lock = self.acquire_lock().await?;
        self.version().await?;
        let selected = self.require_provider(app, id).await?;
        if !selected.configured || !selected.ready {
            return Err(fail(
                "CC_SWITCH_PROVIDER_NOT_READY",
                "this provider is missing an API URL, credential or model and cannot be selected",
            ));
        }
        self.run(
            &[
                "--app".to_string(),
                app.as_str().to_string(),
                "provider".to_string(),
                "switch".to_string(),
                id.to_string(),
            ],
            false,
        )
        .await?;
        let provider = self.require_provider(app, id).await?;
        if !provider.is_current.unwrap_or(false) {
            return Err(state_invalid(
                "CC Switch did not record the provider switch",
            ));
        }
        Ok(provider)
    }
}

impl CcSwitchService {
    async fn add_in_staging(
        &self,
        app: ProviderApp,
        input: &ProviderInput,
        config: &Value,
        config_path: &Path,
    ) -> HubResult<Provider> {
        crate::fsx::write_json_atomic(config_path, config)
            .map_err(|error| path_unsafe(format!("cannot stage the provider config: {error}")))?;

        let mut args = vec![
            "--app".to_string(),
            app.as_str().to_string(),
            "provider".to_string(),
            "add".to_string(),
            "--id".to_string(),
            input.id.clone(),
            "--name".to_string(),
            non_empty(&input.name, "provider name", MAX_TEXT_LEN)?,
            "--config-file".to_string(),
            config_path.to_string_lossy().into_owned(),
        ];
        if let Some(api_format) = &input.api_format {
            if app != ProviderApp::Pi {
                args.push("--api-format".to_string());
                args.push(api_format.clone());
            }
        }
        self.run(&args, false).await?;

        if !self.read_rows(app)?.iter().any(|row| row.id == input.id) {
            return Err(state_invalid("CC Switch did not persist the provider"));
        }

        let mut index = self.read_index()?;
        let (model_list, default_model) = validate_models(&input.model_list, &input.default_model)?;
        let mut entry = Map::new();
        entry.insert("modelList".to_string(), json!(model_list));
        entry.insert("defaultModel".to_string(), json!(default_model));
        entry.insert(
            "apiFormat".to_string(),
            json!(input
                .api_format
                .clone()
                .unwrap_or_else(|| default_api_format(app).to_string())),
        );
        if app == ProviderApp::Claude {
            entry.insert(
                "authMode".to_string(),
                json!(auth_mode_str(auth_mode(input.auth_mode))),
            );
        }
        let providers = index
            .get_mut("providers")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| state_invalid("CC Switch model index is malformed"))?;
        let app_entry = providers
            .entry(app.as_str().to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !app_entry.is_object() {
            *app_entry = Value::Object(Map::new());
        }
        app_entry
            .as_object_mut()
            .expect("object")
            .insert(input.id.clone(), Value::Object(entry));

        if let Err(error) = crate::fsx::write_json_atomic(&self.index_path, &index) {
            let mut partial = fail(
                "CC_SWITCH_PARTIAL_ADD",
                "the provider was added but the model index could not be written",
            );
            partial.details = Some(json!({"providerAdded": true, "io": error.to_string()}));
            return Err(partial);
        }

        self.require_provider(app, &input.id).await
    }
}

fn open_lock_file(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
    Ok(())
}

fn read_json_file(path: &Path) -> HubResult<Value> {
    let bytes = std::fs::read(path).map_err(|_| state_invalid("cannot read CC Switch private state"))?;
    serde_json::from_slice(&bytes).map_err(|_| state_invalid("CC Switch private state is malformed"))
}

fn assert_no_symlink_components(path: &Path) -> HubResult<()> {
    let absolute = pihub_contracts::absolute_normalized(path);
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(path_unsafe(
                    "a CC Switch private path contains a symbolic link",
                ))
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    Ok(())
}

fn assert_no_symlinks_in_tree(root: &Path) -> HubResult<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut scanned = 0usize;
    while let Some(path) = pending.pop() {
        assert_no_symlink_components(&path)?;
        scanned += 1;
        if scanned > TREE_SCAN_LIMIT {
            return Err(path_unsafe(
                "CC Switch private directory has too many entries to check safely",
            ));
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry
                .file_type()
                .map(|kind| kind.is_symlink())
                .unwrap_or(false)
            {
                return Err(path_unsafe(
                    "a CC Switch private directory contains a symbolic link",
                ));
            }
            pending.push(entry_path);
        }
    }
    Ok(())
}

fn tighten_private_tree(root: &Path) -> HubResult<()> {
    assert_no_symlinks_in_tree(root)?;
    if !root.exists() {
        return Ok(());
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let is_dir = metadata.is_dir();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if is_dir { 0o700 } else { 0o600 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .map_err(|_| path_unsafe("cannot tighten CC Switch private permissions"))?;
        }
        if is_dir {
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    pending.push(entry.path());
                }
            }
        }
    }
    Ok(())
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

/// `cc-switch <x.y.z>` on its own line, as emitted by the pinned build.
pub fn parse_version(output: &str) -> Option<String> {
    for line in output.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("cc-switch") else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let candidate = rest.trim();
        let parts: Vec<&str> = candidate.split('.').collect();
        let valid = parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
        if valid {
            return Some(candidate.to_string());
        }
    }
    None
}

fn non_empty(value: &str, label: &str, max_length: usize) -> HubResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || value.chars().count() > max_length
        || value.chars().any(|character| character.is_control())
    {
        return Err(invalid_input(format!("{label} is not valid")));
    }
    Ok(trimmed.to_string())
}

fn validate_id(id: &str) -> HubResult<()> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .next()
            .map(|byte| byte.is_ascii_alphanumeric())
            .unwrap_or(false)
        && id[1..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(invalid_input("provider id is not valid"))
    }
}

struct UrlParts<'a> {
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
    has_query: bool,
    has_fragment: bool,
}

fn split_url(raw: &str) -> Option<UrlParts<'_>> {
    let (scheme, rest) = raw.split_once("://")?;
    if scheme.is_empty() || !scheme.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')) {
        return None;
    }
    let (authority, remainder) = match rest.find(['/', '?', '#']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }
    let (without_fragment, has_fragment) = match remainder.split_once('#') {
        Some((head, _)) => (head, true),
        None => (remainder, false),
    };
    let (path, has_query) = match without_fragment.split_once('?') {
        Some((head, _)) => (head, true),
        None => (without_fragment, false),
    };
    Some(UrlParts {
        scheme,
        authority,
        path,
        has_query,
        has_fragment,
    })
}

fn validate_base_url(raw: &str) -> HubResult<String> {
    let value = non_empty(raw, "API URL", 2048)?;
    let parts = split_url(&value).ok_or_else(|| invalid_input("API URL is not valid"))?;
    let scheme = parts.scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https")
        || parts.authority.contains('@')
        || parts.has_query
        || parts.has_fragment
    {
        return Err(invalid_input(
            "API URL must be an HTTP(S) address without credentials or query parameters",
        ));
    }
    Ok(value.trim_end_matches('/').to_string())
}

/// Never echo credentials or query parameters back to a caller.
pub fn public_base_url(raw: &str) -> Option<String> {
    let parts = split_url(raw)?;
    let scheme = parts.scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let authority = match parts.authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => parts.authority,
    };
    if authority.is_empty() {
        return None;
    }
    let rendered = format!("{scheme}://{authority}{}", parts.path);
    Some(rendered.trim_end_matches('/').to_string())
}

fn validate_models(
    raw: &[String],
    default_model: &str,
) -> HubResult<(Vec<String>, String)> {
    if raw.is_empty() || raw.len() > 100 {
        return Err(invalid_input("modelList must contain 1 to 100 models"));
    }
    let mut models = Vec::with_capacity(raw.len());
    for model in raw {
        models.push(non_empty(model, "model id", 160)?);
    }
    let unique: std::collections::BTreeSet<&String> = models.iter().collect();
    if unique.len() != models.len() {
        return Err(invalid_input("modelList contains duplicate models"));
    }
    let selected = if default_model.is_empty() {
        models[0].clone()
    } else {
        non_empty(default_model, "default model", 160)?
    };
    if !models.iter().any(|model| model == &selected) {
        return Err(invalid_input("the default model is not in modelList"));
    }
    Ok((models, selected))
}

fn default_api_format(app: ProviderApp) -> &'static str {
    match app {
        ProviderApp::Claude => "anthropic",
        ProviderApp::Codex => "responses",
        ProviderApp::Pi => "openai-completions",
    }
}

fn auth_mode(mode: Option<AuthMode>) -> AuthMode {
    mode.unwrap_or(AuthMode::ApiKey)
}

fn config_for(
    app: ProviderApp,
    name: &str,
    base_url: &str,
    api_key: &str,
    model_list: &[String],
    default_model: &str,
    api_format: Option<&str>,
    auth_mode_input: Option<AuthMode>,
) -> HubResult<Value> {
    match app {
        ProviderApp::Claude => {
            if let Some(format) = api_format {
                if format != "anthropic" {
                    return Err(fail(
                        "CC_SWITCH_UNSUPPORTED",
                        "Claude currently only supports the native Anthropic API format",
                    ));
                }
            }
            let mode = auth_mode(auth_mode_input);
            let mut env = Map::new();
            env.insert("ANTHROPIC_BASE_URL".to_string(), json!(base_url));
            env.insert(
                match mode {
                    AuthMode::Bearer => "ANTHROPIC_AUTH_TOKEN",
                    AuthMode::ApiKey => "ANTHROPIC_API_KEY",
                }
                .to_string(),
                json!(api_key),
            );
            env.insert("ANTHROPIC_MODEL".to_string(), json!(default_model));
            Ok(json!({"env": Value::Object(env)}))
        }
        ProviderApp::Codex => {
            if let Some(format) = api_format {
                if !matches!(format, "responses" | "openai_responses") {
                    return Err(fail(
                        "CC_SWITCH_UNSUPPORTED",
                        "Codex currently only supports the OpenAI Responses API format",
                    ));
                }
            }
            let model = serde_json::to_string(default_model).expect("json string");
            let display = serde_json::to_string(name).expect("json string");
            let url = serde_json::to_string(base_url).expect("json string");
            let toml = [
                "model_provider = \"custom\"".to_string(),
                format!("model = {model}"),
                String::new(),
                "[model_providers.custom]".to_string(),
                format!("name = {display}"),
                format!("base_url = {url}"),
                "wire_api = \"responses\"".to_string(),
                "requires_openai_auth = true".to_string(),
                String::new(),
            ]
            .join("\n");
            Ok(json!({"auth": {"OPENAI_API_KEY": api_key}, "config": toml}))
        }
        ProviderApp::Pi => {
            let format = api_format.unwrap_or("openai-completions");
            if !matches!(
                format,
                "openai-completions" | "openai-responses" | "anthropic-messages"
            ) {
                return Err(fail(
                    "CC_SWITCH_UNSUPPORTED",
                    "Pi does not support this API format",
                ));
            }
            Ok(json!({
                "name": name,
                "baseUrl": base_url,
                "api": format,
                "apiKey": api_key,
                "models": model_list.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
            }))
        }
    }
}

fn build_provider(
    app: ProviderApp,
    row: &ProviderRow,
    index: &Value,
    pi_provider: Option<&str>,
    pi_model: Option<&str>,
) -> HubResult<Provider> {
    let config: Value = serde_json::from_str(&row.settings_config)
        .map_err(|_| state_invalid("CC Switch private state is malformed"))?;
    let saved = index
        .get("providers")
        .and_then(|providers| providers.get(app.as_str()))
        .and_then(|apps| apps.get(&row.id));

    let (base_url, mut models, mut default_model, has_credential) = match app {
        ProviderApp::Claude => {
            let env = config.get("env");
            let base_url = env
                .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let default_model = env
                .and_then(|env| env.get("ANTHROPIC_MODEL"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let has_credential = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
                .iter()
                .any(|key| {
                    env.and_then(|env| env.get(*key))
                        .map(|value| !value.is_null())
                        .unwrap_or(false)
                });
            (base_url, Vec::new(), default_model, has_credential)
        }
        ProviderApp::Codex => {
            let toml = config
                .get("config")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let base_url = toml_string_value(toml, "base_url");
            let default_model = toml_string_value(toml, "model");
            let has_credential = ["auth", "env"].iter().any(|section| {
                config
                    .get(*section)
                    .and_then(|value| value.get("OPENAI_API_KEY"))
                    .map(|value| !value.is_null())
                    .unwrap_or(false)
            });
            (base_url, Vec::new(), default_model, has_credential)
        }
        ProviderApp::Pi => {
            let base_url = config
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let models: Vec<String> = config
                .get("models")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("id").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let default_model = if pi_provider == Some(row.id.as_str()) {
                pi_model.map(str::to_owned)
            } else {
                models.first().cloned()
            };
            let has_credential = config
                .get("apiKey")
                .map(|value| !value.is_null())
                .unwrap_or(false);
            (base_url, models, default_model, has_credential)
        }
    };

    if let Some(saved) = saved {
        if let Some(list) = saved.get("modelList").and_then(Value::as_array) {
            models = list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        if let Some(model) = saved.get("defaultModel").and_then(Value::as_str) {
            default_model = Some(model.to_string());
        }
    }
    if let Some(model) = &default_model {
        if !models.iter().any(|item| item == model) {
            models.insert(0, model.clone());
        }
    }

    let safe_base_url = base_url.as_deref().and_then(public_base_url);
    let configured = safe_base_url.is_some()
        && has_credential
        && default_model.is_some()
        && !models.is_empty();
    let api_format = saved
        .and_then(|saved| saved.get("apiFormat"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            (app == ProviderApp::Pi)
                .then(|| config.get("api").and_then(Value::as_str).map(str::to_owned))
                .flatten()
        });
    let auth_mode_value = if app == ProviderApp::Claude {
        let saved_mode = saved
            .and_then(|saved| saved.get("authMode"))
            .and_then(Value::as_str);
        match saved_mode {
            Some("api-key") => Some(AuthMode::ApiKey),
            Some("bearer") => Some(AuthMode::Bearer),
            _ => {
                let bearer = config
                    .get("env")
                    .and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN"))
                    .map(|value| !value.is_null())
                    .unwrap_or(false);
                Some(if bearer { AuthMode::Bearer } else { AuthMode::ApiKey })
            }
        }
    } else {
        None
    };

    Ok(Provider {
        id: row.id.clone(),
        name: row.name.clone(),
        base_url: safe_base_url,
        model_list: models,
        default_model,
        api_format,
        auth_mode: auth_mode_value,
        configured,
        ready: configured,
        is_current: if app == ProviderApp::Pi {
            None
        } else {
            Some(row.is_current)
        },
        is_default: if app == ProviderApp::Pi {
            pi_provider == Some(row.id.as_str())
        } else {
            row.is_current
        },
    })
}

/// Extract `key = "<json string>"` from a TOML document without a parser.
fn toml_string_value(toml: &str, key: &str) -> Option<String> {
    for line in toml.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with('"') {
            continue;
        }
        let end = json_string_end(rest)?;
        return serde_json::from_str::<String>(&rest[..end]).ok();
    }
    None
}

/// Byte offset just past the closing quote of a JSON string starting at 0.
fn json_string_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut index = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

fn auth_mode_str(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::ApiKey => "api-key",
        AuthMode::Bearer => "bearer",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_version, public_base_url, toml_string_value, validate_base_url, validate_id,
        validate_models,
    };

    #[test]
    fn version_banner_is_parsed_strictly() {
        assert_eq!(parse_version("cc-switch 5.10.5\n"), Some("5.10.5".to_string()));
        assert_eq!(parse_version("noise\ncc-switch 5.10.5"), Some("5.10.5".to_string()));
        assert_eq!(parse_version("cc-switch 5.10"), None);
        assert_eq!(parse_version("cc-switchx 5.10.5"), None);
        assert_eq!(parse_version("cc-switch 5.10.5-beta"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn provider_ids_follow_the_2_1_rule() {
        assert!(validate_id("route-a").is_ok());
        assert!(validate_id("a").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("-leading").is_err());
        assert!(validate_id("has space").is_err());
        assert!(validate_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn base_urls_reject_credentials_queries_and_non_http() {
        assert_eq!(
            validate_base_url("https://example.invalid/v1/").expect("valid"),
            "https://example.invalid/v1"
        );
        assert!(validate_base_url("https://user:pw@example.invalid/v1").is_err());
        assert!(validate_base_url("https://example.invalid/v1?api_key=x").is_err());
        assert!(validate_base_url("https://example.invalid/v1#frag").is_err());
        assert!(validate_base_url("file:///etc/passwd").is_err());
        assert!(validate_base_url("not a url").is_err());
    }

    #[test]
    fn public_base_urls_never_leak_secrets() {
        assert_eq!(
            public_base_url("https://example.invalid/v1?api_key=secret-in-url").as_deref(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(
            public_base_url("https://user:secret@example.invalid/v1").as_deref(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(public_base_url("not-a-url"), None);
    }

    #[test]
    fn model_lists_are_validated_and_defaulted() {
        let (models, selected) = validate_models(
            &["model-one".to_string(), "model-two".to_string()],
            "",
        )
        .expect("valid");
        assert_eq!(models.len(), 2);
        assert_eq!(selected, "model-one");

        assert!(validate_models(&[], "").is_err());
        assert!(
            validate_models(&["a".to_string(), "a".to_string()], "").is_err(),
            "duplicates rejected"
        );
        assert!(
            validate_models(&["a".to_string()], "b").is_err(),
            "default must be listed"
        );
    }

    #[test]
    fn codex_toml_values_are_extracted_without_a_parser() {
        let toml = "model_provider = \"custom\"\nmodel = \"gpt-5\"\n\n[model_providers.custom]\nname = \"n\"\nbase_url = \"https://example.invalid/v1\"\n";
        assert_eq!(toml_string_value(toml, "model").as_deref(), Some("gpt-5"));
        assert_eq!(
            toml_string_value(toml, "base_url").as_deref(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(toml_string_value(toml, "missing"), None);
        // A value holding an escaped quote round-trips.
        assert_eq!(
            toml_string_value("base_url = \"https://a/\\\"q\\\"\"", "base_url").as_deref(),
            Some("https://a/\"q\"")
        );
    }
}
