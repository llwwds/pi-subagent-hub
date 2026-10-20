//! Daemon lifecycle: random loopback bind, state publication, authenticated
//! API access, graceful stop and state cleanup.

use std::sync::Arc;
use std::time::Duration;

use pihub_contracts::{
    AgentSpec, AgentView, ApiView, ControlAction, ControlResult, EventQuery, HubApi, HubError,
    HubPaths, HubResult, PromptCatalog, Provider, ProviderApp, ProviderApps, ProviderDiagnostics,
    ProviderInput, ProviderSnapshot, RuntimeRequestKind, SkillImportInput, SkillImportResult,
    SkillManagerAction, SkillSummary, Snapshot, SnapshotV2, StoredEvent,
};
use serde_json::json;
use tokio::sync::Notify;

struct EmptyApi;

#[pihub_contracts::async_trait]
impl HubApi for EmptyApi {
    async fn snapshot(&self, _view: ApiView) -> HubResult<Snapshot> {
        Ok(Snapshot::V2(SnapshotV2 {
            version: pihub_contracts::APP_VERSION.to_string(),
            profile: pihub_contracts::ProfileView {
                name: "default".into(),
                digest: "digest".into(),
                tools: vec![],
            },
            agents: vec![],
            active_count: 0,
            managed_count: 0,
            event_cursor: 0,
        }))
    }
    async fn list_agents(&self, _include_stopped: bool) -> HubResult<Vec<AgentView>> {
        Ok(vec![])
    }
    async fn agent(&self, _view: ApiView, id: &str) -> HubResult<AgentView> {
        Err(HubError::new("agent_not_found", format!("unknown agent: {id}")))
    }
    async fn create(&self, _view: ApiView, _spec: AgentSpec) -> HubResult<AgentView> {
        Err(HubError::new("request_failed", "not supported"))
    }
    async fn send(&self, _id: &str, _kind: RuntimeRequestKind, _message: &str) -> HubResult<serde_json::Value> {
        Ok(json!({}))
    }
    async fn control(&self, _view: ApiView, _id: &str, _action: ControlAction) -> HubResult<ControlResult> {
        Ok(ControlResult::Rpc(json!({})))
    }
    async fn events(&self, _view: ApiView, _query: EventQuery) -> HubResult<Vec<StoredEvent>> {
        Ok(vec![])
    }
    async fn carriers(&self) -> HubResult<Vec<pihub_contracts::CarrierDiagnostic>> {
        Ok(vec![])
    }
    async fn providers(&self) -> HubResult<ProviderSnapshot> {
        Ok(ProviderSnapshot {
            switch: ProviderDiagnostics {
                available: false,
                version: None,
                binary_path: "/tmp/cc-switch".into(),
                config_dir: "/tmp/cc-switch".into(),
                error_code: Some("CC_SWITCH_UNSUPPORTED".into()),
                reason: Some("not installed".into()),
            },
            apps: ProviderApps {
                claude: vec![],
                codex: vec![],
                pi: vec![],
            },
        })
    }
    async fn add_provider(&self, _input: ProviderInput) -> HubResult<Provider> {
        Err(HubError::new("CC_SWITCH_UNSUPPORTED", "not installed"))
    }
    async fn switch_provider(&self, _app: ProviderApp, _id: &str) -> HubResult<Provider> {
        Err(HubError::new("CC_SWITCH_UNSUPPORTED", "not installed"))
    }
    async fn skills(&self) -> HubResult<Vec<SkillSummary>> {
        Ok(vec![])
    }
    async fn import_skill(&self, _input: SkillImportInput) -> HubResult<SkillImportResult> {
        Err(HubError::new("request_failed", "not supported"))
    }
    async fn skill_manager(&self, _action: SkillManagerAction, _input: serde_json::Value) -> HubResult<serde_json::Value> {
        Ok(json!({ "connected": false }))
    }
    async fn prompts(&self) -> HubResult<PromptCatalog> {
        Ok(PromptCatalog {
            principles: vec![],
            default_principle_ids: vec![],
        })
    }
}

#[tokio::test]
async fn daemon_publishes_state_serves_and_cleans_up() {
    let home = tempfile::tempdir().unwrap();
    let paths = HubPaths::resolve(home.path(), "/nonexistent/app-root");
    let api = Arc::new(EmptyApi);

    let stop = Arc::new(Notify::new());
    let daemon_paths = paths.clone();
    let daemon_api = api.clone();
    let handle = tokio::spawn(async move { pihub_app::run_daemon(daemon_api, daemon_paths).await });

    // Wait for daemon.json (the daemon writes it after binding).
    let mut metadata = None;
    for _ in 0..100 {
        if let Ok(raw) = std::fs::read_to_string(&paths.daemon_path)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            metadata = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let metadata = metadata.expect("daemon should publish state/daemon.json");
    assert_eq!(metadata["host"], "127.0.0.1");
    let port = metadata["port"].as_u64().unwrap();
    assert!(port > 0, "the daemon must bind a random loopback port");
    assert_eq!(metadata["pid"].as_u64().unwrap(), std::process::id() as u64);

    let control: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.control_path).unwrap()).unwrap();
    let token = control["token"].as_str().unwrap().to_string();
    assert!(token.len() >= 32, "control token must be long enough");

    // The lock file exists while the daemon runs.
    assert!(paths.lock_path.exists());

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let health: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["data"]["name"], "pi-subagent-hub");
    assert_eq!(health["data"]["pid"].as_u64().unwrap(), std::process::id() as u64);

    let snapshot: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/v2/snapshot"))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["ok"], true);
    assert_eq!(snapshot["data"]["eventCursor"], 0);

    let stop_response: serde_json::Value = client
        .post(format!("http://127.0.0.1:{port}/v1/daemon/stop"))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stop_response["data"], json!({ "stopping": true }));

    let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(joined.is_ok(), "daemon should exit after the stop request");
    joined.unwrap().unwrap().expect("daemon exits cleanly");

    // State cleanup: metadata and lock are removed, the control token is kept.
    assert!(!paths.daemon_path.exists());
    assert!(!paths.lock_path.exists());
    assert!(paths.control_path.exists());

    // Port is released.
    let rebind = tokio::net::TcpListener::bind(("127.0.0.1", port as u16)).await;
    assert!(rebind.is_ok(), "the loopback port must be released");
    let _ = stop;
}
