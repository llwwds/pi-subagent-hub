//! HTTP contract tests against a fake `HubApi` on a real loopback socket.
//!
//! These assert the 2.1 wire behaviour the Rust server must preserve:
//! envelope shape, status codes, auth exemption, body limit, v1/v2 view
//! separation and the frozen-prompt text boundary.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use pihub_contracts::{
    AgentSpec, AgentView, AgentViewV2, ApiView, Carrier, CarrierCapabilities,
    CarrierDiagnostic, ControlAction, ControlResult, EventQuery, HubApi, HubError, HubPaths,
    HubResult, PromptCatalog, PromptChannel, PromptMode, PromptPrinciple, PromptSummary,
    Provider, ProviderApp, ProviderApps, ProviderDiagnostics, ProviderInput, ProviderSnapshot,
    PublicAgent, RuntimeRequestKind, SkillImportInput, SkillImportResult, SkillManagerAction,
    SkillRef,
    SkillPolicyView, SkillPolicyViewMode, SkillSummary, Snapshot, SnapshotV2, StoredEvent,
};
use pihub_app::http::{router, ServerState, BODY_LIMIT};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Notify;

const TOKEN: &str = "test-token-abc";
const V1_ID: &str = "legacy-agent";
const V2_ID: &str = "platform-agent";

fn public_agent(id: &str, status: &str) -> PublicAgent {
    PublicAgent {
        id: id.to_string(),
        name: format!("name-{id}"),
        cwd: "/tmp/work".into(),
        workspace_mode: "shared".into(),
        provider: Some("gw".into()),
        model: Some("model-a".into()),
        thinking: Some(pihub_contracts::ThinkingLevel::High),
        status: status.parse().unwrap(),
        desired_state: "running".parse().unwrap(),
        pid: Some(4242),
        session_id: format!("session-{id}"),
        session_file: None,
        session_dir: format!("/tmp/session-{id}").into(),
        profile_digest: "digest".into(),
        last_error: None,
        created_at: "2026-10-20T10:00:00.000Z".into(),
        updated_at: "2026-10-20T10:05:00.000Z".into(),
        tools: pihub_contracts::DEFAULT_TOOLS
            .iter()
            .map(|tool| tool.to_string())
            .collect(),
        // A platform Agent carries grant objects; the legacy fixture below keeps
        // the same shape because /v1 serialises whatever the row holds.
        skills: vec![SkillRef::Grant {
            id: "skill-a".into(),
            path: "/tmp/skills/objects/aaa".into(),
            sha256: "a".repeat(64),
        }],
        extensions: vec![],
    }
}

fn summary(with_text: bool) -> PromptSummary {
    PromptSummary {
        mode: PromptMode::Append,
        carrier: Some(Carrier::Pi),
        channel: Some(PromptChannel::Append),
        support: Some(PromptChannel::Append),
        principle_ids: vec!["first-principles".into()],
        custom_text_chars: 12,
        prompt_chars: 48,
        digest: "sha256:abcdef".into(),
        resolved_at: Some("2026-10-20T10:00:00.000Z".into()),
        custom_text: with_text.then(|| "custom-text!".to_string()),
        prompt: with_text.then(|| "FROZEN-PROMPT-BODY".to_string()),
    }
}

fn view_v2(id: &str, with_text: bool) -> AgentViewV2 {
    AgentViewV2 {
        agent: public_agent(id, "idle"),
        carrier: Carrier::Pi,
        skill_policy: SkillPolicyView {
            mode: SkillPolicyViewMode::All,
            ids: vec!["skill-a".into()],
        },
        skill_ids: vec!["skill-a".into()],
        skill_digest: Some("sha256:skill".into()),
        prompt_config: Some(summary(with_text)),
    }
}

#[derive(Default)]
struct FakeApi {
    created: std::sync::Mutex<Vec<AgentSpec>>,
    manager_actions: std::sync::Mutex<Vec<String>>,
    /// Let the panel tests exercise realistic populations and statuses without
    /// creating real Agent processes or touching the daemon's database.
    snapshot_agents: std::sync::Mutex<Option<Vec<AgentViewV2>>>,
    /// When set, every snapshot request answers with the *other* view shape,
    /// which exercises the app's view-mismatch handling.
    flip_snapshot_view: std::sync::Mutex<bool>,
    /// Provider inputs as they arrive from the panel form / API.
    provider_inputs: std::sync::Mutex<Vec<ProviderInput>>,
    /// Frozen-prompt variants exercised by the label tests.
    prompt_none: std::sync::Mutex<bool>,
    prompt_legacy: std::sync::Mutex<bool>,
    prompt_empty_digest: std::sync::Mutex<bool>,
    /// Freeze the resolved authorization as `only` with a single grant.
    skill_only: std::sync::Mutex<bool>,
    /// When set the fake behaves like core with the V1-GATE fix: the legacy
    /// lookup itself refuses a platform Agent.
    core_gates_v1: std::sync::Mutex<bool>,
    /// Internal error code `create` should fail with, to prove the wire mapping.
    create_error: std::sync::Mutex<Option<String>>,
}

impl FakeApi {
    /// The v2 view for `id`, with the test's prompt/skill variants applied.
    fn view(&self, id: &str, with_text: bool) -> AgentViewV2 {
        let mut view = view_v2(id, with_text);
        if *self.prompt_none.lock().unwrap() {
            view.prompt_config = Some(PromptSummary {
                mode: PromptMode::None,
                carrier: Some(Carrier::Pi),
                channel: None,
                support: None,
                principle_ids: Vec::new(),
                custom_text_chars: 0,
                prompt_chars: 0,
                digest: "sha256:3c78f0d1e2a4b5c6d7e8f90123456789abcdef0123456789abcdef0123456789"
                    .into(),
                resolved_at: Some("2026-10-20T10:00:00.000Z".into()),
                custom_text: None,
                prompt: None,
            });
        }
        if *self.prompt_legacy.lock().unwrap() {
            view.prompt_config = None;
        }
        if *self.prompt_empty_digest.lock().unwrap() {
            let mut summary = summary(with_text);
            summary.digest = String::new();
            view.prompt_config = Some(summary);
        }
        if *self.skill_only.lock().unwrap() {
            view.skill_policy = SkillPolicyView {
                mode: SkillPolicyViewMode::Only,
                ids: vec!["skill-a".into()],
            };
            view.skill_ids = vec!["skill-a".into()];
            view.skill_digest = Some("sha256:skill".into());
        }
        view
    }
}

#[pihub_contracts::async_trait]
impl HubApi for FakeApi {
    async fn snapshot(&self, view: ApiView) -> HubResult<Snapshot> {
        let flipped = *self.flip_snapshot_view.lock().unwrap();
        let view = if flipped {
            match view {
                ApiView::V1 => ApiView::V2,
                ApiView::V2 => ApiView::V1,
            }
        } else {
            view
        };
        match view {
            ApiView::V2 => {
                let override_agents = self.snapshot_agents.lock().unwrap().clone();
                let has_override = override_agents.is_some();
                let agents = override_agents.unwrap_or_else(|| {
                    vec![self.view(V2_ID, false), self.view("platform-agent-2", false)]
                });
                let active_count = if has_override {
                    agents.iter().filter(|view| view.agent.status.is_active()).count()
                } else {
                    2
                };
                let managed_count = if has_override { agents.len() } else { 3 };
                Ok(Snapshot::V2(SnapshotV2 {
                    version: pihub_contracts::APP_VERSION.to_string(),
                    profile: pihub_contracts::ProfileView {
                        name: "default".into(),
                        digest: "profile-digest".into(),
                        tools: vec!["read".into(), "bash".into()],
                    },
                    agents,
                    active_count,
                    managed_count,
                    event_cursor: 42,
                }))
            }
            ApiView::V1 => Ok(Snapshot::V1(pihub_contracts::SnapshotV1 {
                version: pihub_contracts::APP_VERSION.to_string(),
                profile: pihub_contracts::ProfileView {
                    name: "default".into(),
                    digest: "profile-digest".into(),
                    tools: vec!["read".into()],
                },
                agents: vec![public_agent(V1_ID, "idle")],
                event_cursor: 7,
            })),
        }
    }

    async fn list_agents(&self, _include_stopped: bool) -> HubResult<Vec<AgentView>> {
        Ok(vec![AgentView::V1(public_agent(V1_ID, "idle"))])
    }

    async fn agent(&self, view: ApiView, id: &str) -> HubResult<AgentView> {
        match view {
            ApiView::V2 => {
                if id == V2_ID || id == "platform-agent-2" {
                    Ok(AgentView::V2(self.view(id, true)))
                } else if id == V1_ID {
                    // The platform view serves pre-platform Agents too, with a
                    // `legacy` skill policy and no frozen authorization.
                    let mut view = view_v2(id, true);
                    view.skill_policy = SkillPolicyView {
                        mode: SkillPolicyViewMode::Legacy,
                        ids: Vec::new(),
                    };
                    view.skill_ids = Vec::new();
                    view.skill_digest = None;
                    view.prompt_config = None;
                    Ok(AgentView::V2(view))
                } else {
                    Err(HubError::new(
                        "agent_not_found",
                        format!("unknown agent: {id}"),
                    ))
                }
            }
            ApiView::V1 => {
                // Two core shapes must both end in a 404 for a platform Agent:
                // - with the V1-GATE fix, the legacy lookup refuses it here;
                // - without it, the legacy lookup resolves and the app refuses.
                let platform = id == V2_ID || id == "platform-agent-2";
                let gated = *self.core_gates_v1.lock().unwrap();
                if platform && gated {
                    Err(HubError::new(
                        "agent_not_found",
                        format!("unknown agent: {id}"),
                    ))
                } else if id == V1_ID || platform {
                    Ok(AgentView::V1(public_agent(id, "idle")))
                } else {
                    Err(HubError::new(
                        "agent_not_found",
                        format!("unknown agent: {id}"),
                    ))
                }
            }
        }
    }

    async fn create(&self, _view: ApiView, spec: AgentSpec) -> HubResult<AgentView> {
        self.created.lock().unwrap().push(spec);
        if let Some(code) = self.create_error.lock().unwrap().clone() {
            return Err(HubError::new(code, "内部中文消息保持不变"));
        }
        Ok(AgentView::V2(self.view(V2_ID, false)))
    }

    async fn send(&self, id: &str, kind: RuntimeRequestKind, message: &str) -> HubResult<Value> {
        Ok(json!({ "id": id, "kind": kind.as_str(), "message": message }))
    }

    async fn control(
        &self,
        view: ApiView,
        id: &str,
        action: ControlAction,
    ) -> HubResult<ControlResult> {
        // Mirrors core::manager::control(): the requested view is also an entity
        // gate, so V1 refuses a platform Agent, and an unknown id always fails.
        if !matches!(id, V1_ID | V2_ID | "platform-agent-2") {
            return Err(HubError::new(
                "agent_not_found",
                format!("unknown agent: {id}"),
            ));
        }
        if view == ApiView::V1 && (id == V2_ID || id == "platform-agent-2") {
            return Err(HubError::new(
                "agent_not_found",
                format!("unknown agent: {id}"),
            ));
        }
        match action {
            ControlAction::Abort => Ok(ControlResult::Rpc(json!({ "aborted": true }))),
            _ => Ok(ControlResult::Agent(
                AgentView::V1(public_agent(id, "stopped")).into(),
            )),
        }
    }

    async fn events(&self, _view: ApiView, query: EventQuery) -> HubResult<Vec<StoredEvent>> {
        let payload = json!({ "pid": 4242, "sessionId": "session-platform-agent" });
        Ok(vec![StoredEvent {
            cursor: query.after + 1,
            event_id: "evt-1".into(),
            agent_id: query.agent_id.clone().or(Some(V2_ID.to_string())),
            event_type: "agent_ready".into(),
            payload_json: payload.to_string(),
            payload,
            created_at: "2026-10-20T10:00:01.000Z".into(),
        }])
    }

    async fn carriers(&self) -> HubResult<Vec<CarrierDiagnostic>> {
        Ok(vec![
            CarrierDiagnostic {
                id: Carrier::Pi,
                display_name: "Pi Agent CLI".into(),
                capabilities: CarrierCapabilities {
                    messages: vec![
                        RuntimeRequestKind::Prompt,
                        RuntimeRequestKind::Steer,
                        RuntimeRequestKind::FollowUp,
                        RuntimeRequestKind::Abort,
                    ],
                    events: true,
                    skill_loading_selection: true,
                    skill_isolation: true,
                    system_prompt: Some(PromptChannel::Append),
                },
                available: true,
                reason: None,
            },
            CarrierDiagnostic {
                id: Carrier::Codex,
                display_name: "Codex CLI".into(),
                capabilities: CarrierCapabilities {
                    messages: vec![RuntimeRequestKind::Prompt, RuntimeRequestKind::Abort],
                    events: true,
                    skill_loading_selection: true,
                    skill_isolation: true,
                    system_prompt: Some(PromptChannel::Developer),
                },
                available: true,
                reason: None,
            },
        ])
    }

    async fn providers(&self) -> HubResult<ProviderSnapshot> {
        Ok(ProviderSnapshot {
            switch: ProviderDiagnostics {
                available: true,
                version: Some("5.10.5".into()),
                binary_path: "/tmp/cc-switch".into(),
                config_dir: "/tmp/cc-switch/data".into(),
                error_code: None,
                reason: None,
            },
            apps: ProviderApps {
                claude: vec![Provider {
                    id: "claude-gw".into(),
                    name: "Claude Gateway".into(),
                    base_url: Some("https://api.example.com/v1".into()),
                    model_list: vec!["claude-a".into()],
                    default_model: Some("claude-a".into()),
                    api_format: Some("anthropic".into()),
                    auth_mode: Some(pihub_contracts::AuthMode::ApiKey),
                    configured: true,
                    ready: true,
                    is_current: Some(true),
                    is_default: true,
                }],
                codex: vec![],
                pi: vec![Provider {
                    id: "pi-gw".into(),
                    name: "Pi Gateway".into(),
                    base_url: Some("https://pi.example.com/v1".into()),
                    model_list: vec!["model-a".into()],
                    default_model: Some("model-a".into()),
                    api_format: Some("openai-completions".into()),
                    auth_mode: None,
                    configured: true,
                    ready: true,
                    is_current: None,
                    is_default: false,
                }],
            },
        })
    }

    async fn add_provider(&self, input: ProviderInput) -> HubResult<Provider> {
        self.provider_inputs.lock().unwrap().push(input.clone());
        Ok(Provider {
            id: input.id,
            name: input.name,
            base_url: Some(input.base_url),
            model_list: input.model_list,
            default_model: Some(input.default_model),
            api_format: input.api_format,
            auth_mode: input.auth_mode,
            configured: true,
            ready: true,
            is_current: Some(false),
            is_default: false,
        })
    }

    async fn switch_provider(&self, app: ProviderApp, id: &str) -> HubResult<Provider> {
        if app == ProviderApp::Pi {
            return Err(HubError::new(
                "CC_SWITCH_UNSUPPORTED",
                "pi providers are chosen per Agent",
            ));
        }
        Ok(Provider {
            id: id.into(),
            name: id.into(),
            base_url: None,
            model_list: vec![],
            default_model: None,
            api_format: None,
            auth_mode: None,
            configured: true,
            ready: true,
            is_current: Some(true),
            is_default: true,
        })
    }

    async fn skills(&self) -> HubResult<Vec<SkillSummary>> {
        Ok(vec![SkillSummary {
            id: "skill-a".into(),
            name: "Skill A".into(),
            version: "1".into(),
            digest: "sha256:skill-a".into(),
            current_sha256: "a".repeat(64),
            source: "/tmp/skills/a".into(),
            imported_at: "2026-10-20T09:00:00.000Z".into(),
            versions: vec![],
        }])
    }

    async fn import_skill(&self, input: SkillImportInput) -> HubResult<SkillImportResult> {
        Ok(SkillImportResult {
            id: input.id,
            sha256: "b".repeat(64),
            source: input.source_path.display().to_string(),
            imported_at: "2026-10-20T09:30:00.000Z".into(),
            file_count: 2,
            byte_count: 128,
            path: "/tmp/objects/b".into(),
        })
    }

    async fn skill_manager(&self, action: SkillManagerAction, input: Value) -> HubResult<Value> {
        self.manager_actions
            .lock()
            .unwrap()
            .push(action.as_str().to_string());
        Ok(json!({
            "connected": false,
            "action": action.as_str(),
            "input": input,
            "snapshot": { "syncedAt": "2026-10-20T09:00:00.000Z", "digest": "sha256:cat" },
            "manager": { "available": true, "version": "0.4.1", "cliPath": "/tmp/skills-manager" },
            "drift": { "added": [], "removed": [] },
        }))
    }

    async fn prompts(&self) -> HubResult<PromptCatalog> {
        Ok(PromptCatalog {
            principles: vec![PromptPrinciple {
                id: "first-principles".into(),
                title: "第一性原理".into(),
                text: "从根本约束推导".into(),
            }],
            default_principle_ids: vec!["first-principles".into()],
        })
    }
}

struct Harness {
    base: String,
    stop: Arc<Notify>,
    api: Arc<FakeApi>,
    _home: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.notify_waiters();
        self.handle.abort();
    }
}

async fn start() -> Harness {
    start_with(Arc::new(FakeApi::default())).await
}

async fn start_with(api: Arc<FakeApi>) -> Harness {
    let home = tempfile::tempdir().unwrap();
    let paths = HubPaths::resolve(home.path(), "/nonexistent/app-root");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(Notify::new());
    let state = Arc::new(ServerState::with_stop(
        api.clone(),
        paths,
        TOKEN,
        stop.clone(),
    ));
    let server = router(state);
    let serve_stop = stop.clone();
    let handle = tokio::spawn(async move {
        let _ = pihub_app::serve_listener(server, listener, serve_stop).await;
    });
    Harness {
        base: format!("http://127.0.0.1:{port}"),
        stop,
        api,
        _home: home,
        handle,
    }
}

fn client() -> reqwest::Client {
    // Do not follow redirects: the panel's form handlers answer with 303 and
    // the test asserts on that status, not on the follow-up page.
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn get(harness: &Harness, path: &str) -> (StatusCode, Value) {
    let response = client()
        .get(format!("{}{path}", harness.base))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    (status, body)
}

async fn post(harness: &Harness, path: &str, body: Value) -> (StatusCode, Value) {
    let response = client()
        .post(format!("{}{path}", harness.base))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let payload: Value = response.json().await.unwrap();
    (status, payload)
}

#[tokio::test]
async fn health_is_unauthenticated_and_matches_contract() {
    let harness = start().await;
    let response = client()
        .get(format!("{}/v1/health", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["error"], Value::Null);
    assert_eq!(payload["data"]["name"], "pi-subagent-hub");
    assert_eq!(payload["data"]["apiVersion"], 1);
    assert!(payload["data"]["pid"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn api_requires_the_control_token() {
    let harness = start().await;
    let response = client()
        .get(format!("{}/v2/snapshot", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let payload: Value = response.json().await.unwrap();
    assert_eq!(
        payload,
        json!({
            "ok": false,
            "data": null,
            "error": {
                "code": "unauthorized",
                "message": "valid local control token required",
                "retryable": false,
                "details": null,
            },
        })
    );
}

#[tokio::test]
async fn v2_snapshot_carries_summaries_without_prompt_text() {
    let harness = start().await;
    let (status, payload) = get(&harness, "/v2/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    let snapshot = &payload["data"];
    assert_eq!(snapshot["eventCursor"], 42);
    assert_eq!(snapshot["activeCount"], 2);
    assert_eq!(snapshot["managedCount"], 3);
    let agent = &snapshot["agents"][0];
    assert_eq!(agent["id"], V2_ID);
    assert_eq!(agent["carrier"], "pi");
    assert_eq!(agent["skillPolicy"]["mode"], "all");
    assert_eq!(agent["skillIds"][0], "skill-a");
    assert_eq!(agent["skillDigest"], "sha256:skill");
    assert_eq!(agent["promptConfig"]["mode"], "append");
    assert!(agent["promptConfig"].get("prompt").is_none());
    assert!(agent["promptConfig"].get("customText").is_none());
    // Raw columns never reach the wire.
    assert!(agent.get("agent_dir").is_none());
    assert!(agent.get("prompt_config_json").is_none());
}

#[tokio::test]
async fn v2_snapshot_reports_a_view_mismatch_instead_of_fabricating() {
    let api = Arc::new(FakeApi::default());
    *api.flip_snapshot_view.lock().unwrap() = true;
    let harness = start_with(api.clone()).await;

    let (status, payload) = get(&harness, "/v2/snapshot").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["error"]["code"], "unsupported_view");
    assert!(payload["data"].is_null());

    // The legacy route still narrows losslessly for the same core mismatch.
    let (status, payload) = get(&harness, "/v1/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"]["agents"][0]["id"], V2_ID);
    assert_eq!(payload["data"]["agents"][0]["workspace_mode"], "shared");
    assert!(payload["data"]["agents"][0].get("carrier").is_none());
}

#[tokio::test]
async fn v2_detail_returns_the_frozen_prompt_body() {
    let harness = start().await;
    let (status, payload) = get(&harness, &format!("/v2/agents/{V2_ID}")).await;
    assert_eq!(status, StatusCode::OK);
    let agent = &payload["data"];
    assert_eq!(agent["id"], V2_ID);
    assert_eq!(agent["promptConfig"]["prompt"], "FROZEN-PROMPT-BODY");
    assert_eq!(agent["promptConfig"]["customText"], "custom-text!");
}

#[tokio::test]
async fn v1_detail_has_no_platform_or_raw_fields() {
    let harness = start().await;
    let (status, payload) = get(&harness, &format!("/v1/agents/{V1_ID}")).await;
    assert_eq!(status, StatusCode::OK);
    let agent = &payload["data"];
    assert_eq!(agent["id"], V1_ID);
    assert_eq!(agent["workspace_mode"], "shared");
    // 2.1 spreads the hydrated columns and drops only their `*_json` twins.
    assert_eq!(
        agent["tools"],
        json!(["read", "bash", "edit", "write", "grep", "find", "ls"])
    );
    assert!(agent["skills"].is_array(), "v1 keeps the hydrated skills");
    assert_eq!(agent["skills"][0]["id"], "skill-a");
    assert!(agent["extensions"].is_array());
    for absent in [
        "promptConfig",
        "carrier",
        "skillPolicy",
        "skillIds",
        "agent_dir",
        "logs_dir",
        "skills_json",
        "prompt_config_json",
        "skill_authorization_json",
    ] {
        assert!(
            agent.get(absent).is_none(),
            "v1 detail must not expose {absent}"
        );
    }
}

#[tokio::test]
async fn events_carry_the_raw_payload_json() {
    let harness = start().await;
    let (status, payload) = get(&harness, &format!("/v2/agents/{V2_ID}/events")).await;
    assert_eq!(status, StatusCode::OK);
    let event = &payload["data"][0];
    assert_eq!(event["event_type"], "agent_ready");
    assert!(event["payload"].is_object(), "parsed projection stays");
    let raw = event["payload_json"]
        .as_str()
        .expect("2.1 returns the stored payload_json text too");
    let reparsed: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(reparsed, event["payload"]);
}

#[tokio::test]
async fn v1_refuses_a_v2_agent() {
    // Even though the legacy view resolves the row, a frozen skill
    // authorization makes it invisible in the /v1 namespace.
    let harness = start().await;
    let (status, payload) = get(&harness, &format!("/v1/agents/{V2_ID}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["error"]["code"], "agent_not_found");
    assert_eq!(payload["data"], Value::Null);
    assert_eq!(
        payload["error"]["message"],
        format!("unknown agent: {V2_ID}")
    );
}

#[tokio::test]
async fn handler_failures_use_the_request_failed_wire_code() {
    // 2.1 wrapped every handler throw in `400 request_failed`; the internal code
    // (here CC_SWITCH_UNSUPPORTED) must not leak into the envelope, while the
    // message stays intact.
    let harness = start().await;
    let (status, payload) = post(&harness, "/v2/providers/pi/some-provider/switch", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["error"]["code"], "request_failed");
    assert_eq!(
        payload["error"]["message"],
        "pi providers are chosen per Agent"
    );
    assert_eq!(payload["error"]["retryable"], false);
}

#[tokio::test]
async fn domain_errors_are_wrapped_as_request_failed_with_the_message_kept() {
    // Parity review O_error_code_mapping: core/runtime domain codes must not
    // become the wire code, and the message must pass through untouched so the
    // owner of the text (core/runtime) keeps control of its language.
    for domain_code in [
        "CC_SWITCH_UNSUPPORTED",
        "carrier_unavailable",
        "invalid_prompt_config",
    ] {
        let api = Arc::new(FakeApi::default());
        *api.create_error.lock().unwrap() = Some(domain_code.to_string());
        let harness = start_with(api).await;

        let (status, payload) = post(
            &harness,
            "/v2/agents",
            json!({ "name": "x", "cwd": "/tmp", "carrier": "pi" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "code {domain_code}");
        assert_eq!(
            payload["error"]["code"], "request_failed",
            "domain code {domain_code} must not reach the wire"
        );
        assert_eq!(
            payload["error"]["message"], "内部中文消息保持不变",
            "message text is owned by core/runtime and must pass through"
        );
        assert_eq!(payload["error"]["retryable"], false);
    }
}

#[tokio::test]
async fn agent_not_found_keeps_its_own_wire_code() {
    // The one exception 2.1 preserved is the 404 agent_not_found envelope.
    let harness = start().await;
    let (status, payload) = get(&harness, "/v2/agents/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["error"]["code"], "agent_not_found");
}

#[tokio::test]
async fn v1_send_refuses_a_platform_agent_when_core_gates_the_legacy_view() {
    // `HubApi::send()` has no `ApiView`, so the route must do the gate itself.
    let api = Arc::new(FakeApi::default());
    *api.core_gates_v1.lock().unwrap() = true;
    let harness = start_with(api).await;

    for action in ["prompt", "steer", "follow-up"] {
        let (status, payload) = post(
            &harness,
            &format!("/v1/agents/{V2_ID}/{action}"),
            json!({ "message": "must not be delivered" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "v1 {action}");
        assert_eq!(payload["error"]["code"], "agent_not_found", "v1 {action}");
        assert_eq!(payload["data"], Value::Null);
    }
}

#[tokio::test]
async fn v1_send_still_works_for_a_legacy_agent() {
    // With and without core's gate, a pre-platform Agent keeps accepting
    // v1 prompt/steer/follow-up.
    for core_gates in [true, false] {
        let api = Arc::new(FakeApi::default());
        *api.core_gates_v1.lock().unwrap() = core_gates;
        let harness = start_with(api).await;

        for (action, expected_kind) in [
            ("prompt", "prompt"),
            ("steer", "steer"),
            ("follow-up", "follow_up"),
        ] {
            let (status, payload) = post(
                &harness,
                &format!("/v1/agents/{V1_ID}/{action}"),
                json!({ "message": "hello legacy" }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "v1 {action} (core_gates={core_gates})");
            assert_eq!(payload["data"]["id"], V1_ID);
            assert_eq!(payload["data"]["kind"], expected_kind);
            assert_eq!(payload["data"]["message"], "hello legacy");
        }
    }
}

#[tokio::test]
async fn v1_lifecycle_routes_also_refuse_a_v2_agent() {
    let harness = start().await;
    for action in ["prompt", "steer", "follow-up", "abort", "stop", "restart"] {
        let (status, payload) = post(
            &harness,
            &format!("/v1/agents/{V2_ID}/{action}"),
            json!({ "message": "hi" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "action {action}");
        assert_eq!(payload["error"]["code"], "agent_not_found", "action {action}");
    }
    let (status, payload) = get(&harness, &format!("/v1/agents/{V2_ID}/events")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["error"]["code"], "agent_not_found");
}

#[tokio::test]
async fn unknown_agent_and_unknown_route_use_distinct_codes() {
    let harness = start().await;
    let (status, payload) = get(&harness, "/v2/agents/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["error"]["code"], "agent_not_found");
    assert_eq!(payload["error"]["message"], "unknown agent: nope");

    let (status, payload) = get(&harness, "/v2/not-a-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["error"]["code"], "not_found");
    assert_eq!(payload["error"]["message"], "route not found");
}

#[tokio::test]
async fn body_over_one_mib_is_rejected_with_request_failed() {
    let harness = start().await;
    let huge = "x".repeat(BODY_LIMIT + 16);
    let body = json!({ "name": huge }).to_string();
    let response = client()
        .post(format!("{}/v2/agents", harness.base))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["code"], "request_failed");
    assert_eq!(payload["error"]["message"], "request body exceeds 1 MiB");
}

#[tokio::test]
async fn create_provider_and_agent_return_201() {
    let harness = start().await;
    let (status, payload) = post(
        &harness,
        "/v2/providers",
        json!({
            "app": "codex",
            "id": "team-api",
            "name": "Team API",
            "baseUrl": "https://api.example.com/v1",
            "apiKey": "secret",
            "modelList": ["model-a"],
            "defaultModel": "model-a",
            "apiFormat": "responses"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(payload["data"]["id"], "team-api");
    assert!(payload["data"].get("apiKey").is_none());

    let (status, payload) = post(
        &harness,
        "/v2/agents",
        json!({
            "name": "reviewer",
            "cwd": "/tmp/work",
            "carrier": "pi",
            "skillPolicy": { "mode": "all" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(payload["data"]["id"], V2_ID);
    // POST /v2/agents answers with the detail view.
    assert_eq!(payload["data"]["promptConfig"]["prompt"], "FROZEN-PROMPT-BODY");
    assert_eq!(harness.api.created.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn v2_lifecycle_routes_delegate_and_keep_the_legacy_stop_shape() {
    let harness = start().await;
    let (status, payload) = post(
        &harness,
        &format!("/v2/agents/{V2_ID}/prompt"),
        json!({ "message": "go" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"]["kind"], "prompt");
    assert_eq!(payload["data"]["message"], "go");

    let (status, payload) = post(&harness, &format!("/v2/agents/{V2_ID}/abort"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"]["aborted"], true);

    let (status, payload) = post(&harness, &format!("/v2/agents/{V2_ID}/stop"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    // 2.1 wraps v2 stop in the legacy Agent shape.
    assert_eq!(payload["data"]["id"], V2_ID);
    assert!(payload["data"].get("promptConfig").is_none());
    assert!(payload["data"].get("carrier").is_none());
}

#[tokio::test]
async fn v2_stop_and_restart_succeed_for_a_platform_agent() {
    // Regression: passing ApiView::V1 here tripped core's legacy-only gate, so
    // every /v2 Agent answered 404 agent_not_found on stop/restart.
    let harness = start().await;
    for action in ["stop", "restart"] {
        let (status, payload) =
            post(&harness, &format!("/v2/agents/{V2_ID}/{action}"), json!({})).await;
        assert_eq!(status, StatusCode::OK, "v2 {action} must not be gated as v1");
        assert_eq!(payload["data"]["id"], V2_ID, "legacy shape keeps the id");
        // The legacy projection is preserved: no v2-only fields leak.
        for absent in ["skillPolicy", "skillIds", "skillDigest", "promptConfig", "carrier"] {
            assert!(
                payload["data"].get(absent).is_none(),
                "v2 {action} must answer in the legacy shape, found {absent}"
            );
        }
        assert_eq!(payload["data"]["workspace_mode"], "shared");
    }
}

#[tokio::test]
async fn dashboard_lifecycle_actions_succeed_for_a_platform_agent() {
    // The panel manages platform Agents, so its control calls must not apply the
    // legacy-only gate: a success redirect carries `toast`, a refusal carries
    // `error`.
    let harness = start().await;
    for action in ["restart", "stop", "abort"] {
        let response = client()
            .post(format!("{}/ui/agents/{V2_ID}/{action}", harness.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("_token={TOKEN}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "ui {action}");
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            location.contains("toast="),
            "ui {action} should report success, got {location}"
        );
        assert!(
            !location.contains("error="),
            "ui {action} must not report an error for a platform Agent, got {location}"
        );
        // The success toast text is URL-encoded into the redirect.
        assert!(
            location.contains("%E6%8C%87%E4%BB%A4%E5%B7%B2%E6%8E%A5%E6%94%B6"),
            "expected the '指令已接收' toast, got {location}"
        );
    }
}

#[tokio::test]
async fn v1_restart_of_a_platform_agent_stays_404() {
    // The legacy namespace keeps its own gate.
    let harness = start().await;
    let (status, payload) = post(&harness, &format!("/v1/agents/{V2_ID}/restart"), json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["error"]["code"], "agent_not_found");
}

#[tokio::test]
async fn v2_stop_and_restart_of_an_unknown_agent_are_404() {
    let harness = start().await;
    for action in ["stop", "restart"] {
        let (status, payload) =
            post(&harness, &format!("/v2/agents/ghost/{action}"), json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "v2 {action}");
        assert_eq!(payload["error"]["code"], "agent_not_found", "v2 {action}");
        assert_eq!(payload["error"]["message"], "unknown agent: ghost");
    }
}

#[tokio::test]
async fn manager_and_carrier_routes_are_reachable() {
    let harness = start().await;
    let (status, payload) = get(&harness, "/v2/skills/manager").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"]["action"], "status");
    assert_eq!(payload["data"]["input"]["live"], false);

    let (status, payload) = post(&harness, "/v2/skills/manager/connect", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"]["action"], "connect");

    let (status, payload) = get(&harness, "/v2/carriers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"][0]["displayName"], "Pi Agent CLI");
    assert_eq!(payload["data"][0]["label"], "Pi Agent CLI");
    assert_eq!(payload["data"][0]["capabilities"]["systemPrompt"], "append");

    let (status, payload) = get(&harness, "/v2/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"]["switch"]["available"], true);
    assert_eq!(payload["data"]["apps"]["claude"][0]["authMode"], "api-key");
    assert_eq!(payload["data"]["apps"]["pi"][0]["isCurrent"], Value::Null);

    let (status, payload) = get(&harness, "/v2/prompts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"]["principles"][0]["id"], "first-principles");

    let (status, payload) = get(&harness, "/v2/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"][0]["currentSha256"].as_str().unwrap().len(), 64);

    let (status, payload) = post(
        &harness,
        "/v2/skills/import",
        json!({ "id": "skill-b", "sourcePath": "/tmp/skills/b" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(payload["data"]["id"], "skill-b");
}

#[tokio::test]
async fn daemon_stop_answers_then_signals_shutdown() {
    let harness = start().await;
    let (status, payload) = post(&harness, "/v1/daemon/stop", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["data"], json!({ "stopping": true }));

    let stop = harness.stop.clone();
    let notified = tokio::time::timeout(std::time::Duration::from_secs(2), stop.notified()).await;
    assert!(notified.is_ok(), "stop signal should fire after the response");
}

#[tokio::test]
async fn panel_page_injects_token_and_renders_all_views() {
    let harness = start().await;
    let response = client()
        .get(format!("{}/", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    let csp = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("script-src 'self'"));
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    let html = response.text().await.unwrap();

    // The control token is embedded for the panel's own forms, like the 2.1
    // `<meta name="pihub-token">`.
    assert!(html.contains(&format!("content=\"{TOKEN}\"")));
    // The panel brand tracks the contract version, so a bump without a panel
    // update would be caught here instead of shipping a stale label.
    assert!(html.contains(pihub_contracts::APP_VERSION));
    assert!(html.contains("href=\"/styles.css\""));
    assert!(html.contains("src=\"/app.js\""));
    // Three views and both dialogs are rendered server-side.
    assert!(html.contains("id=\"agents-view\""));
    assert!(html.contains("id=\"skills-view\""));
    assert!(html.contains("id=\"providers-view\""));
    assert!(html.contains("id=\"create-dialog\""));
    assert!(html.contains("id=\"provider-dialog\""));
    // Agent cards link to the detail selection.
    assert!(html.contains(&format!("/?view=agents&agent={V2_ID}")));
    // Provider literals are preserved.
    // `API_FORMATS` / `CLAUDE_AUTH_MODES` literals survive as option values.
    for format in [
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
        "responses",
        "anthropic",
        "api-key",
        "bearer",
    ] {
        assert!(
            html.contains(&format!("value=\"{format}\"")),
            "panel must offer the {format} choice"
        );
    }
    assert!(html.contains("x-api-key"));
    // Skills manager block stays explicit about manual connection.
    assert!(html.contains("Skills Manager"));
    assert!(html.contains("/ui/skills/manager/connect"));
}

#[tokio::test]
async fn panel_board_exposes_real_agents_beyond_the_old_activity_limit() {
    let api = Arc::new(FakeApi::default());
    let agents = (0..65)
        .map(|index| {
            let mut view = view_v2(&format!("fleet-{index:02}"), false);
            let status = match index {
                0 => "running",
                1 => "waiting_input",
                2 => "failed",
                3 => "stopped",
                _ => "idle",
            };
            view.agent.status = status.parse().unwrap();
            if !view.agent.status.is_active() {
                view.agent.pid = None;
            }
            view
        })
        .collect();
    *api.snapshot_agents.lock().unwrap() = Some(agents);
    let harness = start_with(api).await;

    let html = client()
        .get(format!("{}/?view=agents", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Each node must retain its actual status and link to the correct Agent;
    // in particular, the old 48-node activity summary must not truncate the
    // main board. The all/active scope itself is exercised in a browser.
    for (id, tone) in [
        ("fleet-00", "running"),
        ("fleet-01", "waiting_input"),
        ("fleet-02", "failed"),
        ("fleet-03", "stopped"),
        ("fleet-64", "idle"),
    ] {
        let needle = format!("data-agent-id=\"{id}\"");
        let index = html.find(&needle).unwrap_or_else(|| panic!("missing node for {id}"));
        let start = html[..index].rfind('<').expect("node start tag");
        let end = index + html[index..].find('>').expect("node end tag");
        let node = &html[start..end];
        assert!(node.contains(&format!("data-tone=\"{tone}\"")), "wrong status on {node}");
        assert!(node.contains(&format!("agent={id}")), "wrong destination on {node}");
    }

    let filtered = client()
        .get(format!("{}/?view=agents&q=fleet-64", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(filtered.contains("data-agent-id=\"fleet-64\""));
    assert!(!filtered.contains("data-agent-id=\"fleet-00\""));
}

#[tokio::test]
async fn panel_offers_independent_board_and_atmosphere_controls() {
    let harness = start().await;
    let html = client()
        .get(format!("{}/?view=agents", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The two independent controls make all six combinations reachable.
    for (preference, value) in [
        ("skin", "cloud"),
        ("skin", "flow"),
        ("atmosphere", "subtle"),
        ("atmosphere", "balanced"),
        ("atmosphere", "immersive"),
    ] {
        assert!(
            html.split('>').any(|tag| {
                tag.contains(&format!("data-preference=\"{preference}\""))
                    && tag.contains(&format!("data-value=\"{value}\""))
                    && tag.contains("aria-pressed=")
            }),
            "missing accessible {preference} choice {value}"
        );
    }
    assert!(html.contains("data-scope=\"active\""));
    assert!(html.contains("data-scope=\"all\""));
    for (attribute, value) in [
        ("data-value", "cloud"),
        ("data-value", "balanced"),
        ("data-scope", "active"),
    ] {
        let selected = html
            .split('>')
            .find(|tag| tag.contains(&format!("{attribute}=\"{value}\"")))
            .unwrap();
        assert!(selected.contains("aria-pressed=\"true\""), "unexpected default: {selected}");
    }
}

#[tokio::test]
async fn panel_detail_page_renders_the_frozen_prompt_body() {
    let harness = start().await;
    let response = client()
        .get(format!("{}/?view=agents&agent={V2_ID}", harness.base))
        .send()
        .await
        .unwrap();
    let html = response.text().await.unwrap();
    assert!(html.contains("FROZEN-PROMPT-BODY"));
    assert!(html.contains("detail-prompt-text"));
    assert!(html.contains("agent_ready"));
    assert!(html.contains("data-detail-expand"));
    assert!(html.contains("data-detail-back"));
    // Message actions are plain form submit buttons, no bespoke JS.
    assert!(html.contains(&format!("/ui/agents/{V2_ID}/prompt")));
    assert!(html.contains(&format!("/ui/agents/{V2_ID}/steer")));
    assert!(html.contains(&format!("/ui/agents/{V2_ID}/follow-up")));
    assert!(html.contains(&format!("/ui/agents/{V2_ID}/stop")));
    assert!(html.contains(&format!("/ui/agents/{V2_ID}/restart")));
}

#[tokio::test]
async fn static_assets_are_served_with_the_locked_csp() {
    let harness = start().await;
    let css = client()
        .get(format!("{}/styles.css", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(css.status(), StatusCode::OK);
    assert_eq!(
        css.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/css; charset=utf-8"
    );
    assert!(css.headers().get(header::CONTENT_SECURITY_POLICY).is_some());
    let body = css.text().await.unwrap();
    assert!(!body.trim().is_empty());
    assert!(body.contains("prefers-reduced-motion"));

    let js = client()
        .get(format!("{}/app.js", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(
        js.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/javascript; charset=utf-8"
    );
    let body = js.text().await.unwrap();
    assert!(body.contains("showModal"));
    assert!(!body.contains("/v2/"));
    // Cancel/Escape must return the dialog form to the server-rendered
    // defaults, while an open dialog still suspends the auto refresh.
    assert!(body.contains("addEventListener(\"close\""));
    assert!(body.contains("querySelector(\"form\")?.reset()"));
    assert!(body.contains("document.querySelector(\"dialog[open]\")"));

    // Opening must never reset: that would erase values the user is editing.
    let open_body_start = body.find("const openDialog").expect("openDialog present");
    let open_body_end = body[open_body_start..]
        .find("};")
        .map(|offset| open_body_start + offset)
        .expect("openDialog closes");
    let open_body = &body[open_body_start..open_body_end];
    assert!(
        !open_body.contains("reset"),
        "openDialog must not reset the form, found: {open_body}"
    );
}

#[tokio::test]
async fn panel_forms_require_the_embedded_token() {
    let harness = start().await;
    let response = client()
        .post(format!("{}/ui/agents/create", harness.base))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("_token=wrong&name=x&cwd=/tmp")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = client()
        .post(format!("{}/ui/providers/add", harness.base))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(format!(
            "_token={TOKEN}&app=codex&id=team&name=Team&baseUrl=https%3A%2F%2Fapi.example.com%2Fv1&apiFormat=responses&apiKey=secret&modelList=model-a"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(response
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("view=providers"));
}

#[tokio::test]
async fn frozen_none_prompt_is_not_reported_as_legacy() {
    // A new Agent that deliberately appends nothing still froze a prompt
    // decision (mode=none with a digest). It must not be labelled legacy.
    let api = Arc::new(FakeApi::default());
    *api.prompt_none.lock().unwrap() = true;
    let harness = start_with(api).await;

    let html = client()
        .get(format!("{}/?view=agents&agent={V2_ID}", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        html.contains("未追加（使用载体自带系统提示）"),
        "a valid mode=none record must read as 'nothing appended'"
    );
    assert!(
        !html.contains("旧版 Agent · 未记录"),
        "a frozen mode=none record must not be reported as legacy"
    );
    // No prompt body is shown, because nothing was appended.
    assert!(!html.contains("detail-prompt-details"));
}

#[tokio::test]
async fn legacy_prompt_record_still_reads_as_unrecorded() {
    // Agents created before frozen prompt config have no record at all.
    let api = Arc::new(FakeApi::default());
    *api.prompt_legacy.lock().unwrap() = true;
    let harness = start_with(api).await;

    let html = client()
        .get(format!("{}/?view=agents&agent={V2_ID}", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("旧版 Agent · 未记录"));
    assert!(!html.contains("未追加（使用载体自带系统提示）"));
}

#[tokio::test]
async fn a_summary_without_a_digest_is_treated_as_unrecorded() {
    // Defensive: a summary without a digest is not proof of a frozen decision.
    let api = Arc::new(FakeApi::default());
    *api.prompt_empty_digest.lock().unwrap() = true;
    let harness = start_with(api).await;

    let html = client()
        .get(format!("{}/?view=agents&agent={V2_ID}", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("旧版 Agent · 未记录"));
}

#[tokio::test]
async fn skill_policy_label_describes_the_frozen_grants() {
    // `all` still explains that the set was frozen at creation time.
    let harness = start().await;
    let html = client()
        .get(format!("{}/?view=agents&agent={V2_ID}", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("全部 1 个 Skill（创建时冻结）"));
    assert!(html.contains("skill-a"), "grant chips must stay visible");

    // A record resolved into a single concrete grant must not claim the
    // operator picked "only"; it reports the frozen count instead.
    let api = Arc::new(FakeApi::default());
    *api.skill_only.lock().unwrap() = true;
    let harness = start_with(api).await;
    let html = client()
        .get(format!("{}/?view=agents&agent={V2_ID}", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("已冻结 1 个 Skill（创建时固定）"));
    assert!(
        !html.contains("仅指定"),
        "the label must not assert an operator-chosen 'only' policy"
    );
    assert!(html.contains("skill-a"), "grant chips must stay visible");
}

#[tokio::test]
async fn provider_api_format_options_are_scoped_per_carrier() {
    let harness = start().await;
    // The dialog is server-rendered, so assert on the HTML itself.
    let html = client()
        .get(format!("{}/?view=providers&create-provider=1", harness.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Each option declares its carrier and exactly one default per carrier.
    for (app, value) in [
        ("pi", "openai-completions"),
        ("codex", "responses"),
        ("claude", "anthropic"),
    ] {
        assert!(
            html.contains(&format!(
                "value=\"{value}\" data-app=\"{app}\" data-default=\"1\""
            )),
            "expected {app} default option {value}"
        );
    }
    assert!(html.contains("value=\"openai-responses\" data-app=\"pi\""));
    assert!(html.contains("value=\"anthropic-messages\" data-app=\"pi\""));

    // The dialog starts on Pi, so the other carriers' options are hidden until
    // the browser glue re-scopes them.
    assert!(html.contains("value=\"responses\" data-app=\"codex\" data-default=\"1\" hidden"));
    assert!(html.contains("value=\"anthropic\" data-app=\"claude\" data-default=\"1\" hidden"));
    assert!(!html.contains("value=\"openai-completions\" data-app=\"pi\" data-default=\"1\" hidden"));
}

#[tokio::test]
async fn provider_api_format_is_normalised_to_the_selected_carrier() {
    let harness = start().await;

    // A Pi format submitted while Codex is selected resolves to Codex's default
    // instead of being forwarded to CC Switch.
    let response = client()
        .post(format!("{}/ui/providers/add", harness.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_token={TOKEN}&app=codex&id=team-codex&name=Team&baseUrl=https%3A%2F%2Fapi.example.com%2Fv1&apiFormat=openai-completions&apiKey=secret&modelList=model-a"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // Claude likewise normalises to `anthropic`, and keeps its own formats.
    let response = client()
        .post(format!("{}/ui/providers/add", harness.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_token={TOKEN}&app=claude&id=team-claude&name=Team&baseUrl=https%3A%2F%2Fapi.example.com%2Fv1&apiFormat=responses&authMode=bearer&apiKey=secret&modelList=model-a"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let inputs = harness.api.provider_inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].app, ProviderApp::Codex);
    assert_eq!(inputs[0].api_format.as_deref(), Some("responses"));
    assert_eq!(inputs[1].app, ProviderApp::Claude);
    assert_eq!(inputs[1].api_format.as_deref(), Some("anthropic"));
    assert_eq!(inputs[1].auth_mode, Some(pihub_contracts::AuthMode::Bearer));

    // A carrier-appropriate value is preserved verbatim.
    let response = client()
        .post(format!("{}/ui/providers/add", harness.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_token={TOKEN}&app=pi&id=team-pi&name=Team&baseUrl=https%3A%2F%2Fapi.example.com%2Fv1&apiFormat=anthropic-messages&apiKey=secret&modelList=model-a"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let inputs = harness.api.provider_inputs.lock().unwrap().clone();
    assert_eq!(inputs[2].app, ProviderApp::Pi);
    assert_eq!(inputs[2].api_format.as_deref(), Some("anthropic-messages"));
}

#[tokio::test]
async fn panel_agent_detail_requires_an_existing_agent() {
    let harness = start().await;
    let response = client()
        .get(format!(
            "{}/?view=agents&agent=missing-agent",
            harness.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = response.text().await.unwrap();
    // Unknown ids fall back to the empty state instead of a server error.
    assert!(html.contains("empty-state"));
}

#[tokio::test]
async fn method_mismatch_is_not_found() {
    let harness = start().await;
    let response = client()
        .request(Method::DELETE, format!("{}/v2/snapshot", harness.base))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["code"], "not_found");
}

#[tokio::test]
async fn request_builder_keeps_axum_types_used() {
    // Guards against accidental API drift in the request type used by the
    // panel dispatcher.
    let request = Request::builder()
        .uri("/v2/snapshot")
        .body(Body::empty())
        .unwrap();
    assert_eq!(request.method(), Method::GET);
}
