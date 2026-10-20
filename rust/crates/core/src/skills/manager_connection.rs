//! Manual, read-only connection to the real `skills-manager` desktop app
//! (`src/skills/manager-connection.js`).
//!
//! The manager stays the single owner of Skill content and metadata. The Hub
//! only records its own connection, keeps a catalog snapshot for listing and
//! drift reporting, and freezes content into its own object store at Agent
//! creation. Every CLI invocation is a read-only query.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use pihub_contracts::{
    canonical_json, is_safe_token, now_iso, sha256_prefixed, HubError, HubResult,
    ManagerConnectInput,
    ManagerConnection, ManagerDisconnectInput, ManagerSelection, ManagerSkillSource,
    ManagerSyncInput, SkillPolicy, SkillPolicyMode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::fsutil;
use crate::skills::registry::FsSkillRegistry;

const CONFIG_VERSION: u32 = 1;
const SNAPSHOT_VERSION: u32 = 1;
const MIN_SUPPORTED: [u64; 3] = [1, 28, 0];
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const STALE_AFTER_MS: i64 = 24 * 60 * 60 * 1000;
const MANAGER_DIR_NAME: &str = ".skills-manager";
const CLI_NAME: &str = "skills-manager-cli";

/// Every command PiHub may run against the manager; all are read-only queries.
pub const READ_ONLY_COMMANDS: [&str; 5] = [
    "repo status",
    "presets list",
    "skills list",
    "skills show",
    "agents list",
];

fn reject(code: &'static str, message: impl Into<String>) -> HubError {
    HubError::new(code, message)
}

fn assert_absolute(value: &Path, code: &'static str, label: &str) -> HubResult<PathBuf> {
    if !value.is_absolute()
        || value
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(reject(
            code,
            format!("{label} must be an absolute path without traversal"),
        ));
    }
    Ok(fsutil::absolute_normalized(value))
}

/// The persisted connection record (`manager-connection.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManagerConfig {
    schema_version: u32,
    connected_at: String,
    last_synced_at: String,
    cli_path: String,
    manager_base_dir: String,
    manager_skills_dir: String,
    manager_version: String,
    #[serde(default)]
    agent_key: Option<String>,
    selection: ManagerSelection,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotManager {
    cli_path: String,
    version: String,
    base_dir: String,
    skills_dir: String,
    mode: String,
    #[serde(default)]
    active_preset_id: Option<String>,
    #[serde(default)]
    presets: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotSkill {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    path: String,
    #[serde(default)]
    entry_path: Option<String>,
    enabled: bool,
    tags: Vec<String>,
    presets: Vec<String>,
    preset_ids: Vec<String>,
    deployed_to: Vec<String>,
    source_type: String,
}

impl SnapshotSkill {
    fn entry(&self) -> &str {
        self.entry_path.as_deref().unwrap_or(&self.path)
    }

    fn digest_view(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "path": self.path,
            "enabled": self.enabled,
            "tags": self.tags,
            "presets": self.presets,
            "presetIds": self.preset_ids,
            "deployedTo": self.deployed_to,
            "sourceType": self.source_type,
        })
    }

    fn api_view(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "path": self.path,
            "entryPath": self.entry_path,
            "enabled": self.enabled,
            "tags": self.tags,
            "presets": self.presets,
            "presetIds": self.preset_ids,
            "deployedTo": self.deployed_to,
            "sourceType": self.source_type,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotIssue {
    id: String,
    code: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    schema_version: u32,
    synced_at: String,
    manager: SnapshotManager,
    digest: String,
    skills: Vec<SnapshotSkill>,
    #[serde(default)]
    invalid: Vec<SnapshotIssue>,
    #[serde(default)]
    warnings: Vec<SnapshotIssue>,
}

fn snapshot_digest(skills: &[SnapshotSkill]) -> String {
    let document = Value::Array(skills.iter().map(SnapshotSkill::digest_view).collect());
    sha256_prefixed(canonical_json(&document))
}

fn normalize_selection(selection: &ManagerSelection) -> HubResult<ManagerSelection> {
    fn pick(values: &[String], label: &str) -> HubResult<Vec<String>> {
        let mut cleaned: Vec<String> = Vec::new();
        for value in values {
            let trimmed = fsutil::js_trim(value).to_string();
            if trimmed.is_empty() {
                return Err(reject(
                    "invalid_manager_selection",
                    format!("selection.{label} must be an array of non-empty strings"),
                ));
            }
            if !cleaned.contains(&trimmed) {
                cleaned.push(trimmed);
            }
        }
        fsutil::sort_strings_utf16(&mut cleaned);
        Ok(cleaned)
    }
    Ok(ManagerSelection {
        preset_ids: pick(&selection.preset_ids, "presetIds")?,
        tags: pick(&selection.tags, "tags")?,
        ids: pick(&selection.ids, "ids")?,
    })
}

fn normalize_policy(policy: Option<&SkillPolicy>) -> HubResult<(SkillPolicyMode, Vec<String>)> {
    match policy {
        None => Ok((SkillPolicyMode::All, Vec::new())),
        Some(SkillPolicy::All) => Ok((SkillPolicyMode::All, Vec::new())),
        Some(SkillPolicy::Only { ids }) => {
            // Manager-side wrapper: token validity uses `is_safe_token`, but the
            // error code and duplicate/order handling stay those of 2.1.
            let mut seen: Vec<&str> = Vec::new();
            for id in ids {
                if !is_safe_token(id, 80) {
                    return Err(reject(
                        "invalid_skill_policy",
                        "skill IDs must be 1-80 safe letters, digits, dots, underscores, or hyphens",
                    ));
                }
                if seen.contains(&id.as_str()) {
                    return Err(reject(
                        "invalid_skill_policy",
                        "duplicate skill IDs are not allowed",
                    ));
                }
                seen.push(id.as_str());
            }
            let mut sorted: Vec<String> = ids.clone();
            fsutil::sort_strings_utf16(&mut sorted);
            Ok((SkillPolicyMode::Only, sorted))
        }
    }
}

fn matches_selection(skill: &SnapshotSkill, selection: &ManagerSelection) -> bool {
    let preset_ok = selection.preset_ids.is_empty()
        || skill
            .preset_ids
            .iter()
            .any(|id| selection.preset_ids.contains(id));
    let tag_ok =
        selection.tags.is_empty() || skill.tags.iter().any(|tag| selection.tags.contains(tag));
    let id_ok = selection.ids.is_empty() || selection.ids.contains(&skill.id);
    preset_ok && tag_ok && id_ok
}

/// Environment snapshot handed to the CLI; `None` inherits the daemon's env.
pub type EnvSnapshot = Option<BTreeMap<String, OsString>>;

pub struct FsManagerConnection {
    home: PathBuf,
    cli_path: Option<PathBuf>,
    env: EnvSnapshot,
    platform: String,
    timeout: Duration,
    skills_root: PathBuf,
    config_path: PathBuf,
    snapshot_path: PathBuf,
}

impl std::fmt::Debug for FsManagerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsManagerConnection")
            .field("home", &self.home)
            .field("cli_path", &self.cli_path)
            .finish_non_exhaustive()
    }
}

impl FsManagerConnection {
    pub fn new(home: impl AsRef<Path>) -> HubResult<Self> {
        Self::with_options(home, None, None, None, None)
    }

    pub fn with_options(
        home: impl AsRef<Path>,
        cli_path: Option<PathBuf>,
        env: EnvSnapshot,
        platform: Option<String>,
        timeout: Option<Duration>,
    ) -> HubResult<Self> {
        let home = assert_absolute(home.as_ref(), "unsafe_manager_path", "home")?;
        let cli_path = match cli_path {
            Some(path) => Some(assert_absolute(
                &path,
                "unsafe_manager_path",
                "cliPath",
            )?),
            None => None,
        };
        let skills_root = home.join("skills");
        Ok(Self {
            config_path: skills_root.join("manager-connection.json"),
            snapshot_path: skills_root.join("manager-catalog.json"),
            skills_root,
            home,
            cli_path,
            env,
            platform: platform.unwrap_or_else(|| std::env::consts::OS.to_string()),
            timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
        })
    }

    fn env_var(&self, key: &str) -> Option<OsString> {
        match &self.env {
            Some(env) => env.get(key).cloned(),
            None => std::env::var_os(key),
        }
    }

    fn platform(&self) -> &str {
        match self.platform.as_str() {
            "macos" => "darwin",
            other => other,
        }
    }

    fn manager_home(&self) -> PathBuf {
        if let Some(value) = self.env_var("SKILLS_MANAGER_HOME") {
            return PathBuf::from(value);
        }
        let user_home = self
            .env_var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        user_home.join(MANAGER_DIR_NAME)
    }

    fn ensure_private_dir(&self, path: &Path, create: bool) -> HubResult<bool> {
        if create && fsutil::stat_no_follow(path)?.is_none() {
            fsutil::ensure_private_dir(path)?;
        }
        let Some(metadata) = fsutil::stat_no_follow(path)? else {
            return Ok(false);
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(reject(
                "unsafe_manager_path",
                format!(
                    "skill manager storage must be a real directory: {}",
                    path.display()
                ),
            ));
        }
        if create {
            fsutil::chmod(path, fsutil::PRIVATE_DIR_MODE).map_err(HubError::from)?;
        }
        Ok(true)
    }

    fn read_record(&self, path: &Path, code: &'static str, label: &str) -> HubResult<Option<Value>> {
        let Some(metadata) = fsutil::stat_no_follow(path)? else {
            return Ok(None);
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(reject(
                "unsafe_manager_path",
                format!("{label} must be a real file: {}", path.display()),
            ));
        }
        let text = std::fs::read_to_string(path).map_err(HubError::from)?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| {
                reject(
                    code,
                    format!("{label} is not valid JSON: {} ({error})", path.display()),
                )
            })
    }

    fn assert_replaceable(&self, path: &Path) -> HubResult<()> {
        if let Some(metadata) = fsutil::stat_no_follow(path)? {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(reject(
                    "unsafe_manager_path",
                    format!(
                        "refusing to replace a non-regular manager file: {}",
                        path.display()
                    ),
                ));
            }
        }
        Ok(())
    }

    fn write_record(&self, path: &Path, value: &Value) -> HubResult<()> {
        self.assert_replaceable(path)?;
        self.ensure_private_dir(&self.skills_root, true)?;
        fsutil::atomic_write_json(path, value, fsutil::PRIVATE_FILE_MODE)?;
        fsutil::chmod(path, fsutil::PRIVATE_FILE_MODE).map_err(HubError::from)
    }

    fn read_config(&self) -> HubResult<Option<ManagerConfig>> {
        let Some(value) = self.read_record(
            &self.config_path,
            "invalid_manager_config",
            "manager connection",
        )?
        else {
            return Ok(None);
        };
        let mut config: ManagerConfig = serde_json::from_value(value).map_err(|error| {
            reject(
                "invalid_manager_config",
                format!("unsupported manager connection format: {error}"),
            )
        })?;
        if config.schema_version != CONFIG_VERSION || config.cli_path.is_empty() {
            return Err(reject(
                "invalid_manager_config",
                "unsupported manager connection format",
            ));
        }
        config.manager_base_dir = assert_absolute(
            Path::new(&config.manager_base_dir),
            "invalid_manager_config",
            "managerBaseDir",
        )?
        .to_string_lossy()
        .into_owned();
        config.manager_skills_dir = assert_absolute(
            Path::new(&config.manager_skills_dir),
            "invalid_manager_config",
            "managerSkillsDir",
        )?
        .to_string_lossy()
        .into_owned();
        config.selection = normalize_selection(&config.selection)?;
        Ok(Some(config))
    }

    fn read_snapshot(&self) -> HubResult<Option<Snapshot>> {
        let Some(value) = self.read_record(
            &self.snapshot_path,
            "invalid_manager_snapshot",
            "manager catalog snapshot",
        )?
        else {
            return Ok(None);
        };
        let snapshot: Snapshot = serde_json::from_value(value).map_err(|error| {
            reject(
                "invalid_manager_snapshot",
                format!("unsupported manager catalog snapshot format: {error}"),
            )
        })?;
        if snapshot.schema_version != SNAPSHOT_VERSION {
            return Err(reject(
                "invalid_manager_snapshot",
                "unsupported manager catalog snapshot format",
            ));
        }
        for skill in &snapshot.skills {
            if !is_safe_token(&skill.id, 80) || skill.path.is_empty() {
                return Err(reject(
                    "invalid_manager_snapshot",
                    "manager catalog snapshot contains an invalid skill entry",
                ));
            }
        }
        Ok(Some(snapshot))
    }

    fn public_config(&self, config: &ManagerConfig) -> Value {
        json!({
            "schemaVersion": config.schema_version,
            "connectedAt": config.connected_at,
            "lastSyncedAt": config.last_synced_at,
            "cliPath": config.cli_path,
            "managerBaseDir": config.manager_base_dir,
            "managerSkillsDir": config.manager_skills_dir,
            "managerVersion": config.manager_version,
            "agentKey": config.agent_key,
            "selection": {
                "presetIds": config.selection.preset_ids,
                "tags": config.selection.tags,
                "ids": config.selection.ids,
            },
            "source": config.source,
        })
    }

    fn source_state(&self, snapshot: &Snapshot) -> Value {
        let dir = Path::new(&snapshot.manager.skills_dir);
        let skills_dir_ok = fsutil::stat_no_follow(dir)
            .ok()
            .flatten()
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        let mut missing: Vec<String> = Vec::new();
        for skill in &snapshot.skills {
            let ok = fsutil::stat_no_follow(Path::new(skill.entry()))
                .ok()
                .flatten()
                .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                .unwrap_or(false);
            if !ok {
                missing.push(skill.id.clone());
            }
        }
        json!({
            "skillsDir": snapshot.manager.skills_dir,
            "skillsDirOk": skills_dir_ok,
            "missing": missing,
            "checkedAt": now_iso(),
        })
    }

    fn snapshot_summary(&self, snapshot: Option<&Snapshot>, source: Option<&Value>) -> Value {
        let Some(snapshot) = snapshot else {
            return Value::Null;
        };
        let source_owned = match source {
            Some(value) => value.clone(),
            None => self.source_state(snapshot),
        };
        let age_ms = chrono::DateTime::parse_from_rfc3339(&snapshot.synced_at)
            .ok()
            .map(|parsed| {
                (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_milliseconds()
            });
        let stale = age_ms.is_none_or(|age| age > STALE_AFTER_MS);
        let missing: Vec<String> = source_owned
            .get("missing")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|text| text.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let invalid: Vec<Value> = snapshot
            .invalid
            .iter()
            .map(|entry| json!({"id": entry.id, "code": entry.code, "reason": entry.reason}))
            .collect();
        let warnings: Vec<Value> = snapshot
            .warnings
            .iter()
            .map(|entry| json!({"id": entry.id, "code": entry.code, "reason": entry.reason}))
            .collect();
        let checked_at = source_owned.get("checkedAt").cloned().unwrap_or(Value::Null);
        let skills_dir_ok = source_owned
            .get("skillsDirOk")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let fully_stale = stale || !missing.is_empty() || !invalid.is_empty() || !skills_dir_ok;
        let stale_reason = if stale {
            Some("snapshot is older than 24h".to_string())
        } else if !skills_dir_ok {
            Some("manager skills directory is missing".to_string())
        } else if !missing.is_empty() {
            Some(format!(
                "{} selected skill source path(s) missing",
                missing.len()
            ))
        } else if !invalid.is_empty() {
            Some(format!(
                "{} selected manager entry(ies) are not importable",
                invalid.len()
            ))
        } else {
            None
        };
        json!({
            "syncedAt": snapshot.synced_at,
            "skillCount": snapshot.skills.len(),
            "selectedIds": snapshot.skills.iter().map(|skill| skill.id.clone()).collect::<Vec<_>>(),
            "digest": snapshot.digest,
            "mode": snapshot.manager.mode,
            "managerVersion": snapshot.manager.version,
            "managerSkillsDir": snapshot.manager.skills_dir,
            "ageMs": age_ms,
            "freshness": {"ageMs": age_ms, "stale": stale},
            "availability": {
                "selectedInvalid": invalid,
                "warnings": warnings,
                "invalidCount": invalid.len(),
                "warningCount": warnings.len(),
                "checkedAt": checked_at,
            },
            "stale": fully_stale,
            "staleReason": stale_reason,
            "missingSources": missing,
            "invalidSources": invalid,
            "warnings": warnings,
            "skippedCount": invalid.len(),
            "warningCount": warnings.len(),
        })
    }

    async fn probe(&self) -> Value {
        let configured = self.read_config().ok().flatten();
        let found = self
            .discover(configured.as_ref().map(|config| config.cli_path.clone()))
            .await;
        json!({
            "available": found.available,
            "cliPath": found.cli_path,
            "version": found.version,
            "versionSupported": found.version_supported,
            "baseDir": found.manager_base_dir,
            "skillsDir": found.manager_skills_dir,
            "reason": found.reason,
        })
    }

    /// Locate the real manager CLI without touching manager state.
    async fn discover(&self, configured_cli: Option<String>) -> Discovery {
        let manager_home = self.manager_home();
        let manager_home = fsutil::absolute_normalized(&manager_home);
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(path) = configured_cli {
            candidates.push(fsutil::absolute_normalized(Path::new(&path)));
        }
        for key in ["PIHUB_SKILLS_MANAGER_CLI", "SKILLS_MANAGER_CLI"] {
            if let Some(value) = self.env_var(key) {
                candidates.push(fsutil::absolute_normalized(Path::new(&value)));
            }
        }
        if let Some(path) = &self.cli_path {
            candidates.push(path.clone());
        }
        candidates.push(manager_home.join("bin").join(CLI_NAME));
        if self.platform() == "darwin" {
            candidates.push(PathBuf::from(format!(
                "/Applications/skills-manager.app/Contents/MacOS/{CLI_NAME}"
            )));
        }
        if let Some(path) = self.env_var("PATH") {
            for entry in std::env::split_paths(&path) {
                if !entry.as_os_str().is_empty() {
                    candidates.push(entry.join(CLI_NAME));
                }
            }
        }
        let mut unique: Vec<PathBuf> = Vec::new();
        for candidate in candidates {
            if !unique.contains(&candidate) {
                unique.push(candidate);
            }
        }
        let found = unique.iter().find_map(|candidate| executable_file(candidate));
        let mut discovery = Discovery {
            available: false,
            cli_path: found.clone(),
            version: None,
            manager_base_dir: manager_home.clone(),
            manager_skills_dir: manager_home.join("skills"),
            version_supported: false,
            reason: None,
        };
        let Some(found) = found else {
            discovery.reason = Some(
                "skills-manager CLI not found; set PIHUB_SKILLS_MANAGER_CLI or pass cliPath"
                    .to_string(),
            );
            return discovery;
        };
        match self.run(&found, &["--version"]).await {
            Ok(output) => match parse_version(&output) {
                Some(version) => {
                    discovery.available = true;
                    discovery.version_supported = version_at_least(&version, MIN_SUPPORTED);
                    if !discovery.version_supported {
                        discovery.reason = Some(format!(
                            "skills-manager {version} is older than {}",
                            MIN_SUPPORTED
                                .iter()
                                .map(|part| part.to_string())
                                .collect::<Vec<_>>()
                                .join(".")
                        ));
                    }
                    discovery.version = Some(version);
                }
                None => {
                    discovery.reason =
                        Some("skills-manager CLI did not report a release version".to_string());
                }
            },
            Err(error) => discovery.reason = Some(error.message),
        }
        discovery
    }

    /// Run one read-only CLI command with a bounded output and a hard timeout.
    async fn run(&self, cli_path: &Path, args: &[&str]) -> HubResult<String> {
        let mut command = Command::new(cli_path);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = &self.env {
            command.env_clear();
            command.envs(env);
        }
        let mut child = command.spawn().map_err(|error| {
            reject(
                "manager_query_failed",
                format!("{CLI_NAME} {} failed: {error}", args.join(" ")),
            )
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let timeout = self.timeout;
        let collected = tokio::time::timeout(timeout, async move {
            let (out, err) = tokio::join!(read_capped(stdout), read_capped(stderr));
            let status = child.wait().await;
            (out, err, status)
        })
        .await;
        let (out, err, status) = match collected {
            Ok(value) => value,
            Err(_) => {
                return Err(reject(
                    "manager_query_failed",
                    format!(
                        "{CLI_NAME} {} timed out after {}s",
                        args.join(" "),
                        timeout.as_secs()
                    ),
                ));
            }
        };
        let stdout = out.map_err(|error| reject("manager_query_failed", error.to_string()))?;
        let stderr = err.unwrap_or_default();
        let status = status.map_err(|error| {
            reject(
                "manager_query_failed",
                format!("{CLI_NAME} {} failed: {error}", args.join(" ")),
            )
        })?;
        if !status.success() {
            let detail = String::from_utf8_lossy(&stderr)
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            let detail = if detail.trim().is_empty() {
                "no output".to_string()
            } else {
                detail.trim().to_string()
            };
            return Err(reject(
                "manager_query_failed",
                format!("{CLI_NAME} {} failed: {detail}", args.join(" ")),
            ));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    async fn read_json_command(&self, cli_path: &Path, command: &str) -> HubResult<Value> {
        let mut args: Vec<&str> = command.split(' ').collect();
        assert_read_only(&args)?;
        args.push("--json");
        let output = self.run(cli_path, &args).await?;
        serde_json::from_str(&output).map_err(|_| {
            reject(
                "manager_query_failed",
                format!("{command} did not return JSON"),
            )
        })
    }

    async fn repo_status(&self, cli_path: &Path) -> HubResult<(PathBuf, PathBuf, Option<String>)> {
        let parsed = self.read_json_command(cli_path, "repo status").await?;
        let base_dir = parsed
            .get("base_dir")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("manager_query_failed", "repo status is missing base_dir/skills_dir"))?;
        let skills_dir = parsed
            .get("skills_dir")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("manager_query_failed", "repo status is missing base_dir/skills_dir"))?;
        Ok((
            assert_absolute(
                Path::new(base_dir),
                "invalid_manager_config",
                "manager base directory",
            )?,
            assert_absolute(
                Path::new(skills_dir),
                "invalid_manager_config",
                "manager skills directory",
            )?,
            parsed
                .get("active_preset_id")
                .and_then(Value::as_str)
                .map(|value| value.to_string()),
        ))
    }

    async fn list_presets(&self, cli_path: &Path) -> HubResult<Vec<Value>> {
        let parsed = self.read_json_command(cli_path, "presets list").await?;
        let items = parsed
            .as_array()
            .ok_or_else(|| reject("manager_query_failed", "presets list did not return an array"))?;
        Ok(items
            .iter()
            .map(|preset| {
                json!({
                    "id": preset.get("id").map(scalar_string).unwrap_or_default(),
                    "name": preset.get("name").map(scalar_string).unwrap_or_default(),
                    "skillCount": preset.get("skill_count").and_then(Value::as_u64).unwrap_or(0),
                    "active": preset.get("active").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect())
    }

    async fn list_skills(&self, cli_path: &Path) -> HubResult<Vec<SnapshotSkill>> {
        let parsed = self.read_json_command(cli_path, "skills list").await?;
        let items = parsed
            .as_array()
            .ok_or_else(|| reject("manager_query_failed", "skills list did not return an array"))?;
        let mut skills = Vec::with_capacity(items.len());
        for skill in items {
            let name = skill.get("name").map(scalar_string).unwrap_or_default();
            if !is_safe_token(&name, 80) {
                return Err(reject(
                    "invalid_manager_snapshot",
                    format!("manager returned an unusable skill name: {name:?}"),
                ));
            }
            let path = skill
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    reject(
                        "invalid_manager_snapshot",
                        "manager skill entry is missing its path",
                    )
                })?;
            skills.push(SnapshotSkill {
                id: name.clone(),
                name,
                description: skill
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|text| text.to_string()),
                path: assert_absolute(Path::new(path), "invalid_manager_snapshot", "skill path")?
                    .to_string_lossy()
                    .into_owned(),
                entry_path: None,
                enabled: skill.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                tags: string_array(skill.get("tags")),
                presets: string_array(skill.get("presets")),
                preset_ids: string_array(skill.get("preset_ids")),
                deployed_to: string_array(skill.get("deployed_to")),
                source_type: skill
                    .get("source_type")
                    .and_then(Value::as_str)
                    .unwrap_or("local")
                    .to_string(),
            });
        }
        Ok(skills)
    }

    /// Resolve a path that must live inside the manager library, accepting a
    /// system alias for the library root (macOS `/tmp` -> `/private/tmp`) while
    /// still rejecting every symlink segment below the chosen root.
    fn canonical_entry(
        &self,
        literal_path: &Path,
        literal_root: &Path,
        canonical_root: &Path,
        label: &str,
    ) -> HubResult<PathBuf> {
        let resolved = fsutil::absolute_normalized(literal_path);
        let (base, segments) = if fsutil::is_within(&resolved, literal_root) {
            (literal_root.to_path_buf(), resolved.strip_prefix(literal_root))
        } else if fsutil::is_within(&resolved, canonical_root) {
            (canonical_root.to_path_buf(), resolved.strip_prefix(canonical_root))
        } else {
            return Err(reject(
                "unsafe_manager_skill_path",
                format!("{label} escapes the manager library"),
            ));
        };
        let segments = segments.map_err(|_| {
            reject(
                "unsafe_manager_skill_path",
                format!("{label} escapes the manager library"),
            )
        })?;
        let mut cursor = base;
        for part in segments.components() {
            cursor.push(part);
            let Some(metadata) = fsutil::stat_no_follow(&cursor)? else {
                return Err(reject(
                    "manager_source_missing",
                    format!("{label} is missing: {}", cursor.display()),
                ));
            };
            if metadata.file_type().is_symlink() {
                return Err(reject(
                    "unsafe_manager_skill_path",
                    format!("{label} crosses a symlink inside the manager library"),
                ));
            }
        }
        let canonical = fsutil::real_path(&resolved, "manager_source_missing", label)?;
        let expected = canonical_root.join(segments);
        if canonical != expected {
            return Err(reject(
                "unsafe_manager_skill_path",
                format!("{label} crosses a symlink inside the manager library"),
            ));
        }
        if !fsutil::is_within(&canonical, canonical_root) {
            return Err(reject(
                "unsafe_manager_skill_path",
                format!("{label} escapes the manager library"),
            ));
        }
        Ok(canonical)
    }

    /// Resolve the directory that actually holds `SKILL.md` for one entry.
    async fn resolve_entry(
        &self,
        cli_path: &Path,
        skill: &SnapshotSkill,
        literal_root: &Path,
        canonical_root: &Path,
    ) -> HubResult<PathBuf> {
        let literal_path = assert_absolute(
            Path::new(&skill.path),
            "invalid_manager_snapshot",
            &format!("skill path for {}", skill.id),
        )?;
        let stat = fsutil::stat_no_follow(&literal_path)?;
        if stat
            .as_ref()
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(reject(
                "unsafe_manager_skill_path",
                format!("manager skill directory must not be a symlink: {}", skill.id),
            ));
        }
        let has_manifest = stat
            .as_ref()
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
            && fsutil::stat_no_follow(&literal_path.join("SKILL.md"))?.is_some();
        if has_manifest {
            return self.canonical_entry(
                &literal_path,
                literal_root,
                canonical_root,
                &format!("skill {}", skill.id),
            );
        }
        let shown = self
            .read_json_command(cli_path, &format!("skills show {}", skill.id))
            .await
            .map_err(|error| {
                reject(
                    "invalid_manager_skill",
                    format!(
                        "manager cannot resolve a SKILL.md entry for {}: {}",
                        skill.id, error.message
                    ),
                )
            })?;
        let skill_file = shown.get("skill_file").and_then(Value::as_str).ok_or_else(|| {
            reject(
                "invalid_manager_skill",
                format!("manager did not report a SKILL.md entry for {}", skill.id),
            )
        })?;
        let skill_file_path = Path::new(skill_file);
        let basename = skill_file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !skill_file_path.is_absolute() || basename != "SKILL.md" {
            return Err(reject(
                "invalid_manager_skill",
                format!("manager did not report a SKILL.md entry for {}", skill.id),
            ));
        }
        let Some(file_stat) = fsutil::stat_no_follow(skill_file_path)? else {
            if !stat.map(|metadata| metadata.is_dir()).unwrap_or(false) {
                return Err(reject(
                    "manager_source_missing",
                    format!("manager skill source is missing: {}", skill.id),
                ));
            }
            return Err(reject(
                "invalid_manager_skill",
                format!("manager-reported SKILL.md entry is missing: {}", skill.id),
            ));
        };
        if file_stat.file_type().is_symlink() || !file_stat.is_file() {
            return Err(reject(
                "unsafe_manager_skill_path",
                format!("manager skill entry must be a regular SKILL.md file: {}", skill.id),
            ));
        }
        let entry_dir = skill_file_path.parent().map(Path::to_path_buf).ok_or_else(|| {
            reject(
                "unsafe_manager_skill_path",
                format!("skill_file for {} has no parent", skill.id),
            )
        })?;
        let dir_stat = fsutil::stat_no_follow(&entry_dir)?;
        match dir_stat {
            Some(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            _ => {
                return Err(reject(
                    "unsafe_manager_skill_path",
                    format!(
                        "manager skill entry directory is not a real directory: {}",
                        skill.id
                    ),
                ));
            }
        }
        self.canonical_entry(
            &entry_dir,
            literal_root,
            canonical_root,
            &format!("skill {}", skill.id),
        )
    }

    async fn build_snapshot(
        &self,
        cli_path: &Path,
        skills_dir: &Path,
        base_dir: &Path,
        version: &str,
        selection: &ManagerSelection,
    ) -> HubResult<Snapshot> {
        let dir_stat = fsutil::stat_no_follow(skills_dir)?;
        match dir_stat {
            Some(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            _ => {
                return Err(reject(
                    "manager_source_missing",
                    format!(
                        "manager skills directory is missing: {}",
                        skills_dir.display()
                    ),
                ));
            }
        }
        let literal_root = fsutil::absolute_normalized(skills_dir);
        let canonical_root = fsutil::real_path(
            skills_dir,
            "manager_source_missing",
            "manager skills directory",
        )?;
        let skills = self.list_skills(cli_path).await?;
        let presets = self.list_presets(cli_path).await?;
        let known_ids: Vec<&str> = skills.iter().map(|skill| skill.id.as_str()).collect();
        let known_presets: Vec<String> = presets
            .iter()
            .filter_map(|preset| preset.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        for id in &selection.preset_ids {
            if !known_presets.contains(id) {
                return Err(reject(
                    "invalid_manager_selection",
                    format!("unknown manager preset ID: {id}"),
                ));
            }
        }
        for id in &selection.ids {
            if !known_ids.contains(&id.as_str()) {
                return Err(reject(
                    "invalid_manager_selection",
                    format!("unknown manager skill: {id}"),
                ));
            }
        }
        let mut usable: Vec<SnapshotSkill> = Vec::new();
        let mut unusable: Vec<(SnapshotSkill, SnapshotIssue)> = Vec::new();
        for skill in skills {
            match self
                .resolve_entry(cli_path, &skill, &literal_root, &canonical_root)
                .await
            {
                Ok(entry) => {
                    let mut resolved = skill;
                    resolved.entry_path = Some(entry.to_string_lossy().into_owned());
                    usable.push(resolved);
                }
                Err(error) => {
                    let issue = SnapshotIssue {
                        id: skill.id.clone(),
                        code: error.code,
                        reason: error.message,
                    };
                    unusable.push((skill, issue));
                }
            }
        }
        let mut selected: Vec<SnapshotSkill> = usable
            .into_iter()
            .filter(|skill| matches_selection(skill, selection))
            .collect();
        selected.sort_by(|left, right| fsutil::utf16_cmp(&left.id, &right.id));
        let mut invalid: Vec<SnapshotIssue> = Vec::new();
        let mut warnings: Vec<SnapshotIssue> = Vec::new();
        for (skill, issue) in unusable {
            if matches_selection(&skill, selection) {
                invalid.push(issue);
            } else {
                warnings.push(issue);
            }
        }
        for id in &selection.ids {
            if let Some(issue) = invalid.iter().find(|issue| &issue.id == id) {
                return Err(HubError::new(issue.code.clone(), issue.reason.clone()));
            }
        }
        if selected.is_empty() {
            if let Some(issue) = invalid.first() {
                return Err(HubError::new(issue.code.clone(), issue.reason.clone()));
            }
            return Err(reject(
                "manager_catalog_empty",
                "manager catalog has no matching skills",
            ));
        }
        let digest = snapshot_digest(&selected);
        Ok(Snapshot {
            schema_version: SNAPSHOT_VERSION,
            synced_at: now_iso(),
            manager: SnapshotManager {
                cli_path: cli_path.to_string_lossy().into_owned(),
                version: version.to_string(),
                base_dir: base_dir.to_string_lossy().into_owned(),
                skills_dir: skills_dir.to_string_lossy().into_owned(),
                mode: "cli".to_string(),
                active_preset_id: presets
                    .iter()
                    .find(|preset| {
                        preset
                            .get("active")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .and_then(|preset| {
                        preset.get("id").and_then(Value::as_str).map(str::to_string)
                    }),
                presets,
            },
            digest,
            skills: selected,
            invalid,
            warnings,
        })
    }

    fn list_skills_from_snapshot(&self, filters: Option<&ManagerSelection>) -> HubResult<Vec<Value>> {
        let skills = self
            .read_snapshot()?
            .map(|snapshot| snapshot.skills)
            .unwrap_or_default();
        Ok(skills
            .into_iter()
            .filter(|skill| match filters {
                Some(selection) => matches_selection(skill, selection),
                None => true,
            })
            .map(|skill| skill.api_view())
            .collect())
    }
}

fn scalar_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().map(scalar_string).collect())
        .unwrap_or_default()
}

fn assert_read_only(args: &[&str]) -> HubResult<()> {
    let command = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
    if !READ_ONLY_COMMANDS.contains(&command.as_str()) {
        return Err(reject(
            "manager_write_refused",
            format!("only read-only manager commands are allowed, refused: {command}"),
        ));
    }
    Ok(())
}

fn executable_file(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let metadata = fsutil::stat_no_follow(path).ok().flatten()?;
    if !metadata.is_file() {
        return None;
    }
    let access = std::process::Command::new("test")
        .arg("-x")
        .arg(path)
        .status()
        .ok()?;
    if !access.success() {
        return None;
    }
    fsutil::real_path(path, "manager_query_failed", "manager CLI").ok()
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .rev()
        .find(|token| {
            let parts: Vec<&str> = token.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .map(|token| token.to_string())
}

fn version_at_least(version: &str, minimum: [u64; 3]) -> bool {
    let parts: Vec<u64> = version
        .split('.')
        .filter_map(|part| part.parse::<u64>().ok())
        .collect();
    if parts.len() != 3 {
        return false;
    }
    for index in 0..3 {
        if parts[index] != minimum[index] {
            return parts[index] > minimum[index];
        }
    }
    true
}

struct Discovery {
    available: bool,
    cli_path: Option<PathBuf>,
    version: Option<String>,
    manager_base_dir: PathBuf,
    manager_skills_dir: PathBuf,
    version_supported: bool,
    reason: Option<String>,
}

async fn read_capped<R>(reader: Option<R>) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(reader) = reader else {
        return Ok(Vec::new());
    };
    let mut buffer = Vec::new();
    let mut limited = reader.take((MAX_OUTPUT_BYTES + 1) as u64);
    limited.read_to_end(&mut buffer).await?;
    if buffer.len() > MAX_OUTPUT_BYTES {
        let _ = fsutil::path_exists(Path::new("/"));
        return Err(std::io::Error::other(
            "manager CLI output exceeded the 16 MiB bound",
        ));
    }
    Ok(buffer)
}

#[async_trait::async_trait]
impl ManagerConnection for FsManagerConnection {
    async fn status(&self, live: bool) -> HubResult<Value> {
        let mut result = json!({
            "connected": false,
            "config": Value::Null,
            "snapshot": Value::Null,
            "manager": Value::Null,
            "source": Value::Null,
            "drift": Value::Null,
            "error": Value::Null,
        });
        let config = match self.read_config() {
            Ok(config) => config,
            Err(error) => {
                let base = result.as_object_mut().expect("status object");
                base.insert("error".into(), json!({"code": error.code, "message": error.message}));
                return Ok(result);
            }
        };
        let snapshot = match self.read_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let base = result.as_object_mut().expect("status object");
                base.insert("error".into(), json!({"code": error.code, "message": error.message}));
                return Ok(result);
            }
        };
        let source = snapshot.as_ref().map(|snapshot| self.source_state(snapshot));
        let summary = self.snapshot_summary(snapshot.as_ref(), source.as_ref());
        {
            let base = result.as_object_mut().expect("status object");
            base.insert("source".into(), source.clone().unwrap_or(Value::Null));
            base.insert("snapshot".into(), summary);
        }
        let Some(config) = config else {
            if live {
                let manager = self.probe().await;
                result
                    .as_object_mut()
                    .expect("status object")
                    .insert("manager".into(), manager);
            }
            return Ok(result);
        };
        {
            let base = result.as_object_mut().expect("status object");
            base.insert("connected".into(), json!(true));
            base.insert("config".into(), self.public_config(&config));
        }
        if !live {
            return Ok(result);
        }
        let manager = self.probe().await;
        let mut drift = Value::Null;
        if manager
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && snapshot.is_some()
        {
            let cli_path = manager
                .get("cliPath")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_default();
            match self.list_skills(&cli_path).await {
                Ok(live_all) => {
                    let snapshot = snapshot.expect("checked above");
                    let live_scoped: Vec<SnapshotSkill> = live_all
                        .into_iter()
                        .filter(|skill| matches_selection(skill, &config.selection))
                        .collect();
                    let known: BTreeMap<String, &SnapshotSkill> = snapshot
                        .skills
                        .iter()
                        .map(|skill| (skill.id.clone(), skill))
                        .collect();
                    let suppressed: Vec<String> = snapshot
                        .invalid
                        .iter()
                        .chain(snapshot.warnings.iter())
                        .map(|entry| entry.id.clone())
                        .collect();
                    let live_ids: Vec<String> =
                        live_scoped.iter().map(|skill| skill.id.clone()).collect();
                    let mut live_known: Vec<SnapshotSkill> = live_scoped
                        .iter()
                        .filter(|skill| known.contains_key(&skill.id))
                        .cloned()
                        .collect();
                    live_known.sort_by(|left, right| fsutil::utf16_cmp(&left.id, &right.id));
                    let mut added: Vec<String> = live_ids
                        .iter()
                        .filter(|id| !known.contains_key(*id) && !suppressed.contains(*id))
                        .cloned()
                        .collect();
                    added.sort_by(|left, right| fsutil::utf16_cmp(left, right));
                    let mut removed: Vec<String> = known
                        .keys()
                        .filter(|id| !live_ids.contains(*id))
                        .cloned()
                        .collect();
                    removed.sort_by(|left, right| fsutil::utf16_cmp(left, right));
                    let mut snapshot_sorted: Vec<SnapshotSkill> = snapshot.skills.clone();
                    snapshot_sorted.sort_by(|left, right| fsutil::utf16_cmp(&left.id, &right.id));
                    let metadata_changed =
                        snapshot_digest(&live_known) != snapshot_digest(&snapshot_sorted);
                    let mut catalog_unavailable: Vec<String> = live_ids
                        .iter()
                        .filter(|id| suppressed.contains(*id))
                        .cloned()
                        .collect();
                    catalog_unavailable.sort_by(|left, right| fsutil::utf16_cmp(left, right));
                    drift = json!({
                        "added": added,
                        "removed": removed,
                        "changed": !added.is_empty() || !removed.is_empty() || metadata_changed,
                        "digest": snapshot_digest(&live_known),
                        "catalogUnavailable": catalog_unavailable,
                    });
                }
                Err(error) => {
                    result.as_object_mut().expect("status object").insert(
                        "manager".into(),
                        json!({"available": false, "reason": error.message}),
                    );
                    return Ok(result);
                }
            }
        }
        let base = result.as_object_mut().expect("status object");
        base.insert("manager".into(), manager);
        base.insert("drift".into(), drift);
        Ok(result)
    }

    async fn connect(&self, input: ManagerConnectInput) -> HubResult<Value> {
        let configured_cli = input
            .cli_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .or_else(|| self.cli_path.as_ref().map(|path| path.to_string_lossy().into_owned()));
        let discovery = self.discover(configured_cli).await;
        if !discovery.available {
            return Err(reject(
                "manager_unavailable",
                discovery
                    .reason
                    .unwrap_or_else(|| "skills-manager CLI is unavailable".to_string()),
            ));
        }
        if !discovery.version_supported {
            return Err(reject(
                "manager_unavailable",
                discovery.reason.unwrap_or_else(|| {
                    format!(
                        "skills-manager {} is not supported",
                        discovery.version.clone().unwrap_or_default()
                    )
                }),
            ));
        }
        let selection = normalize_selection(&input.selection)?;
        if let Some(agent_key) = &input.agent_key {
            if !is_safe_token(agent_key, 64) {
                return Err(reject(
                    "invalid_manager_agent",
                    "agentKey must be 1-64 letters, digits, dots, underscores, or hyphens",
                ));
            }
        }
        self.assert_replaceable(&self.config_path)?;
        self.assert_replaceable(&self.snapshot_path)?;
        let cli_path = discovery
            .cli_path
            .clone()
            .ok_or_else(|| reject("manager_unavailable", "skills-manager CLI is unavailable"))?;
        let version = discovery.version.clone().unwrap_or_default();
        let (base_dir, skills_dir, _) = self.repo_status(&cli_path).await?;
        if let Some(requested) = &input.manager_base_dir {
            let requested = assert_absolute(
                requested,
                "invalid_manager_config",
                "managerBaseDir",
            )?;
            if requested != base_dir
                && fsutil::real_path(&requested, "invalid_manager_config", "managerBaseDir")?
                    != fsutil::real_path(&base_dir, "invalid_manager_config", "managerBaseDir")?
            {
                return Err(reject(
                    "invalid_manager_config",
                    format!(
                        "manager base directory mismatch: manager reports {}",
                        base_dir.display()
                    ),
                ));
            }
        }
        if let Some(requested) = &input.manager_skills_dir {
            let requested = assert_absolute(
                requested,
                "invalid_manager_config",
                "managerSkillsDir",
            )?;
            if requested != skills_dir
                && fsutil::real_path(&requested, "invalid_manager_config", "managerSkillsDir")?
                    != fsutil::real_path(&skills_dir, "invalid_manager_config", "managerSkillsDir")?
            {
                return Err(reject(
                    "invalid_manager_config",
                    format!(
                        "manager skills directory mismatch: manager reports {}",
                        skills_dir.display()
                    ),
                ));
            }
        }
        if let Some(agent_key) = &input.agent_key {
            let agents = self.read_json_command(&cli_path, "agents list").await?;
            let found = agents
                .as_array()
                .map(|items| {
                    items.iter().any(|agent| {
                        agent.get("key").and_then(Value::as_str) == Some(agent_key.as_str())
                    })
                })
                .unwrap_or(false);
            if !found {
                return Err(reject(
                    "invalid_manager_agent",
                    format!("unknown manager agent key: {agent_key}"),
                ));
            }
        }
        let snapshot = self
            .build_snapshot(&cli_path, &skills_dir, &base_dir, &version, &selection)
            .await?;
        let config = ManagerConfig {
            schema_version: CONFIG_VERSION,
            connected_at: now_iso(),
            last_synced_at: snapshot.synced_at.clone(),
            cli_path: cli_path.to_string_lossy().into_owned(),
            manager_base_dir: base_dir.to_string_lossy().into_owned(),
            manager_skills_dir: skills_dir.to_string_lossy().into_owned(),
            manager_version: version,
            agent_key: input.agent_key.clone(),
            selection,
            source: input
                .source
                .as_deref()
                .map(fsutil::js_trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("manual")
                .to_string(),
        };
        let snapshot_value = serde_json::to_value(&snapshot)
            .map_err(|error| reject("invalid_manager_snapshot", error.to_string()))?;
        self.write_record(&self.snapshot_path, &snapshot_value)?;
        let config_value = serde_json::to_value(&config)
            .map_err(|error| reject("invalid_manager_config", error.to_string()))?;
        self.write_record(&self.config_path, &config_value)?;
        Ok(json!({
            "connected": true,
            "config": self.public_config(&config),
            "snapshot": self.snapshot_summary(Some(&snapshot), None),
        }))
    }

    async fn sync(&self, input: ManagerSyncInput) -> HubResult<Value> {
        let config = self
            .read_config()?
            .ok_or_else(|| reject("manager_not_connected", "no skills-manager connection; run connect first"))?;
        let previous = self.read_snapshot()?;
        let discovery = self.discover(Some(config.cli_path.clone())).await;
        if !discovery.available || !discovery.version_supported {
            return Ok(json!({
                "synced": false,
                "error": {
                    "code": "manager_unavailable",
                    "message": discovery.reason.unwrap_or_else(|| "skills-manager CLI is unavailable".to_string()),
                },
                "keptSnapshot": self.snapshot_summary(previous.as_ref(), None),
                "config": self.public_config(&config),
            }));
        }
        let cli_path = discovery
            .cli_path
            .clone()
            .ok_or_else(|| reject("manager_unavailable", "skills-manager CLI is unavailable"))?;
        let version = discovery.version.clone().unwrap_or_default();
        let outcome = async {
            let (base_dir, skills_dir, _) = self.repo_status(&cli_path).await?;
            let snapshot = self
                .build_snapshot(&cli_path, &skills_dir, &base_dir, &version, &config.selection)
                .await?;
            Ok::<_, HubError>(snapshot)
        }
        .await;
        let snapshot = match outcome {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Ok(json!({
                    "synced": false,
                    "error": {"code": error.code, "message": error.message},
                    "keptSnapshot": self.snapshot_summary(previous.as_ref(), None),
                    "config": self.public_config(&config),
                }));
            }
        };
        let before: Vec<String> = previous
            .as_ref()
            .map(|snapshot| snapshot.skills.iter().map(|skill| skill.id.clone()).collect())
            .unwrap_or_default();
        let after: Vec<String> = snapshot.skills.iter().map(|skill| skill.id.clone()).collect();
        let unchanged = previous.as_ref().map(|value| value.digest.clone()) == Some(snapshot.digest.clone());
        let skipped: Vec<Value> = snapshot
            .invalid
            .iter()
            .map(|entry| json!({"id": entry.id, "code": entry.code}))
            .collect();
        let warnings: Vec<Value> = snapshot
            .warnings
            .iter()
            .map(|entry| json!({"id": entry.id, "code": entry.code}))
            .collect();
        let refreshed = ManagerConfig {
            last_synced_at: snapshot.synced_at.clone(),
            manager_version: version,
            ..config.clone()
        };
        if unchanged && !input.force {
            let refreshed_value = serde_json::to_value(&refreshed)
                .map_err(|error| reject("invalid_manager_config", error.to_string()))?;
            self.write_record(&self.config_path, &refreshed_value)?;
            return Ok(json!({
                "synced": true,
                "changed": false,
                "added": [],
                "removed": [],
                "skipped": skipped,
                "warnings": warnings,
                "config": self.public_config(&refreshed),
                "snapshot": self.snapshot_summary(Some(&snapshot), None),
            }));
        }
        let snapshot_value = serde_json::to_value(&snapshot)
            .map_err(|error| reject("invalid_manager_snapshot", error.to_string()))?;
        self.write_record(&self.snapshot_path, &snapshot_value)?;
        let refreshed_value = serde_json::to_value(&refreshed)
            .map_err(|error| reject("invalid_manager_config", error.to_string()))?;
        self.write_record(&self.config_path, &refreshed_value)?;
        let mut added: Vec<String> = after
            .iter()
            .filter(|id| !before.contains(*id))
            .cloned()
            .collect();
        added.sort_by(|left, right| fsutil::utf16_cmp(left, right));
        let mut removed: Vec<String> = before
            .iter()
            .filter(|id| !after.contains(*id))
            .cloned()
            .collect();
        removed.sort_by(|left, right| fsutil::utf16_cmp(left, right));
        Ok(json!({
            "synced": true,
            "changed": true,
            "added": added,
            "removed": removed,
            "skipped": skipped,
            "warnings": warnings,
            "config": self.public_config(&refreshed),
            "snapshot": self.snapshot_summary(Some(&snapshot), None),
        }))
    }

    async fn disconnect(&self, input: ManagerDisconnectInput) -> HubResult<Value> {
        let existed = fsutil::path_exists(&self.config_path);
        if existed {
            std::fs::remove_file(&self.config_path).map_err(HubError::from)?;
        }
        if input.purge_snapshot {
            let _ = std::fs::remove_file(&self.snapshot_path);
        }
        let snapshot_kept = !input.purge_snapshot && fsutil::path_exists(&self.snapshot_path);
        let snapshot = if input.purge_snapshot {
            Value::Null
        } else {
            let current = self.read_snapshot()?;
            self.snapshot_summary(current.as_ref(), None)
        };
        Ok(json!({
            "connected": false,
            "alreadyDisconnected": !existed,
            "snapshotKept": snapshot_kept,
            "snapshot": snapshot,
        }))
    }

    fn resolve_sources(&self, policy: Option<&SkillPolicy>) -> HubResult<Vec<ManagerSkillSource>> {
        let config = self
            .read_config()?
            .ok_or_else(|| reject("manager_not_connected", "no skills-manager connection; run connect first"))?;
        let (mode, only_ids) = normalize_policy(policy)?;
        let snapshot = self
            .read_snapshot()?
            .ok_or_else(|| reject("manager_snapshot_missing", "no manager catalog snapshot; run sync first"))?;
        let skills_dir = PathBuf::from(&config.manager_skills_dir);
        match fsutil::stat_no_follow(&skills_dir)? {
            Some(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            _ => {
                return Err(reject(
                    "manager_source_missing",
                    format!(
                        "manager skills directory is missing: {}",
                        skills_dir.display()
                    ),
                ));
            }
        }
        let literal_root = fsutil::absolute_normalized(&skills_dir);
        let canonical_root = fsutil::real_path(
            &skills_dir,
            "manager_source_missing",
            "manager skills directory",
        )?;
        if mode == SkillPolicyMode::Only {
            for id in &only_ids {
                if let Some(issue) = snapshot.invalid.iter().find(|issue| &issue.id == id) {
                    return Err(HubError::new(issue.code.clone(), issue.reason.clone()));
                }
            }
        }
        let chosen: Vec<SnapshotSkill> = match mode {
            SkillPolicyMode::All => snapshot.skills.clone(),
            SkillPolicyMode::Only => {
                let mut selected = Vec::new();
                for id in &only_ids {
                    let skill = snapshot
                        .skills
                        .iter()
                        .find(|skill| &skill.id == id)
                        .ok_or_else(|| {
                            reject("skill_not_found", format!("unknown manager skill: {id}"))
                        })?;
                    selected.push(skill.clone());
                }
                selected
            }
        };
        let mut sources: Vec<ManagerSkillSource> = chosen
            .into_iter()
            .map(|skill| {
                Ok(ManagerSkillSource {
                    id: skill.id.clone(),
                    name: skill.name.clone(),
                    source_path: self.canonical_entry(
                        Path::new(skill.entry()),
                        &literal_root,
                        &canonical_root,
                        &format!("skill {}", skill.id),
                    )?,
                })
            })
            .collect::<HubResult<Vec<_>>>()?;
        sources.sort_by(|left, right| fsutil::utf16_cmp(&left.id, &right.id));
        Ok(sources)
    }
}

/// Freeze every selected manager skill into `registry`, then return the exact
/// scope authorization (later manager changes can never widen it).
pub async fn resolve_manager_authorization(
    connection: &FsManagerConnection,
    registry: &FsSkillRegistry,
    policy: Option<&SkillPolicy>,
) -> HubResult<pihub_contracts::SkillAuthorization> {
    let sources = connection.resolve_sources(policy)?;
    let config_version = connection
        .read_config()?
        .map(|config| config.manager_version)
        .unwrap_or_default();
    for source in &sources {
        registry.import_skill_sync(
            &source.id,
            &source.source_path,
            Some(&format!(
                "skills-manager {config_version} @ {}",
                source.source_path.display()
            )),
        )?;
    }
    registry.resolve_authorization_sync(Some(&SkillPolicy::Only {
        ids: sources.iter().map(|source| source.id.clone()).collect(),
    }))
}

/// Public list used by `/v2/skills/manager` payloads.
pub fn list_manager_skills(
    connection: &FsManagerConnection,
    filters: Option<&ManagerSelection>,
) -> HubResult<Vec<Value>> {
    connection.list_skills_from_snapshot(filters)
}

/// Exposed for tests and for the app: the exact `status` shape without I/O.
pub fn empty_status() -> Value {
    let mut map = Map::new();
    map.insert("connected".into(), json!(false));
    map.insert("config".into(), Value::Null);
    map.insert("snapshot".into(), Value::Null);
    map.insert("manager".into(), Value::Null);
    map.insert("source".into(), Value::Null);
    map.insert("drift".into(), Value::Null);
    map.insert("error".into(), Value::Null);
    Value::Object(map)
}
