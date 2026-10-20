//! Agent lifecycle and the app-facing `HubApi` implementation.
//!
//! The manager is the single logical event writer: runtime adapters send
//! bounded signals over a `tokio` channel and never touch SQLite.

use std::collections::HashMap;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pihub_contracts::{
    assert_agent_id, is_ephemeral_stream_event, new_id, now_iso, AgentPatch, AgentRecord, AgentSpec,
    AgentStatus, AgentView, AgentViewV2, ApiView, Carrier, ControlAction, ControlResult,
    DesiredState, EventQuery, HubError, HubResult, HubApi, ListAgentsOptions, ManagerConnection,
    NewEvent,
    PromptCatalog, PromptService, PromptSummary, Provider, ProviderInput,
    ProviderSnapshot, ProviderService, PublicAgent, Runtime, RuntimeEvent, RuntimeFactory,
    RuntimeLaunch, RuntimeRequestKind, SkillImportInput, SkillImportResult, SkillPolicy, SkillRef,
    SkillRegistry, SkillSummary, Snapshot, SnapshotV1, SnapshotV2, Store, StoredEvent,
    RUNTIME_EVENT_CHANNEL_CAPACITY,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::config::ensure_pi_config_files;
use crate::fsutil;
use crate::logging::{resolve_hub_log_dir, AgentLog, HubLog};
use crate::profiles::ResolvedProfile;
use crate::prompts::DefaultPromptService;
use crate::skills::manager_connection::{
    list_manager_skills, resolve_manager_authorization, FsManagerConnection,
};
use crate::skills::registry::FsSkillRegistry;


type RuntimeMap = Arc<std::sync::Mutex<HashMap<String, Arc<dyn Runtime>>>>;

#[derive(Clone)]
pub struct AgentManagerOptions {
    pub paths: pihub_contracts::HubPaths,
    pub store: Arc<dyn Store>,
    pub profile: ResolvedProfile,
    pub runtime_factory: Arc<dyn RuntimeFactory>,
    pub skill_registry: Option<Arc<FsSkillRegistry>>,
    pub manager_connection: Option<Arc<FsManagerConnection>>,
    pub provider_service: Option<Arc<dyn ProviderService>>,
    pub log: Option<Arc<HubLog>>,
    pub log_debug: bool,
}

pub struct AgentManager {
    paths: pihub_contracts::HubPaths,
    store: Arc<dyn Store>,
    profile: ResolvedProfile,
    runtime_factory: Arc<dyn RuntimeFactory>,
    prompter: DefaultPromptService,
    skill_registry: Option<Arc<FsSkillRegistry>>,
    manager_connection: Option<Arc<FsManagerConnection>>,
    provider_service: Option<Arc<dyn ProviderService>>,
    injected_log: Option<Arc<HubLog>>,
    log: std::sync::Mutex<Option<Arc<HubLog>>>,
    log_debug: bool,
    runtimes: RuntimeMap,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("home", &self.paths.home)
            .field("profile", &self.profile.digest)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct AgentPaths {
    root: PathBuf,
    agent_dir: PathBuf,
    session_dir: PathBuf,
    logs_dir: PathBuf,
    state_dir: PathBuf,
    tmp_dir: PathBuf,
}

impl AgentManager {
    pub fn new(options: AgentManagerOptions) -> HubResult<Self> {
        Ok(Self {
            paths: options.paths,
            store: options.store,
            profile: options.profile,
            runtime_factory: options.runtime_factory,
            prompter: DefaultPromptService,
            skill_registry: options.skill_registry,
            manager_connection: options.manager_connection,
            provider_service: options.provider_service,
            injected_log: options.log.clone(),
            log: std::sync::Mutex::new(options.log),
            log_debug: options.log_debug,
            runtimes: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn paths(&self) -> &pihub_contracts::HubPaths {
        &self.paths
    }

    pub fn profile(&self) -> &ResolvedProfile {
        &self.profile
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    /// Agents left active by a previous daemon become `crashed` on startup.
    pub fn recover_after_daemon_restart(&self) -> HubResult<()> {
        self.store.mark_interrupted()
    }

    fn agent_paths(&self, id: &str, carrier: Carrier) -> AgentPaths {
        let root = self.paths.agents_dir.join(id);
        let agent_dir = root.join(match carrier {
            Carrier::Pi => "pi-agent".to_string(),
            other => format!("{}-agent", other.as_str()),
        });
        AgentPaths {
            session_dir: root.join("sessions"),
            logs_dir: root.join("logs"),
            state_dir: root.join("state"),
            tmp_dir: root.join("tmp"),
            agent_dir,
            root,
        }
    }

    /// Private per-Agent directories plus carrier-native configuration links.
    fn provision(&self, id: &str, carrier: Carrier, platform: bool) -> HubResult<AgentPaths> {
        let paths = self.agent_paths(id, carrier);
        for directory in [
            &paths.root,
            &paths.agent_dir,
            &paths.session_dir,
            &paths.logs_dir,
            &paths.state_dir,
            &paths.tmp_dir,
        ] {
            fsutil::ensure_private_dir(directory)?;
        }
        if carrier == Carrier::Pi {
            let settings = paths.agent_dir.join("settings.json");
            if !fsutil::path_exists(&settings) {
                fsutil::atomic_write_json(
                    &settings,
                    &json!({"enableInstallTelemetry": false, "defaultProjectTrust": "never"}),
                    fsutil::PRIVATE_FILE_MODE,
                )?;
            }
            if platform {
                let models_source = self.paths.cc_switch_pi_dir.join("models.json");
                if !fsutil::path_exists(&models_source) {
                    return Err(HubError::new(
                        "provider_unavailable",
                        "CC Switch Pi models.json is unavailable; add a Pi provider first",
                    ));
                }
                let target = paths.agent_dir.join("models.json");
                match fsutil::stat_no_follow(&target)? {
                    Some(metadata) => {
                        let link_ok = metadata.file_type().is_symlink()
                            && std::fs::read_link(&target).ok().as_deref() == Some(models_source.as_path());
                        if !link_ok {
                            return Err(HubError::new(
                                "provider_unavailable",
                                "Pi models link differs from the platform CC Switch configuration",
                            ));
                        }
                    }
                    None => symlink(&models_source, &target).map_err(HubError::from)?,
                }
                let auth = paths.agent_dir.join("auth.json");
                if !fsutil::path_exists(&auth) {
                    fsutil::atomic_write_json(&auth, &json!({}), fsutil::PRIVATE_FILE_MODE)?;
                }
            } else {
                ensure_pi_config_files(&self.paths)?;
                link_shared(&self.paths.auth_path, &paths.agent_dir.join("auth.json"))?;
                link_shared(&self.paths.models_path, &paths.agent_dir.join("models.json"))?;
            }
        }
        Ok(paths)
    }

    fn frozen_prompt_path(&self, state_dir: &Path) -> PathBuf {
        state_dir.join("prompt-config.json")
    }

    /// Frozen prompt of one Agent: store projection first, private file second.
    pub fn frozen_prompt(&self, agent: &AgentRecord) -> Option<pihub_contracts::FrozenPromptConfig> {
        if let Some(stored) = &agent.prompt_config {
            return Some(stored.clone());
        }
        let paths = self.agent_paths(&agent.id, agent.carrier);
        let raw = std::fs::read_to_string(self.frozen_prompt_path(&paths.state_dir)).ok();
        self.prompter.read_frozen(raw.as_deref())
    }

    pub fn prompt_catalog(&self) -> PromptCatalog {
        self.prompter.list()
    }

    pub fn prompt_config_of(
        &self,
        agent: &AgentRecord,
        include_text: bool,
    ) -> Option<PromptSummary> {
        self.frozen_prompt(agent)
            .map(|frozen| self.prompter.summary(&frozen, include_text))
    }

    fn record(&self, agent_id: Option<&str>, event_type: &str, payload: Value) -> HubResult<i64> {
        self.store.append_event(NewEvent::new(
            new_id(),
            agent_id.map(str::to_string),
            event_type,
            payload,
        ))
    }

    fn ensure_log(&self) -> HubResult<Arc<HubLog>> {
        if let Some(log) = self.injected_log.clone() {
            return Ok(log);
        }
        let mut guard = self.log.lock().map_err(|_| {
            HubError::new("log_unavailable", "log handle lock poisoned")
        })?;
        if let Some(log) = guard.clone() {
            return Ok(log);
        }
        let log = Arc::new(HubLog::open(&resolve_hub_log_dir(&self.paths))?);
        *guard = Some(log.clone());
        Ok(log)
    }

    pub fn agent_log(&self, agent_id: &str, carrier: Carrier, source: &str) -> HubResult<AgentLog> {
        let sink = self.ensure_log()?;
        Ok(AgentLog::new(sink, agent_id, carrier, source))
    }

    pub fn log_stats(&self) -> Option<Value> {
        self.log
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|log| log.stats()))
    }

    pub fn log_debug(&self) -> bool {
        self.log_debug
    }

    /// Flush and close the shared Hub log stream.
    pub async fn close_logs(&self) {
        let log = self.log.lock().ok().and_then(|mut guard| guard.take());
        if let Some(log) = log {
            let _ = log.close_sync();
        }
    }

    fn carrier_label(carrier: Carrier) -> &'static str {
        match carrier {
            Carrier::Pi => "Pi",
            Carrier::Codex => "codex",
            Carrier::Claude => "claude",
        }
    }

    /// Resolve the platform provider for a carrier through CC Switch.
    async fn resolve_platform_provider(
        &self,
        carrier: Carrier,
        spec: &AgentSpec,
    ) -> HubResult<(Option<String>, Option<String>)> {
        let Some(service) = &self.provider_service else {
            return Ok((spec.provider.clone(), spec.model.clone()));
        };
        let providers = service
            .list(carrier.into())
            .await?
            .into_iter()
            .filter(|provider| provider.configured)
            .collect::<Vec<Provider>>();
        let selected = spec
            .provider
            .clone()
            .or_else(|| {
                providers
                    .iter()
                    .find(|provider| provider.is_current == Some(true))
                    .map(|provider| provider.id.clone())
            })
            .or_else(|| {
                if providers.len() == 1 {
                    Some(providers[0].id.clone())
                } else {
                    None
                }
            });
        let Some(selected) = selected else {
            return Err(HubError::new(
                "provider_unavailable",
                format!(
                    "no CC Switch provider selected for {}; add one or choose a provider",
                    carrier.as_str()
                ),
            ));
        };
        let provider = providers
            .iter()
            .find(|provider| provider.id == selected)
            .ok_or_else(|| {
                HubError::new(
                    "provider_unavailable",
                    format!("CC Switch provider {selected} is unavailable for {}", carrier.as_str()),
                )
            })?;
        Ok((
            Some(selected),
            spec.model
                .clone()
                .or_else(|| provider.default_model.clone()),
        ))
    }

    async fn resolve_skill_authorization(
        &self,
        policy: Option<&SkillPolicy>,
    ) -> HubResult<Option<pihub_contracts::SkillAuthorization>> {
        let Some(registry) = &self.skill_registry else {
            return Ok(None);
        };
        if let Some(connection) = &self.manager_connection {
            let connected = connection
                .status(false)
                .await
                .ok()
                .and_then(|status| status.get("connected").and_then(Value::as_bool))
                .unwrap_or(false);
            if connected {
                return resolve_manager_authorization(connection, registry, policy)
                    .await
                    .map(Some);
            }
        }
        registry.resolve_authorization_sync(policy).map(Some)
    }

    /// Create one Agent. `platform=false` is the v1 path: Pi carrier, profile
    /// skills, no frozen authorization.
    pub async fn create_agent(&self, spec: AgentSpec, platform: bool) -> HubResult<AgentRecord> {
        let id = assert_agent_id(spec.agent_id.as_deref().unwrap_or(&new_id()))?.to_string();
        if self.store.get_agent(&id)?.is_some() {
            return Err(HubError::new(
                "agent_exists",
                format!("agent already exists: {id}"),
            ));
        }
        let cwd = resolve_cwd(spec.cwd.as_deref())?;
        let carrier = if platform {
            spec.carrier
        } else {
            Carrier::Pi
        };
        let workspace_mode = spec
            .workspace_mode
            .clone()
            .unwrap_or_else(|| "shared".to_string());
        if workspace_mode != "shared" {
            return Err(HubError::new(
                "invalid_workspace_mode",
                "workspaceMode must be shared in 1.0; isolated workspaces are not implemented yet",
            ));
        }
        if platform {
            let diagnostic = self.runtime_factory.diagnose(carrier).await?;
            if !diagnostic.available {
                return Err(HubError::new(
                    "carrier_unavailable",
                    format!(
                        "carrier {} is unavailable: {}",
                        carrier.as_str(),
                        diagnostic.reason.unwrap_or_else(|| "unknown reason".to_string())
                    ),
                ));
            }
        }
        let authorization = if platform {
            self.resolve_skill_authorization(spec.skill_policy.as_ref())
                .await?
        } else {
            None
        };
        let (provider, model) = if platform {
            self.resolve_platform_provider(carrier, &spec).await?
        } else {
            (spec.provider.clone(), spec.model.clone())
        };
        // Resolve the prompt before provisioning so an invalid or unsupported
        // prompt leaves no half-created Agent directory behind.
        let prompt_config =
            self.prompter
                .resolve(spec.prompt_config.as_ref(), carrier, Some(now_iso()))?;
        let name = spec
            .name
            .clone()
            .unwrap_or_else(|| format!("agent-{}", &id[..id.len().min(8)]));
        let private_paths = self.provision(&id, carrier, platform)?;
        fsutil::atomic_write_json(
            &self.frozen_prompt_path(&private_paths.state_dir),
            &prompt_config,
            fsutil::PRIVATE_FILE_MODE,
        )?;
        let now = now_iso();
        let skills: Vec<SkillRef> = match &authorization {
            Some(authorization) => authorization
                .grants
                .iter()
                .map(|grant| SkillRef::Grant {
                    id: grant.id.clone(),
                    path: grant.path.clone(),
                    sha256: grant.sha256.clone(),
                })
                .collect(),
            None => self
                .profile
                .skills
                .iter()
                .map(|resource| SkillRef::Profile {
                    path: resource.path.clone(),
                    sha256: resource.sha256.clone(),
                })
                .collect(),
        };
        let extensions = if platform {
            Vec::new()
        } else {
            self.profile.extensions.clone()
        };
        let record = AgentRecord {
            id: id.clone(),
            name: name.clone(),
            cwd: cwd.clone(),
            workspace_mode: workspace_mode.clone(),
            provider,
            model,
            thinking: spec.thinking,
            status: AgentStatus::Provisioning,
            desired_state: DesiredState::Running,
            pid: None,
            session_id: new_id(),
            session_file: None,
            session_dir: private_paths.session_dir.clone(),
            agent_dir: private_paths.agent_dir.clone(),
            logs_dir: private_paths.logs_dir.clone(),
            profile_digest: self.profile.digest.clone(),
            tools: self.profile.tools.clone(),
            skills,
            extensions,
            carrier,
            skill_authorization: authorization.clone(),
            prompt_config: Some(prompt_config.clone()),
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.create_agent(&record)?;
        let mut created_payload = json!({
            "name": name,
            "cwd": cwd.to_string_lossy(),
            "workspaceMode": workspace_mode,
            "profileDigest": self.profile.digest,
            "sharedWorkspaceWarning": workspace_mode == "shared",
            "prompt": self.prompter.summary(&prompt_config, false),
        });
        if platform {
            if let Some(object) = created_payload.as_object_mut() {
                object.insert("carrier".into(), json!(carrier.as_str()));
                object.insert(
                    "skillDigest".into(),
                    json!(authorization.as_ref().map(|value| value.digest.clone())),
                );
            }
        }
        self.record(Some(&id), "agent_created", created_payload)?;
        // `start_stored_agent` owns the failure projection for every caller.
        self.start_stored_agent(&id, spec.prompt.clone()).await
    }

    /// Start an Agent and finalise any failure in one place.
    ///
    /// Every start attempt (creation, restart, explicit start) funnels through
    /// here, so a carrier that dies before answering can never leave the row in
    /// an active `starting` state: the failure is written as `failed` with
    /// `pid = NULL`, an `agent_start_failed` event is appended, and the Agent
    /// stays retryable. A conflict on an already-running Agent is not a start
    /// failure and leaves the healthy row untouched.
    pub async fn start_stored_agent(
        &self,
        id: &str,
        initial_prompt: Option<String>,
    ) -> HubResult<AgentRecord> {
        let outcome = self.start_attempt(id, initial_prompt).await;
        if let Err(error) = &outcome {
            if error.code != "agent_running" {
                self.finalize_start_failure(id, error)?;
            }
        }
        outcome
    }

    /// Terminal projection for a failed start attempt. Idempotent: it refuses
    /// to downgrade a row that already has a live adapter.
    fn finalize_start_failure(&self, id: &str, error: &HubError) -> HubResult<()> {
        let live = self
            .runtimes
            .lock()
            .map(|runtimes| {
                runtimes
                    .get(id)
                    .map(|runtime| runtime.is_alive())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if live {
            return Ok(());
        }
        let Some(agent) = self.store.get_agent(id)? else {
            return Ok(());
        };
        let was_trying = agent.status.is_active() || agent.desired_state == DesiredState::Running;
        let patch = AgentPatch {
            status: was_trying.then_some(AgentStatus::Failed),
            desired_state: was_trying.then_some(DesiredState::Stopped),
            pid: was_trying.then_some(None),
            last_error: Some(Some(error.message.clone())),
            ..AgentPatch::default()
        };
        self.store.update_agent(id, patch)?;
        self.record(
            Some(id),
            "agent_start_failed",
            json!({"message": error.message.clone()}),
        )?;
        Ok(())
    }

    async fn start_attempt(
        &self,
        id: &str,
        initial_prompt: Option<String>,
    ) -> HubResult<AgentRecord> {
        let agent = self
            .store
            .get_agent(id)?
            .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")))?;
        {
            let runtimes = self.runtimes.lock().map_err(runtime_map_error)?;
            if runtimes.contains_key(id) {
                return Err(HubError::new(
                    "agent_running",
                    format!("agent is already running: {id}"),
                ));
            }
        }
        if let Some(authorization) = &agent.skill_authorization {
            let registry = self.skill_registry.as_ref().ok_or_else(|| {
                HubError::new("skill_registry_unavailable", "skill registry is unavailable")
            })?;
            registry.verify_sync(authorization)?;
        }
        let private_paths = self.provision(
            id,
            agent.carrier,
            agent.skill_authorization.is_some(),
        )?;
        let frozen = self.frozen_prompt(&agent);
        let launch = RuntimeLaunch {
            agent_id: agent.id.clone(),
            name: agent.name.clone(),
            cwd: agent.cwd.clone(),
            workspace_mode: agent.workspace_mode.clone(),
            carrier: agent.carrier,
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            thinking: agent.thinking,
            session_id: agent.session_id.clone(),
            session_dir: agent.session_dir.clone(),
            agent_dir: agent.agent_dir.clone(),
            logs_dir: agent.logs_dir.clone(),
            state_dir: private_paths.state_dir.clone(),
            tmp_dir: private_paths.tmp_dir.clone(),
            tools: agent.tools.clone(),
            skills: agent.skills.clone(),
            extensions: agent.extensions.clone(),
            authorization: agent.skill_authorization.clone(),
            prompt: frozen,
        };
        let log = self.ensure_log()?;
        let agent_log = AgentLog::new(
            log.clone(),
            agent.id.clone(),
            agent.carrier,
            "hub-manager",
        );
        agent_log.note(
            "runtime_start",
            Some(json!({"carrier": agent.carrier.as_str(), "sessionId": agent.session_id})),
            pihub_contracts::LogLevel::Info,
        );
        let (events, receiver) = mpsc::channel::<RuntimeEvent>(RUNTIME_EVENT_CHANNEL_CAPACITY);
        let runtime = self
            .runtime_factory
            .create(launch, events, Some(log.clone()))?;
        {
            let mut runtimes = self.runtimes.lock().map_err(runtime_map_error)?;
            runtimes.insert(agent.id.clone(), runtime.clone());
        }
        self.store.update_agent(
            id,
            AgentPatch {
                status: Some(AgentStatus::Starting),
                desired_state: Some(DesiredState::Running),
                last_error: Some(None),
                ..AgentPatch::default()
            },
        )?;
        let state = match runtime.start().await {
            Ok(state) => state,
            Err(error) => {
                let _ = runtime.stop().await;
                if let Ok(mut runtimes) = self.runtimes.lock() {
                    runtimes.remove(id);
                }
                agent_log.note(
                    "runtime_start_failed",
                    Some(json!({"message": error.message})),
                    pihub_contracts::LogLevel::Error,
                );
                return Err(error);
            }
        };
        spawn_event_pump(
            self.store.clone(),
            self.runtimes.clone(),
            runtime.clone(),
            agent.id.clone(),
            agent.carrier,
            receiver,
        );
        let resolved_model = state
            .model
            .filter(|model| !model.provider.is_empty() && !model.id.is_empty() && model.provider != "unknown" && model.id != "unknown");
        let started = self.store.update_agent(
            id,
            AgentPatch {
                status: Some(if state.is_streaming {
                    AgentStatus::Running
                } else {
                    AgentStatus::Idle
                }),
                desired_state: Some(DesiredState::Running),
                pid: Some(runtime.pid()),
                session_file: Some(state.session_file.clone()),
                provider: Some(resolved_model.as_ref().map(|model| model.provider.clone())),
                model: Some(resolved_model.as_ref().map(|model| model.id.clone())),
                thinking: Some(
                    resolved_model
                        .as_ref()
                        .and_then(|_| state.thinking)
                        .or(agent.thinking),
                ),
                ..AgentPatch::default()
            },
        )?;
        self.record(
            Some(id),
            "agent_ready",
            json!({
                "pid": runtime.pid(),
                "sessionId": state.session_id.clone().unwrap_or_else(|| agent.session_id.clone()),
            }),
        )?;
        agent_log.note(
            "agent_ready",
            Some(json!({"pid": runtime.pid()})),
            pihub_contracts::LogLevel::Info,
        );
        if let Some(prompt) = initial_prompt {
            self.send(id, RuntimeRequestKind::Prompt, &prompt).await?;
            return self
                .store
                .get_agent(id)?
                .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")));
        }
        Ok(started)
    }

    pub async fn send(
        &self,
        id: &str,
        kind: RuntimeRequestKind,
        message: &str,
    ) -> HubResult<Value> {
        if message.is_empty() {
            return Err(HubError::new("invalid_message", "message must be non-empty"));
        }
        let runtime = self.runtime(id)?;
        let response = runtime
            .request(kind, json!({"message": message}))
            .await?;
        self.record(
            Some(id),
            "command_accepted",
            json!({
                "kind": kind.as_str(),
                "responseId": response.get("id").cloned().unwrap_or(Value::Null),
            }),
        )?;
        Ok(response)
    }

    pub async fn request(&self, id: &str, kind: RuntimeRequestKind, payload: Value) -> HubResult<Value> {
        let runtime = self.runtime(id)?;
        let response = runtime.request(kind, payload).await?;
        self.record(
            Some(id),
            "command_accepted",
            json!({
                "kind": kind.as_str(),
                "responseId": response.get("id").cloned().unwrap_or(Value::Null),
            }),
        )?;
        Ok(response)
    }

    fn runtime(&self, id: &str) -> HubResult<Arc<dyn Runtime>> {
        let runtimes = self.runtimes.lock().map_err(runtime_map_error)?;
        let runtime = runtimes
            .get(id)
            .cloned()
            .ok_or_else(|| HubError::new("agent_not_running", format!("agent is not running: {id}")))?;
        if !runtime.is_alive() {
            return Err(HubError::new(
                "agent_not_running",
                format!("agent is not running: {id}"),
            ));
        }
        Ok(runtime)
    }

    pub async fn stop_agent(&self, id: &str) -> HubResult<AgentRecord> {
        let agent = self
            .store
            .get_agent(id)?
            .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")))?;
        self.store.update_agent(
            id,
            AgentPatch {
                status: Some(AgentStatus::Stopping),
                desired_state: Some(DesiredState::Stopped),
                ..AgentPatch::default()
            },
        )?;
        self.record(Some(id), "stop_requested", json!({}))?;
        let runtime = {
            let runtimes = self.runtimes.lock().map_err(runtime_map_error)?;
            runtimes.get(id).cloned()
        };
        if let Some(runtime) = runtime {
            let _ = runtime.stop().await;
            if !runtime.is_alive() {
                // Drop the dead adapter so a following restart can start a fresh
                // process instead of reporting "already running".
                if let Ok(mut runtimes) = self.runtimes.lock() {
                    runtimes.remove(id);
                }
            }
        }
        let still_alive = self
            .runtimes
            .lock()
            .map(|runtimes| {
                runtimes
                    .get(id)
                    .map(|runtime| runtime.is_alive())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !still_alive {
            self.store.update_agent(
                id,
                AgentPatch {
                    status: Some(AgentStatus::Stopped),
                    desired_state: Some(DesiredState::Stopped),
                    pid: Some(None),
                    ..AgentPatch::default()
                },
            )?;
        }
        let _ = agent;
        self.store
            .get_agent(id)?
            .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")))
    }

    pub async fn restart_agent(&self, id: &str) -> HubResult<AgentRecord> {
        if self.store.get_agent(id)?.is_none() {
            return Err(HubError::new("agent_not_found", format!("unknown agent: {id}")));
        }
        let running = {
            let runtimes = self.runtimes.lock().map_err(runtime_map_error)?;
            runtimes.contains_key(id)
        };
        if running {
            self.stop_agent(id).await?;
        }
        self.record(Some(id), "restart_requested", json!({}))?;
        self.start_stored_agent(id, None).await
    }

    pub async fn stop_all(&self) {
        let ids: Vec<String> = self
            .runtimes
            .lock()
            .map(|runtimes| runtimes.keys().cloned().collect())
            .unwrap_or_default();
        for id in ids {
            let _ = self.stop_agent(&id).await;
        }
    }

    pub fn active_count(&self) -> usize {
        self.runtimes
            .lock()
            .map(|runtimes| {
                runtimes
                    .values()
                    .filter(|runtime| runtime.is_alive() && runtime.pid().is_some())
                    .count()
            })
            .unwrap_or_default()
    }

    pub fn managed_count(&self) -> usize {
        self.runtimes
            .lock()
            .map(|runtimes| runtimes.values().filter(|runtime| runtime.is_alive()).count())
            .unwrap_or_default()
    }

    pub fn get_agent(&self, id: &str) -> HubResult<Option<AgentRecord>> {
        self.store.get_agent(id)
    }

    pub fn list_agent_records(&self, options: ListAgentsOptions) -> HubResult<Vec<AgentRecord>> {
        self.store.list_agents(options)
    }

    pub fn list_stored_events(&self, query: EventQuery) -> HubResult<Vec<StoredEvent>> {
        self.store.list_events(query)
    }

    /// 2.1 v1 route gate: an Agent with a frozen Skill authorization is a v2
    /// Agent and must be indistinguishable from a missing one on `/v1`.
    fn require_v1_visible(&self, id: &str) -> HubResult<AgentRecord> {
        let record = self
            .store
            .get_agent(id)?
            .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")))?;
        if record.skill_authorization.is_some() {
            return Err(HubError::new(
                "agent_not_found",
                format!("unknown agent: {id}"),
            ));
        }
        Ok(record)
    }

    fn view(&self, view: ApiView, record: &AgentRecord, include_text: bool) -> AgentView {
        match view {
            ApiView::V1 => AgentView::V1(PublicAgent::from(record)),
            ApiView::V2 => AgentView::V2(AgentViewV2::from_record(
                record,
                self.prompt_config_of(record, include_text),
            )),
        }
    }

    async fn skill_manager_status(&self, live: bool) -> HubResult<Value> {
        let connection = self.manager_connection.as_ref().ok_or_else(|| {
            HubError::new(
                "manager_unavailable",
                "Skill Manager connection is unavailable",
            )
        })?;
        let mut status = connection.status(live).await?;
        let connected = status
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let skills = if connected {
            list_manager_skills(connection, None)?
        } else {
            Vec::new()
        };
        if let Some(object) = status.as_object_mut() {
            object.insert("skills".into(), json!(skills));
        }
        Ok(status)
    }

    /// Exact `/v2/providers` payload when no CC Switch service is injected.
    pub fn unavailable_provider_snapshot(&self) -> ProviderSnapshot {
        ProviderSnapshot {
            switch: pihub_contracts::ProviderDiagnostics {
                available: false,
                version: None,
                binary_path: self.paths.cc_switch_cli_path.clone(),
                config_dir: self.paths.cc_switch_config_dir.clone(),
                error_code: Some("cc_switch_unavailable".to_string()),
                reason: Some("CC Switch service is unavailable".to_string()),
            },
            apps: pihub_contracts::ProviderApps {
                claude: Vec::new(),
                codex: Vec::new(),
                pi: Vec::new(),
            },
        }
    }
}

fn runtime_map_error<T>(_: T) -> HubError {
    HubError::new("agent_state_unavailable", "runtime map lock poisoned")
}

fn link_shared(source: &Path, target: &Path) -> HubResult<()> {
    if !fsutil::path_exists(source) || fsutil::path_exists(target) {
        return Ok(());
    }
    symlink(source, target).map_err(HubError::from)
}

fn resolve_cwd(cwd: Option<&Path>) -> HubResult<PathBuf> {
    let requested = match cwd {
        Some(path) => fsutil::absolute_normalized(path),
        None => std::env::current_dir().map_err(HubError::from)?,
    };
    match fsutil::stat_no_follow(&requested)? {
        Some(metadata) if metadata.is_dir() => Ok(requested),
        _ => Err(HubError::new(
            "invalid_cwd",
            format!("directory does not exist: {}", requested.display()),
        )),
    }
}

/// True while `runtime` is still the instance registered for `agent_id`.
///
/// A stopped CLI carrier can report its exit only after a replacement instance
/// is already running under the same agent id; those late signals must never be
/// attributed to the live instance.
fn with_current_instance<T>(
    runtimes: &RuntimeMap,
    agent_id: &str,
    runtime: &Arc<dyn Runtime>,
    frame: impl FnOnce(&mut HashMap<String, Arc<dyn Runtime>>) -> T,
) -> Option<T> {
    let mut guard = runtimes.lock().ok()?;
    let is_current = guard
        .get(agent_id)
        .map(|current| Arc::ptr_eq(current, runtime))
        .unwrap_or(false);
    if !is_current {
        return None;
    }
    // `frame` runs *inside* the critical section: its store writes cannot be
    // interleaved with a replacement instance being registered under the same
    // agent id, because `start_attempt` inserts under this same lock.
    Some(frame(&mut guard))
}

/// Consume bounded runtime signals and become the only event writer.
///
/// Every write is gated on this pump's own runtime instance still being the
/// registered one, so events from a replaced instance are dropped instead of
/// clobbering the live Agent's status, entry or `last_error`.
fn spawn_event_pump(
    store: Arc<dyn Store>,
    runtimes: RuntimeMap,
    runtime: Arc<dyn Runtime>,
    agent_id: String,
    carrier: Carrier,
    mut receiver: mpsc::Receiver<RuntimeEvent>,
) {
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            // An Exit ends this pump whether or not it still owns the registration.
            let terminal = matches!(event, RuntimeEvent::Exit { .. });
            let _ = with_current_instance(&runtimes, &agent_id, &runtime, |map| {
                handle_runtime_event(&store, map, &agent_id, carrier, &runtime, event)
            });
            if terminal {
                break;
            }
        }
    });
}

/// Apply one runtime frame while the runtime-map critical section is held.
fn handle_runtime_event(
    store: &Arc<dyn Store>,
    map: &mut HashMap<String, Arc<dyn Runtime>>,
    agent_id: &str,
    carrier: Carrier,
    runtime: &Arc<dyn Runtime>,
    event: RuntimeEvent,
) {
    {
            match event {
                RuntimeEvent::Event {
                    event_type,
                    payload,
                } => {
                    // Streaming deltas are progress, not history: no row, no cursor.
                    if is_ephemeral_stream_event(&payload) {
                        return;
                    }
                    // Persist the semantic event *before* projecting the status a
                    // caller can observe. 2.1 then guarantees that whoever sees a
                    // settled Agent also already has its `agent_settled` event, so
                    // the next `command_accepted` can never precede it.
                    let _ = store.append_event(NewEvent::new(
                        new_id(),
                        Some(agent_id.to_string()),
                        event_type.clone(),
                        payload,
                    ));
                    let pid = runtime.pid();
                    match event_type.as_str() {
                        "agent_start" => {
                            let _ = store.update_agent(
                                agent_id,
                                AgentPatch {
                                    status: Some(AgentStatus::Running),
                                    pid: Some(pid),
                                    ..AgentPatch::default()
                                },
                            );
                        }
                        "agent_settled" => {
                            let _ = store.update_agent(
                                agent_id,
                                AgentPatch {
                                    status: Some(AgentStatus::Idle),
                                    pid: Some(pid),
                                    ..AgentPatch::default()
                                },
                            );
                        }
                        "extension_ui_request" => {
                            let _ = store.update_agent(
                                agent_id,
                                AgentPatch {
                                    status: Some(AgentStatus::WaitingInput),
                                    ..AgentPatch::default()
                                },
                            );
                        }
                        _ => {}
                    }
                }
                RuntimeEvent::ProtocolError { message, line } => {
                    let _ = store.update_agent(
                        &agent_id,
                        AgentPatch {
                            last_error: Some(Some(format!("protocol error: {message}"))),
                            ..AgentPatch::default()
                        },
                    );
                    let _ = store.append_event(NewEvent::new(
                        new_id(),
                        Some(agent_id.to_string()),
                        "protocol_error",
                        json!({"message": message, "line": line}),
                    ));
                }
                RuntimeEvent::ProcessError { message } => {
                    let _ = store.update_agent(
                        &agent_id,
                        AgentPatch {
                            status: Some(AgentStatus::Crashed),
                            pid: Some(None),
                            last_error: Some(Some(message.clone())),
                            ..AgentPatch::default()
                        },
                    );
                    let _ = store.append_event(NewEvent::new(
                        new_id(),
                        Some(agent_id.to_string()),
                        "process_error",
                        json!({"message": message}),
                    ));
                }
                RuntimeEvent::Exit { code, signal, at } => {
                    let current = store.get_agent(&agent_id).ok().flatten();
                    let expected = current
                        .as_ref()
                        .map(|agent| {
                            agent.desired_state == DesiredState::Stopped
                                || agent.status == AgentStatus::Stopping
                        })
                        .unwrap_or(false);
                    let last_error = if expected {
                        None
                    } else {
                        Some(format!(
                            "{} process exited (code={}, signal={})",
                            AgentManager::carrier_label(carrier),
                            code.map(|value| value.to_string())
                                .unwrap_or_else(|| "null".to_string()),
                            signal.clone().unwrap_or_else(|| "null".to_string())
                        ))
                    };
                    let _ = store.update_agent(
                        &agent_id,
                        AgentPatch {
                            status: Some(if expected {
                                AgentStatus::Stopped
                            } else {
                                AgentStatus::Crashed
                            }),
                            pid: Some(None),
                            last_error: Some(last_error),
                            ..AgentPatch::default()
                        },
                    );
                    // The critical section is already held and verified; only the
                    // owner of the registration may drop it here.
                    map.remove(agent_id);
                    let _ = store.append_event(NewEvent::new(
                        new_id(),
                        Some(agent_id.to_string()),
                        "process_exit",
                        json!({
                            "code": code,
                            "signal": signal,
                            "at": at,
                            "expected": expected,
                        }),
                    ));
                }
            }
    }
}

#[async_trait::async_trait]
impl HubApi for AgentManager {
    async fn snapshot(&self, view: ApiView) -> HubResult<Snapshot> {
        let profile = self.profile.view();
        match view {
            ApiView::V1 => {
                let agents = self
                    .store
                    .list_agents(ListAgentsOptions {
                        include_stopped: true,
                        legacy_only: true,
                    })?
                    .iter()
                    .map(PublicAgent::from)
                    .collect::<Vec<_>>();
                let event_cursor = self.store.latest_cursor(true)?;
                Ok(Snapshot::V1(SnapshotV1 {
                    version: pihub_contracts::APP_VERSION.to_string(),
                    profile,
                    agents,
                    event_cursor,
                }))
            }
            ApiView::V2 => {
                let records = self.store.list_agents(ListAgentsOptions::default())?;
                let agents = records
                    .iter()
                    .map(|record| {
                        AgentViewV2::from_record(record, self.prompt_config_of(record, false))
                    })
                    .collect::<Vec<_>>();
                let event_cursor = self.store.latest_cursor(false)?;
                Ok(Snapshot::V2(SnapshotV2 {
                    version: pihub_contracts::APP_VERSION.to_string(),
                    profile,
                    agents,
                    active_count: self.active_count(),
                    managed_count: self.managed_count(),
                    event_cursor,
                }))
            }
        }
    }

    async fn list_agents(&self, include_stopped: bool) -> HubResult<Vec<AgentView>> {
        Ok(self
            .store
            .list_agents(ListAgentsOptions {
                include_stopped,
                legacy_only: true,
            })?
            .iter()
            .map(|record| AgentView::V1(PublicAgent::from(record)))
            .collect())
    }

    async fn agent(&self, view: ApiView, id: &str) -> HubResult<AgentView> {
        let record = match view {
            ApiView::V1 => self.require_v1_visible(id)?,
            ApiView::V2 => self
                .store
                .get_agent(id)?
                .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")))?,
        };
        Ok(self.view(view, &record, true))
    }

    async fn create(&self, view: ApiView, spec: AgentSpec) -> HubResult<AgentView> {
        let record = self.create_agent(spec, view == ApiView::V2).await?;
        Ok(self.view(view, &record, true))
    }

    async fn send(
        &self,
        id: &str,
        kind: RuntimeRequestKind,
        message: &str,
    ) -> HubResult<Value> {
        AgentManager::send(self, id, kind, message).await
    }

    async fn control(
        &self,
        view: ApiView,
        id: &str,
        action: ControlAction,
    ) -> HubResult<ControlResult> {
        if view == ApiView::V1 {
            self.require_v1_visible(id)?;
        }
        match action {
            ControlAction::Abort => {
                let response = self
                    .request(id, RuntimeRequestKind::Abort, json!({}))
                    .await?;
                Ok(ControlResult::Rpc(response))
            }
            ControlAction::Stop => {
                let record = self.stop_agent(id).await?;
                Ok(ControlResult::Agent(AgentView::V1(PublicAgent::from(
                    &record,
                ))))
            }
            ControlAction::Restart => {
                let record = self.restart_agent(id).await?;
                Ok(ControlResult::Agent(AgentView::V1(PublicAgent::from(
                    &record,
                ))))
            }
        }
    }

    async fn events(&self, view: ApiView, query: EventQuery) -> HubResult<Vec<StoredEvent>> {
        let mut query = query;
        if view == ApiView::V1 {
            if let Some(id) = query.agent_id.clone() {
                self.require_v1_visible(&id)?;
            }
            query.legacy_only = true;
        }
        self.store.list_events(query)
    }

    async fn carriers(&self) -> HubResult<Vec<pihub_contracts::CarrierDiagnostic>> {
        self.runtime_factory.list().await
    }

    async fn providers(&self) -> HubResult<ProviderSnapshot> {
        match &self.provider_service {
            Some(service) => service.list_all().await,
            None => Ok(self.unavailable_provider_snapshot()),
        }
    }

    async fn add_provider(&self, input: ProviderInput) -> HubResult<Provider> {
        let service = self.provider_service.as_ref().ok_or_else(|| {
            HubError::new("cc_switch_unavailable", "CC Switch service is unavailable")
        })?;
        service.add(input).await
    }

    async fn switch_provider(
        &self,
        app: pihub_contracts::ProviderApp,
        id: &str,
    ) -> HubResult<Provider> {
        let service = self.provider_service.as_ref().ok_or_else(|| {
            HubError::new("cc_switch_unavailable", "CC Switch service is unavailable")
        })?;
        service.switch(app, id).await
    }

    async fn skills(&self) -> HubResult<Vec<SkillSummary>> {
        match &self.skill_registry {
            Some(registry) => registry.list().await,
            None => Ok(Vec::new()),
        }
    }

    async fn import_skill(&self, input: SkillImportInput) -> HubResult<SkillImportResult> {
        let registry = self.skill_registry.as_ref().ok_or_else(|| {
            HubError::new("skill_registry_unavailable", "Skill registry is unavailable")
        })?;
        registry.import(input).await
    }

    async fn skill_manager(
        &self,
        action: pihub_contracts::SkillManagerAction,
        input: Value,
    ) -> HubResult<Value> {
        use pihub_contracts::SkillManagerAction;
        let connection = self.manager_connection.as_ref().ok_or_else(|| {
            HubError::new(
                "manager_unavailable",
                "Skill Manager connection is unavailable",
            )
        })?;
        match action {
            SkillManagerAction::Status => {
                let live = input.get("live").and_then(Value::as_bool).unwrap_or(false);
                self.skill_manager_status(live).await
            }
            SkillManagerAction::Connect => {
                let request: pihub_contracts::ManagerConnectInput =
                    serde_json::from_value(input).map_err(|error| {
                        HubError::new("invalid_manager_config", error.to_string())
                    })?;
                let result = connection.connect(request).await?;
                self.record(
                    None,
                    "skill_manager_connected",
                    json!({
                        "skillCount": result.get("snapshot").and_then(|snapshot| snapshot.get("skillCount")).cloned().unwrap_or(Value::Null),
                        "source": result.get("config").and_then(|config| config.get("source")).cloned().unwrap_or(Value::Null),
                    }),
                )?;
                let mut status = self.skill_manager_status(true).await?;
                if let Some(object) = status.as_object_mut() {
                    object.insert("result".into(), result);
                }
                Ok(status)
            }
            SkillManagerAction::Sync => {
                let request: pihub_contracts::ManagerSyncInput =
                    serde_json::from_value(input).unwrap_or_default();
                let result = connection.sync(request).await?;
                self.record(
                    None,
                    "skill_manager_synced",
                    json!({
                        "synced": result.get("synced").and_then(Value::as_bool).unwrap_or(false),
                        "skillCount": result.get("snapshot").and_then(|snapshot| snapshot.get("skillCount")).cloned().unwrap_or(Value::Null),
                    }),
                )?;
                let mut status = self.skill_manager_status(true).await?;
                if let Some(object) = status.as_object_mut() {
                    object.insert("result".into(), result);
                }
                Ok(status)
            }
            SkillManagerAction::Disconnect => {
                let request: pihub_contracts::ManagerDisconnectInput =
                    serde_json::from_value(input).unwrap_or_default();
                let result = connection.disconnect(request).await?;
                self.record(
                    None,
                    "skill_manager_disconnected",
                    json!({
                        "snapshotKept": result.get("snapshotKept").and_then(Value::as_bool).unwrap_or(false),
                    }),
                )?;
                let mut status = self.skill_manager_status(false).await?;
                if let Some(object) = status.as_object_mut() {
                    object.insert("result".into(), result);
                }
                Ok(status)
            }
        }
    }

    async fn prompts(&self) -> HubResult<PromptCatalog> {
        Ok(self.prompter.list())
    }
}
