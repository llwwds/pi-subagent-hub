//! Agent lifecycle against the contracts boundary: private directories, frozen
//! prompts, bounded event handling, status transitions and the v1/v2 views.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use pihub_contracts::{
    AgentSpec, AgentStatus, ApiView, ControlAction, ControlResult, DesiredState, EventQuery,
    CarrierCapabilities, HubApi, HubError, HubPaths, HubResult, ListAgentsOptions, ModelRef,
    PromptChannel, Runtime, RuntimeEvent, RuntimeFactory, RuntimeLaunch, RuntimeRequestKind,
    RuntimeState, SkillPolicy, Store, ThinkingLevel,
};
use pihub_core::profiles::resolve_profile;
use pihub_core::{AgentManager, AgentManagerOptions, FsSkillRegistry, HubStore};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const GOLDEN_DEFAULT_DIGEST: &str =
    "sha256:d587ddf75d8a632e4f88f32fb11e216a8e1bb90f1bce8eb59a573eaa5115e602";
const GOLDEN_CODEX_DIGEST: &str =
    "sha256:fbd45d80105071307afc2534c2cfe42e17c1937f8bf41030f7ff19e44ad644f6";

struct FakeRuntime {
    pid: u32,
    alive: AtomicBool,
    events: Mutex<Option<mpsc::Sender<RuntimeEvent>>>,
    requests: Mutex<Vec<(RuntimeRequestKind, Value)>>,
    fail_start: bool,
    /// Mimic a CLI carrier: stopping the runtime eventually reports its exit,
    /// possibly only after the next instance is already running.
    exit_on_stop: bool,
}

impl FakeRuntime {
    fn new(
        pid: u32,
        events: mpsc::Sender<RuntimeEvent>,
        fail_start: bool,
        exit_on_stop: bool,
    ) -> Self {
        Self {
            pid,
            alive: AtomicBool::new(true),
            events: Mutex::new(Some(events)),
            requests: Mutex::new(Vec::new()),
            fail_start,
            exit_on_stop,
        }
    }

    fn sender(&self) -> Option<mpsc::Sender<RuntimeEvent>> {
        self.events.lock().ok().and_then(|guard| guard.clone())
    }

    fn send(&self, event: RuntimeEvent) {
        if let Ok(guard) = self.events.lock() {
            if let Some(sender) = guard.as_ref() {
                let _ = sender.try_send(event);
            }
        }
    }
}

#[async_trait::async_trait]
impl Runtime for FakeRuntime {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn start(&self) -> HubResult<RuntimeState> {
        if self.fail_start {
            self.alive.store(false, Ordering::SeqCst);
            return Err(HubError::new(
                "runtime_start_failed",
                "Pi process exited before response (code=1)",
            ));
        }
        Ok(RuntimeState {
            session_id: Some("fake-session".to_string()),
            session_file: Some("/tmp/fake-session.jsonl".to_string()),
            model: Some(ModelRef {
                provider: "fake".to_string(),
                id: "fake-model".to_string(),
            }),
            thinking: Some(ThinkingLevel::Off),
            is_streaming: false,
        })
    }

    async fn request(&self, kind: RuntimeRequestKind, payload: Value) -> HubResult<Value> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push((kind, payload));
        }
        Ok(json!({
            "id": "1",
            "type": "response",
            "command": kind.as_str(),
            "success": true,
            "data": {},
        }))
    }

    async fn stop(&self) -> HubResult<()> {
        self.alive.store(false, Ordering::SeqCst);
        if self.exit_on_stop {
            // The child dies asynchronously, so the exit signal can arrive after
            // the manager already started a replacement instance.
            if let Ok(guard) = self.events.lock() {
                if let Some(sender) = guard.as_ref() {
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                        let _ = sender
                            .send(RuntimeEvent::Exit {
                                code: None,
                                signal: None,
                                at: "2026-01-01T00:00:00.000Z".to_string(),
                            })
                            .await;
                    });
                }
            }
        }
        Ok(())
    }
}

struct FakeFactory {
    available: bool,
    launches: Mutex<Vec<RuntimeLaunch>>,
    runtime: Mutex<Option<Arc<FakeRuntime>>>,
    instances: Mutex<Vec<Arc<FakeRuntime>>>,
    next_pid: Mutex<u32>,
    fail_start: AtomicBool,
    exit_on_stop: AtomicBool,
}

impl FakeFactory {
    fn new(available: bool) -> Self {
        Self {
            available,
            launches: Mutex::new(Vec::new()),
            runtime: Mutex::new(None),
            instances: Mutex::new(Vec::new()),
            next_pid: Mutex::new(5000),
            fail_start: AtomicBool::new(false),
            exit_on_stop: AtomicBool::new(false),
        }
    }

    /// Simulate a carrier that exits before answering (the OBS-1 condition).
    fn set_fail_start(&self, fail: bool) {
        self.fail_start.store(fail, Ordering::SeqCst);
    }

    /// Simulate a CLI carrier whose child exit lands asynchronously.
    fn set_exit_on_stop(&self, exit_on_stop: bool) {
        self.exit_on_stop.store(exit_on_stop, Ordering::SeqCst);
    }

    fn runtime(&self) -> Arc<FakeRuntime> {
        self.runtime
            .lock()
            .expect("runtime lock")
            .clone()
            .expect("runtime created")
    }

    fn launches(&self) -> Vec<RuntimeLaunch> {
        self.launches.lock().expect("launch lock").clone()
    }
}

#[async_trait::async_trait]
impl RuntimeFactory for FakeFactory {
    async fn diagnose(
        &self,
        carrier: pihub_contracts::Carrier,
    ) -> HubResult<pihub_contracts::CarrierDiagnostic> {
        Ok(pihub_contracts::CarrierDiagnostic {
            id: carrier,
            display_name: format!("fake-{}", carrier.as_str()),
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
                system_prompt: Some(PromptChannel::Developer),
            },
            available: self.available,
            reason: if self.available {
                None
            } else {
                Some("fake runtime is disabled".to_string())
            },
        })
    }

    async fn list(&self) -> HubResult<Vec<pihub_contracts::CarrierDiagnostic>> {
        let mut diagnostics = Vec::new();
        for carrier in pihub_contracts::Carrier::ALL {
            diagnostics.push(self.diagnose(carrier).await?);
        }
        Ok(diagnostics)
    }

    fn create(
        &self,
        launch: RuntimeLaunch,
        events: mpsc::Sender<RuntimeEvent>,
        _log: Option<Arc<dyn pihub_contracts::LogSink>>,
    ) -> HubResult<Arc<dyn Runtime>> {
        if let Ok(mut launches) = self.launches.lock() {
            launches.push(launch);
        }
        let mut pid_guard = self.next_pid.lock().expect("pid lock");
        *pid_guard += 1;
        let fail_start = self.fail_start.load(Ordering::SeqCst);
        let exit_on_stop = self.exit_on_stop.load(Ordering::SeqCst);
        let runtime = Arc::new(FakeRuntime::new(*pid_guard, events, fail_start, exit_on_stop));
        self.instances
            .lock()
            .expect("instances lock")
            .push(runtime.clone());
        drop(pid_guard);
        *self.runtime.lock().expect("runtime lock") = Some(runtime.clone());
        Ok(runtime)
    }
}

/// Deterministic pause point for the identity-check/write window.
///
/// The gate is entered from inside `Store::update_agent` for a *crashed* write of
/// one Agent, i.e. after the pump's identity check and before its store mutation
/// completes. With the fixed design that code runs while the runtime-map critical
/// section is held, so a replacement cannot be registered in between.
#[derive(Default)]
struct StoreGate {
    agent_id: Mutex<Option<String>>,
    hits: AtomicUsize,
    entered: Mutex<bool>,
    entered_cv: Condvar,
    released: Mutex<bool>,
    released_cv: Condvar,
}

impl StoreGate {
    fn arm(&self, agent_id: &str) {
        *self.agent_id.lock().expect("gate agent") = Some(agent_id.to_string());
        self.hits.store(0, Ordering::SeqCst);
        *self.entered.lock().expect("gate entered") = false;
        *self.released.lock().expect("gate released") = false;
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Block until the gated write has been entered (event driven, no polling).
    fn wait_entered(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut entered = self.entered.lock().expect("gate entered");
        while !*entered {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = self
                .entered_cv
                .wait_timeout(entered, remaining)
                .expect("gate wait");
            entered = guard;
        }
        true
    }

    fn release(&self) {
        let mut released = self.released.lock().expect("gate released");
        *released = true;
        self.released_cv.notify_all();
    }

    fn pause_if_gated(&self, agent_id: &str, status: Option<AgentStatus>) {
        if status != Some(AgentStatus::Crashed) {
            return;
        }
        let armed = self
            .agent_id
            .lock()
            .map(|guard| guard.as_deref() == Some(agent_id))
            .unwrap_or(false);
        if !armed {
            return;
        }
        self.hits.fetch_add(1, Ordering::SeqCst);
        {
            let mut entered = self.entered.lock().expect("gate entered");
            *entered = true;
            self.entered_cv.notify_all();
        }
        // Bounded wait: a stuck gate must fail the test, never hang the suite.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut released = self.released.lock().expect("gate released");
        while !*released {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            let (guard, _) = self
                .released_cv
                .wait_timeout(released, remaining)
                .expect("gate release wait");
            released = guard;
        }
    }
}

/// Store wrapper that records writes and can pause one crashed write via a gate.
struct RecordingStore {
    inner: Arc<HubStore>,
    gate: Mutex<Option<Arc<StoreGate>>>,
    crashed_writes: AtomicUsize,
    progress: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl RecordingStore {
    fn new(inner: Arc<HubStore>) -> Self {
        Self {
            inner,
            gate: Mutex::new(None),
            crashed_writes: AtomicUsize::new(0),
            progress: Mutex::new(None),
        }
    }

    fn install_gate(&self, gate: Arc<StoreGate>) {
        *self.gate.lock().expect("gate slot") = Some(gate);
    }

    fn crashed_writes(&self) -> usize {
        self.crashed_writes.load(Ordering::SeqCst)
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<String> {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.progress.lock().expect("progress slot") = Some(sender);
        receiver
    }

    fn report(&self, text: String) {
        if let Ok(guard) = self.progress.lock() {
            if let Some(sender) = guard.as_ref() {
                let _ = sender.send(text);
            }
        }
    }
}

impl Store for RecordingStore {
    fn create_agent(&self, agent: &pihub_contracts::AgentRecord) -> HubResult<pihub_contracts::AgentRecord> {
        self.inner.create_agent(agent)
    }

    fn get_agent(&self, id: &str) -> HubResult<Option<pihub_contracts::AgentRecord>> {
        self.inner.get_agent(id)
    }

    fn list_agents(&self, options: ListAgentsOptions) -> HubResult<Vec<pihub_contracts::AgentRecord>> {
        self.inner.list_agents(options)
    }

    fn update_agent(&self, id: &str, patch: pihub_contracts::AgentPatch) -> HubResult<pihub_contracts::AgentRecord> {
        self.report(format!("status:{:?}", patch.status));
        let gate = self.gate.lock().ok().and_then(|guard| guard.clone());
        if let Some(gate) = gate {
            gate.pause_if_gated(id, patch.status);
        }
        if patch.status == Some(AgentStatus::Crashed) {
            self.crashed_writes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.update_agent(id, patch)
    }

    fn append_event(&self, event: pihub_contracts::NewEvent) -> HubResult<i64> {
        self.report(format!("event:{}", event.event_type));
        self.inner.append_event(event)
    }

    fn list_events(&self, query: EventQuery) -> HubResult<Vec<pihub_contracts::StoredEvent>> {
        self.inner.list_events(query)
    }

    fn latest_cursor(&self, legacy_only: bool) -> HubResult<i64> {
        self.inner.latest_cursor(legacy_only)
    }

    fn mark_interrupted(&self) -> HubResult<()> {
        self.inner.mark_interrupted()
    }

    fn close(&self) -> HubResult<()> {
        self.inner.close()
    }
}

struct Harness {
    home: tempfile::TempDir,
    paths: HubPaths,
    store: Arc<RecordingStore>,
    manager: Arc<AgentManager>,
    factory: Arc<FakeFactory>,
}

fn harness(label: &str) -> Harness {
    let home = common::temp_home(label);
    let paths = HubPaths::resolve(home.path(), "/app-root");
    common::mkdir(&paths.state_dir);
    common::mkdir(&paths.agents_dir);
    let hub = Arc::new(HubStore::open(&paths.database_path).expect("store"));
    let store = Arc::new(RecordingStore::new(hub));
    let profile = resolve_profile(&json!({
        "schemaVersion": 1,
        "name": "test",
        "tools": ["read"],
        "skills": [],
        "extensions": [],
    }))
    .expect("profile");
    let factory = Arc::new(FakeFactory::new(true));
    let registry = Arc::new(
        FsSkillRegistry::new(home.path().join("skill-home")).expect("skill registry"),
    );
    let manager = Arc::new(AgentManager::new(AgentManagerOptions {
        paths: paths.clone(),
        store: store.clone() as Arc<dyn Store>,
        profile,
        runtime_factory: factory.clone(),
        skill_registry: Some(registry),
        manager_connection: None,
        provider_service: None,
        log: None,
        log_debug: false,
    })
    .expect("manager"));
    Harness {
        home,
        paths,
        store,
        manager,
        factory,
    }
}

impl Harness {
    fn spec(&self, id: &str, carrier: pihub_contracts::Carrier) -> AgentSpec {
        AgentSpec {
            agent_id: Some(id.to_string()),
            name: Some(id.to_string()),
            cwd: Some(self.home.path().to_path_buf()),
            carrier,
            skill_policy: Some(SkillPolicy::Only { ids: Vec::new() }),
            ..AgentSpec::default()
        }
    }

    fn cc_switch_fixture(&self) {
        common::mkdir(&self.paths.cc_switch_pi_dir);
        common::write_file(&self.paths.cc_switch_pi_dir.join("models.json"), "{}\n");
    }

    async fn wait_for_status(&self, id: &str, expected: AgentStatus) -> pihub_contracts::AgentRecord {
        for _ in 0..400 {
            let agent = self.store.get_agent(id).expect("get").expect("agent");
            if agent.status == expected {
                return agent;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("agent {id} never reached {expected:?}");
    }
}

#[tokio::test]
async fn platform_agent_gets_private_paths_a_frozen_prompt_and_events() {
    let harness = harness("lifecycle-create");
    harness.cc_switch_fixture();
    let created = harness
        .manager
        .create_agent(
            harness.spec("agent-create", pihub_contracts::Carrier::Pi),
            true,
        )
        .await
        .expect("create");

    assert_eq!(created.status, AgentStatus::Idle);
    assert_eq!(created.desired_state, DesiredState::Running);
    assert_eq!(created.pid, Some(5001));
    assert_eq!(created.session_file.as_deref(), Some("/tmp/fake-session.jsonl"));
    assert_eq!(created.provider.as_deref(), Some("fake"));
    assert_eq!(created.model.as_deref(), Some("fake-model"));

    // Directories are private and per-Agent.
    let root = harness.paths.agents_dir.join("agent-create");
    for name in ["pi-agent", "sessions", "logs", "state", "tmp"] {
        let path = root.join(name);
        assert!(path.is_dir(), "{} must exist", path.display());
        assert_eq!(
            pihub_core::fsutil::mode_string(pihub_core::fsutil::file_mode(&path).expect("mode")),
            "700"
        );
    }
    let settings = root.join("pi-agent/settings.json");
    let settings_value = common::read_json(&settings);
    assert_eq!(settings_value["enableInstallTelemetry"], json!(false));
    assert_eq!(settings_value["defaultProjectTrust"], json!("never"));
    assert!(root.join("pi-agent/models.json").is_symlink());
    assert_eq!(
        std::fs::read_link(root.join("pi-agent/models.json")).expect("link"),
        harness.paths.cc_switch_pi_dir.join("models.json")
    );

    // Frozen prompt: file + store projection, both with the Node digest.
    let frozen: pihub_contracts::FrozenPromptConfig =
        serde_json::from_value(common::read_json(&root.join("state/prompt-config.json")))
            .expect("frozen prompt");
    assert_eq!(frozen.digest, GOLDEN_DEFAULT_DIGEST);
    assert_eq!(frozen.principle_ids.len(), 4);
    let stored = harness.store.get_agent("agent-create").expect("get").expect("agent");
    assert_eq!(
        stored.prompt_config.expect("store projection").digest,
        GOLDEN_DEFAULT_DIGEST
    );
    assert_eq!(
        stored.skill_authorization.expect("authorization").mode,
        pihub_contracts::SkillPolicyMode::Only
    );

    // The runtime received the frozen prompt, not a re-resolved one.
    let launches = harness.factory.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(
        launches[0].prompt.as_ref().expect("launch prompt").digest,
        GOLDEN_DEFAULT_DIGEST
    );
    assert_eq!(launches[0].carrier, pihub_contracts::Carrier::Pi);

    let events = harness
        .store
        .list_events(EventQuery {
            agent_id: Some("agent-create".to_string()),
            ..EventQuery::default()
        })
        .expect("events");
    let types: Vec<&str> = events.iter().map(|event| event.event_type.as_str()).collect();
    assert!(types.contains(&"agent_created"));
    assert!(types.contains(&"agent_ready"));
    let created_event = events
        .iter()
        .find(|event| event.event_type == "agent_created")
        .expect("created event");
    assert_eq!(created_event.payload["carrier"], json!("pi"));
    assert_eq!(
        created_event.payload["prompt"]["digest"],
        json!(GOLDEN_DEFAULT_DIGEST)
    );

    assert_eq!(harness.manager.active_count(), 1);
    assert_eq!(harness.manager.managed_count(), 1);
    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn streaming_deltas_never_consume_a_cursor_and_exit_updates_status() {
    let harness = harness("lifecycle-stream");
    harness
        .manager
        .create_agent(
            harness.spec("agent-stream", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let runtime = harness.factory.runtime();
    let baseline = harness.store.latest_cursor(false).expect("cursor");

    runtime.send(RuntimeEvent::Event {
        event_type: "agent_start".to_string(),
        payload: json!({"type": "agent_start"}),
    });
    harness
        .wait_for_status("agent-stream", AgentStatus::Running)
        .await;

    // 3 delta frames (one wrapped twice) must not persist or move the cursor.
    for payload in [
        json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "x"}}),
        json!({"type": "carrier_event", "carrier": "codex", "value": {"type": "item.updated", "item": {"text": "y"}}}),
        json!({"type": "carrier_event", "value": {"type": "carrier_event", "value": {"type": "text_delta"}}}),
        json!({"type": "tool_execution_update", "partialResult": "z".repeat(4096)}),
    ] {
        runtime.send(RuntimeEvent::Event {
            event_type: payload["type"].as_str().unwrap_or("event").to_string(),
            payload,
        });
    }
    // Two semantic frames must persist.
    runtime.send(RuntimeEvent::Event {
        event_type: "tool_execution_end".to_string(),
        payload: json!({"type": "tool_execution_end", "toolCallId": "c1", "isError": false}),
    });
    runtime.send(RuntimeEvent::Event {
        event_type: "message_end".to_string(),
        payload: json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "final"}]}}),
    });

    for _ in 0..400 {
        if harness.store.latest_cursor(false).expect("cursor") >= baseline + 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let events = harness
        .store
        .list_events(EventQuery {
            agent_id: Some("agent-stream".to_string()),
            after: baseline,
            ..EventQuery::default()
        })
        .expect("events");
    let types: Vec<&str> = events.iter().map(|event| event.event_type.as_str()).collect();
    // `agent_start` is semantic; only the four delta frames are filtered.
    assert_eq!(
        types,
        vec!["agent_start", "tool_execution_end", "message_end"]
    );
    assert_eq!(
        harness.store.latest_cursor(false).expect("cursor") - baseline,
        events.len() as i64
    );
    assert_eq!(
        events[2].payload["message"]["content"][0]["text"],
        json!("final")
    );

    runtime.send(RuntimeEvent::Exit {
        code: Some(1),
        signal: None,
        at: "2026-01-01T00:00:00.000Z".to_string(),
    });
    let crashed = harness
        .wait_for_status("agent-stream", AgentStatus::Crashed)
        .await;
    assert_eq!(crashed.pid, None);
    assert!(crashed
        .last_error
        .expect("last error")
        .contains("process exited"));
    assert_eq!(harness.manager.managed_count(), 0);

    let exit_event = harness
        .store
        .list_events(EventQuery {
            agent_id: Some("agent-stream".to_string()),
            after: baseline,
            ..EventQuery::default()
        })
        .expect("events")
        .into_iter()
        .find(|event| event.event_type == "process_exit")
        .expect("exit event");
    assert_eq!(exit_event.payload["expected"], json!(false));
    assert_eq!(exit_event.payload["code"], json!(1));

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn stop_and_restart_keep_the_frozen_prompt() {
    let harness = harness("lifecycle-restart");
    harness
        .manager
        .create_agent(
            harness.spec("agent-restart", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");

    let stopped = harness.manager.stop_agent("agent-restart").await.expect("stop");
    assert_eq!(stopped.status, AgentStatus::Stopped);
    assert_eq!(stopped.desired_state, DesiredState::Stopped);
    assert_eq!(stopped.pid, None);
    assert_eq!(harness.manager.managed_count(), 0);

    let restarted = harness
        .manager
        .restart_agent("agent-restart")
        .await
        .expect("restart");
    assert_eq!(restarted.status, AgentStatus::Idle);
    assert_eq!(restarted.desired_state, DesiredState::Running);
    let launches = harness.factory.launches();
    assert_eq!(launches.len(), 2);
    assert_eq!(
        launches[0].prompt.as_ref().expect("prompt").digest,
        launches[1].prompt.as_ref().expect("prompt").digest
    );
    assert_eq!(
        launches[0].session_id, launches[1].session_id,
        "restart reuses the private session identity"
    );

    let events = harness
        .store
        .list_events(EventQuery {
            agent_id: Some("agent-restart".to_string()),
            ..EventQuery::default()
        })
        .expect("events");
    let types: Vec<&str> = events.iter().map(|event| event.event_type.as_str()).collect();
    assert!(types.contains(&"stop_requested"));
    assert!(types.contains(&"restart_requested"));

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn v1_creation_stays_legacy_and_v2_excludes_it_from_legacy_views() {
    let harness = harness("lifecycle-views");
    harness
        .manager
        .create_agent(
            harness.spec("agent-legacy", pihub_contracts::Carrier::Pi),
            false,
        )
        .await
        .expect("create v1");
    harness
        .manager
        .create_agent(
            harness.spec("agent-modern", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create v2");

    let legacy = harness.store.get_agent("agent-legacy").expect("get").expect("agent");
    assert!(legacy.skill_authorization.is_none());
    assert_eq!(legacy.carrier, pihub_contracts::Carrier::Pi);

    let v1_rows = harness
        .store
        .list_agents(ListAgentsOptions {
            include_stopped: true,
            legacy_only: true,
        })
        .expect("list");
    assert_eq!(v1_rows.len(), 1);
    assert_eq!(v1_rows[0].id, "agent-legacy");

    let snapshot_v1 = harness.manager.snapshot(ApiView::V1).await.expect("v1 snapshot");
    match &snapshot_v1 {
        pihub_contracts::Snapshot::V1(snapshot) => {
            assert_eq!(snapshot.agents.len(), 1);
            assert_eq!(snapshot.agents[0].id, "agent-legacy");
            assert!(snapshot.event_cursor >= 1);
        }
        other => panic!("expected v1 snapshot, got {other:?}"),
    }
    let snapshot_v2 = harness.manager.snapshot(ApiView::V2).await.expect("v2 snapshot");
    match &snapshot_v2 {
        pihub_contracts::Snapshot::V2(snapshot) => {
            assert_eq!(snapshot.agents.len(), 2);
            assert_eq!(snapshot.active_count, 2);
            assert_eq!(snapshot.managed_count, 2);
            let legacy_view = snapshot
                .agents
                .iter()
                .find(|view| view.agent.id == "agent-legacy")
                .expect("legacy view");
            assert_eq!(
                legacy_view.skill_policy.mode,
                pihub_contracts::SkillPolicyViewMode::Legacy
            );
            assert!(legacy_view.prompt_config.as_ref().expect("summary").prompt.is_none());
        }
        other => panic!("expected v2 snapshot, got {other:?}"),
    }

    // v2 detail includes the frozen prompt text, v1 detail never does.
    let detail_v2 = harness
        .manager
        .agent(ApiView::V2, "agent-modern")
        .await
        .expect("v2 detail");
    match detail_v2 {
        pihub_contracts::AgentView::V2(view) => {
            let summary = view.prompt_config.expect("prompt summary");
            assert_eq!(summary.channel, Some(PromptChannel::Developer));
            assert!(summary.prompt_chars > 0);
            assert!(summary.custom_text_chars == 0);
            let text = summary.prompt.expect("v2 detail includes prompt text");
            assert!(text.contains("第一性原理"));
        }
        other => panic!("expected v2 view, got {other:?}"),
    }
    // 2.1 hides a v2 Agent from every /v1 route (`http-server.js:188`).
    let v1_refusal = harness
        .manager
        .agent(ApiView::V1, "agent-modern")
        .await
        .expect_err("v1 must refuse a v2 agent");
    assert_eq!(v1_refusal.code, "agent_not_found");
    let legacy_detail = harness
        .manager
        .agent(ApiView::V1, "agent-legacy")
        .await
        .expect("v1 detail for the legacy agent");
    match legacy_detail {
        pihub_contracts::AgentView::V1(public) => {
            let serialized = serde_json::to_string(&public).expect("serialize");
            assert!(!serialized.contains("promptConfig"));
            assert!(!serialized.contains("skills_json"));
        }
        other => panic!("expected v1 view, got {other:?}"),
    }

    let not_found = harness
        .manager
        .agent(ApiView::V2, "agent-missing")
        .await
        .expect_err("unknown agent");
    assert_eq!(not_found.code, "agent_not_found");

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn send_abort_stop_and_restart_flow_through_the_runtime() {
    let harness = harness("lifecycle-control");
    harness
        .manager
        .create_agent(
            harness.spec("agent-control", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let runtime = harness.factory.runtime();

    harness
        .manager
        .send("agent-control", RuntimeRequestKind::Prompt, "hello")
        .await
        .expect("send");
    let abort = harness
        .manager
        .control(ApiView::V2, "agent-control", ControlAction::Abort)
        .await
        .expect("abort");
    match abort {
        ControlResult::Rpc(response) => {
            assert_eq!(response["command"], json!("abort"));
            assert_eq!(response["success"], json!(true));
        }
        other => panic!("expected rpc result, got {other:?}"),
    }

    let requests = runtime.requests.lock().expect("requests").clone();
    assert_eq!(requests[0].0, RuntimeRequestKind::Prompt);
    assert_eq!(requests[0].1["message"], json!("hello"));
    assert_eq!(requests[1].0, RuntimeRequestKind::Abort);

    let stop = harness
        .manager
        .control(ApiView::V2, "agent-control", ControlAction::Stop)
        .await
        .expect("stop");
    match stop {
        ControlResult::Agent(pihub_contracts::AgentView::V1(public)) => {
            assert_eq!(public.status, AgentStatus::Stopped);
        }
        other => panic!("expected legacy view for stop, got {other:?}"),
    }

    let restart = harness
        .manager
        .control(ApiView::V2, "agent-control", ControlAction::Restart)
        .await
        .expect("restart");
    match restart {
        ControlResult::Agent(pihub_contracts::AgentView::V1(public)) => {
            assert_eq!(public.status, AgentStatus::Idle);
        }
        other => panic!("expected restarted agent, got {other:?}"),
    }

    let empty = harness
        .manager
        .send("agent-control", RuntimeRequestKind::Prompt, "")
        .await
        .expect_err("empty message");
    assert_eq!(empty.code, "invalid_message");

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn create_rejects_duplicates_unavailable_carriers_and_bad_prompts() {
    let harness = harness("lifecycle-reject");
    harness
        .manager
        .create_agent(
            harness.spec("agent-dupe", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let duplicate = harness
        .manager
        .create_agent(
            harness.spec("agent-dupe", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect_err("duplicate");
    assert_eq!(duplicate.code, "agent_exists");

    let mut isolated = harness.spec("agent-isolated", pihub_contracts::Carrier::Codex);
    isolated.workspace_mode = Some("isolated".to_string());
    let error = harness
        .manager
        .create_agent(isolated, true)
        .await
        .expect_err("isolated workspace");
    assert!(error.message.contains("not implemented yet"));
    assert!(harness.store.get_agent("agent-isolated").expect("get").is_none());
    assert!(!harness.paths.agents_dir.join("agent-isolated").exists());

    let mut bad_prompt = harness.spec("agent-bad-prompt", pihub_contracts::Carrier::Codex);
    bad_prompt.prompt_config = Some(pihub_contracts::PromptConfigInput {
        principle_ids: Some(vec!["unknown".to_string()]),
        custom_text: None,
    });
    let error = harness
        .manager
        .create_agent(bad_prompt, true)
        .await
        .expect_err("bad prompt");
    assert_eq!(error.code, "invalid_prompt_config");
    assert!(!harness.paths.agents_dir.join("agent-bad-prompt").exists());

    let unavailable = Arc::new(FakeFactory::new(false));
    let manager = Arc::new(AgentManager::new(AgentManagerOptions {
        paths: harness.paths.clone(),
        store: harness.store.clone() as Arc<dyn Store>,
        profile: resolve_profile(&json!({"schemaVersion": 1, "tools": ["read"]}))
            .expect("profile"),
        runtime_factory: unavailable,
        skill_registry: None,
        manager_connection: None,
        provider_service: None,
        log: None,
        log_debug: false,
    })
    .expect("manager"));
    let error = manager
        .create_agent(
            harness.spec("agent-unavailable", pihub_contracts::Carrier::Claude),
            true,
        )
        .await
        .expect_err("carrier unavailable");
    assert_eq!(error.code, "carrier_unavailable");
    assert!(error.message.contains("fake runtime is disabled"));

    let invalid_cwd = AgentSpec {
        agent_id: Some("agent-cwd".to_string()),
        cwd: Some(PathBuf::from("/definitely/not/here")),
        carrier: pihub_contracts::Carrier::Codex,
        ..AgentSpec::default()
    };
    let error = manager
        .create_agent(invalid_cwd, true)
        .await
        .expect_err("bad cwd");
    assert_eq!(error.code, "invalid_cwd");

    manager.close_logs().await;
    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// Proves the identity check and the frame's store mutation share one critical
/// section. A frame is paused inside its store write (i.e. after the check); a
/// concurrent stop then reaches the runtime-map lock. In the fixed design the
/// stop can only finish *after* that write, so its Stopped projection wins; a
/// check-then-write implementation lets the stop finish first and the late crash
/// write clobbers the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn store_gate_shows_identity_check_and_write_are_one_critical_section() {
    let harness = harness("lifecycle-gate");
    let gate = Arc::new(StoreGate::default());
    harness.store.install_gate(gate.clone());
    let mut progress = harness.store.subscribe();

    harness
        .manager
        .create_agent(
            harness.spec("agent-gate", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let old_instance = harness.factory.runtime();

    // A semantic carrier failure: the pump will write crashed + last_error.
    gate.arm("agent-gate");
    old_instance.send(RuntimeEvent::ProcessError {
        message: "pi worker died".to_string(),
    });
    assert!(
        gate.wait_entered(std::time::Duration::from_secs(20)),
        "the frame must reach its store write (paused after the identity check)"
    );
    assert_eq!(gate.hits(), 1);

    // While that write is paused, stop the Agent. Its `stopping` write happens
    // before it needs the runtime map, so it is a deterministic progress signal.
    let manager = harness.manager.clone();
    let stop = tokio::spawn(async move { manager.stop_agent("agent-gate").await });
    let mut saw_stopping = false;
    while !saw_stopping {
        match tokio::time::timeout(std::time::Duration::from_secs(20), progress.recv()).await {
            Ok(Some(message)) => saw_stopping = message == "status:Some(Stopping)",
            Ok(None) => panic!("store progress channel closed before the stop ran"),
            Err(_) => panic!("stop never reached its store write"),
        }
    }

    // The paused frame holds the runtime-map critical section, so the stop must
    // not be able to run to completion here. The bounded window only bounds
    // liveness; the invariant under test is mutual exclusion.
    let mut stop = stop;
    match tokio::time::timeout(std::time::Duration::from_millis(250), &mut stop).await {
        Ok(joined) => {
            let outcome = joined.expect("stop task did not panic");
            gate.release();
            panic!(
                "concurrent stop completed while the paused frame write held no critical section: {:?}",
                outcome.map(|agent| agent.status)
            );
        }
        Err(_) => {
            // Still blocked: exactly one critical section covers check + write.
        }
    }

    // Release the paused write; the stop can now only proceed after it, so the
    // stop's terminal projection is the last word on the row.
    gate.release();
    let stopped = stop.await.expect("stop task").expect("stop");
    let settled = harness
        .store
        .get_agent("agent-gate")
        .expect("get")
        .expect("agent");
    assert_eq!(
        settled.status,
        AgentStatus::Stopped,
        "the late crash write must not outlive the stop that followed it"
    );
    assert_eq!(stopped.status, AgentStatus::Stopped);
    assert_eq!(settled.desired_state, DesiredState::Stopped);
    assert_eq!(settled.pid, None);
    assert_eq!(harness.manager.managed_count(), 0);
    assert!(
        harness.store.crashed_writes() >= 1,
        "the pre-stop crash write really happened"
    );
    // The crash was written before the stop completed, never after it.
    let events = harness
        .store
        .list_events(EventQuery {
            agent_id: Some("agent-gate".to_string()),
            ..EventQuery::default()
        })
        .expect("events");
    let crash_index = events
        .iter()
        .position(|event| event.event_type == "process_error")
        .expect("process_error recorded");
    let stop_index = events
        .iter()
        .position(|event| event.event_type == "stop_requested")
        .expect("stop_requested recorded");
    assert!(
        stop_index < crash_index,
        "the stop began first, yet the paused frame was applied before the stop could finish"
    );
    // Nothing the paused frame wrote survives the stop that followed it.
    assert_eq!(settled.status, AgentStatus::Stopped);

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// Concurrent restarts while frames are flowing must leave exactly one live
/// instance with no residual error, without relying on sleeps for the proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_restarts_leave_one_healthy_instance() {
    let harness = harness("lifecycle-concurrent");
    harness
        .manager
        .create_agent(
            harness.spec("agent-concurrent", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");

    for round in 0..20 {
        let current = harness.factory.runtime();
        for _ in 0..4 {
            current.send(RuntimeEvent::Event {
                event_type: "agent_settled".to_string(),
                payload: json!({"type": "agent_settled"}),
            });
        }
        harness
            .manager
            .restart_agent("agent-concurrent")
            .await
            .expect("restart");
        let agent = harness
            .store
            .get_agent("agent-concurrent")
            .expect("get")
            .expect("agent");
        assert_eq!(agent.status, AgentStatus::Idle, "round {round}");
        assert_eq!(agent.last_error, None, "round {round}");
        assert_eq!(agent.desired_state, DesiredState::Running, "round {round}");
        assert_eq!(harness.manager.managed_count(), 1, "round {round}");
    }
    assert_eq!(
        harness.store.crashed_writes(),
        0,
        "a benign restart must never write a crash"
    );

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// DOM T2/T5a: an exit reported by a *stopped* instance must not be attributed to
/// the replacement instance that is already running under the same agent id.
#[tokio::test]
async fn late_exit_from_a_stopped_instance_cannot_clobber_the_new_one() {
    let harness = harness("lifecycle-late-exit");
    harness
        .manager
        .create_agent(
            harness.spec("agent-late-exit", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let old_instance = harness.factory.runtime();

    harness
        .manager
        .stop_agent("agent-late-exit")
        .await
        .expect("stop");
    harness
        .manager
        .restart_agent("agent-late-exit")
        .await
        .expect("restart");
    let new_instance = harness.factory.runtime();
    assert!(!Arc::ptr_eq(&old_instance, &new_instance));
    let ready = harness
        .store
        .get_agent("agent-late-exit")
        .expect("get")
        .expect("agent");
    assert_eq!(ready.status, AgentStatus::Idle);
    assert_eq!(ready.last_error, None);

    // The stopped instance reports its exit only now, after the new one is ready.
    let old_sender = old_instance.sender().expect("old sender");
    old_instance.send(RuntimeEvent::Exit {
        code: None,
        signal: None,
        at: "2026-01-01T00:00:00.000Z".to_string(),
    });
    // Event-driven: the old pump terminates when it consumes that exit. A stale
    // frame must not have written a crash before ending.
    tokio::time::timeout(std::time::Duration::from_secs(20), old_sender.closed())
        .await
        .expect("the stale pump must terminate after its exit");

    let after = harness
        .store
        .get_agent("agent-late-exit")
        .expect("get")
        .expect("agent");
    assert_eq!(
        after.status,
        AgentStatus::Idle,
        "a stale instance exit must not mark the live instance crashed"
    );
    assert_eq!(
        after.last_error, None,
        "the stale exit message must not become the live instance's error"
    );
    assert_eq!(after.desired_state, DesiredState::Running);
    assert_eq!(harness.manager.managed_count(), 1, "live entry retained");

    // The retained entry must still be the new instance, not the stale one.
    harness
        .manager
        .send("agent-late-exit", RuntimeRequestKind::Prompt, "still-there")
        .await
        .expect("live instance still addressable");
    assert!(
        new_instance.requests.lock().expect("requests").iter().any(|(kind, payload)| {
            *kind == RuntimeRequestKind::Prompt && payload["message"] == json!("still-there")
        }),
        "the prompt must reach the replacement instance"
    );
    assert_eq!(
        old_instance.requests.lock().expect("requests").len(),
        0,
        "the stopped instance must not receive anything"
    );

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// Original DOM scenario: repeated restarts with no prompt, waiting for the event
/// pumps to settle, must stay `idle` without a stale `last_error`.
#[tokio::test]
async fn consecutive_restarts_stay_idle_without_stale_exit_errors() {
    let harness = harness("lifecycle-restart-settle");
    harness.factory.set_exit_on_stop(true);
    harness
        .manager
        .create_agent(
            harness.spec("agent-settle", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");

    for attempt in 0..3 {
        let restarted = harness
            .manager
            .restart_agent("agent-settle")
            .await
            .expect("restart");
        assert_eq!(restarted.status, AgentStatus::Idle, "attempt {attempt}");
        // Long enough for the previous instance's delayed exit to arrive.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let settled = harness
            .store
            .get_agent("agent-settle")
            .expect("get")
            .expect("agent");
        assert_eq!(
            settled.status,
            AgentStatus::Idle,
            "attempt {attempt}: stale exit must not crash the live instance"
        );
        assert_eq!(
            settled.last_error, None,
            "attempt {attempt}: no stale exit message may survive"
        );
        assert_eq!(settled.desired_state, DesiredState::Running);
        assert_eq!(harness.manager.managed_count(), 1, "attempt {attempt}");
    }

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// OBS-1: a carrier that exits before answering must leave an explicit terminal
/// state, not `starting` forever, and the Agent must stay retryable.
#[tokio::test]
async fn failed_start_reaches_a_terminal_state_and_stays_retryable() {
    let harness = harness("lifecycle-start-failure");

    // 1. Creation against a failing carrier: explicit failure, no stuck row.
    harness.factory.set_fail_start(true);
    let error = harness
        .manager
        .create_agent(
            harness.spec("agent-fail-create", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect_err("start must fail");
    assert_eq!(error.code, "runtime_start_failed");
    let created = harness
        .store
        .get_agent("agent-fail-create")
        .expect("get")
        .expect("agent");
    assert_eq!(created.status, AgentStatus::Failed, "not stuck in starting");
    assert_eq!(created.desired_state, DesiredState::Stopped);
    assert_eq!(created.pid, None);
    assert!(created
        .last_error
        .as_deref()
        .expect("last error")
        .contains("exited before response"));
    assert_eq!(harness.manager.managed_count(), 0, "no dead adapter retained");

    // 2. Restart of a healthy Agent against the same failing carrier.
    harness.factory.set_fail_start(false);
    harness
        .manager
        .create_agent(
            harness.spec("agent-fail-restart", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("healthy create");
    harness.factory.set_fail_start(true);
    let error = harness
        .manager
        .restart_agent("agent-fail-restart")
        .await
        .expect_err("restart must fail");
    assert_eq!(error.code, "runtime_start_failed");
    let restarted = harness
        .store
        .get_agent("agent-fail-restart")
        .expect("get")
        .expect("agent");
    assert_eq!(
        restarted.status,
        AgentStatus::Failed,
        "a failed restart must not leave the row in starting"
    );
    assert_eq!(restarted.desired_state, DesiredState::Stopped);
    assert_eq!(restarted.pid, None);
    assert_eq!(harness.manager.managed_count(), 0);

    // Both failures are recorded as semantic events.
    for id in ["agent-fail-create", "agent-fail-restart"] {
        let events = harness
            .store
            .list_events(EventQuery {
                agent_id: Some(id.to_string()),
                ..EventQuery::default()
            })
            .expect("events");
        let failure = events
            .iter()
            .find(|event| event.event_type == "agent_start_failed")
            .unwrap_or_else(|| panic!("{id} must record agent_start_failed"));
        assert!(failure.payload["message"]
            .as_str()
            .expect("message")
            .contains("exited before response"));
    }

    // 3. The terminal state is retryable once the carrier recovers.
    harness.factory.set_fail_start(false);
    let recovered = harness
        .manager
        .restart_agent("agent-fail-restart")
        .await
        .expect("retry after recovery");
    assert_eq!(recovered.status, AgentStatus::Idle);
    assert_eq!(recovered.desired_state, DesiredState::Running);
    assert_eq!(recovered.pid, Some(5004));
    assert_eq!(harness.manager.managed_count(), 1);

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn a_conflicting_start_does_not_downgrade_a_healthy_agent() {
    let harness = harness("lifecycle-conflict");
    harness
        .manager
        .create_agent(
            harness.spec("agent-conflict", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let error = harness
        .manager
        .start_stored_agent("agent-conflict", None)
        .await
        .expect_err("already running");
    assert_eq!(error.code, "agent_running");
    let agent = harness
        .store
        .get_agent("agent-conflict")
        .expect("get")
        .expect("agent");
    assert_eq!(agent.status, AgentStatus::Idle, "healthy state preserved");
    assert_eq!(agent.desired_state, DesiredState::Running);
    assert_eq!(harness.manager.managed_count(), 1);

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// 2.1 public views keep the hydrated `tools`/`skills`/`extensions` columns and
/// only drop their `*_json` counterparts; for an authorized Agent `skills` holds
/// the frozen grants (`{id,path,sha256}`), not a separate grants field.
#[tokio::test]
async fn public_views_project_hydrated_skills_and_the_workspace_warning_is_boolean() {
    let harness = harness("lifecycle-public-shape");
    let source = common::write_skill_package(harness.home.path(), "shape-skill", "body");
    harness
        .manager
        .import_skill(pihub_contracts::SkillImportInput {
            id: "shape-skill".to_string(),
            source_path: source,
            source_label: None,
        })
        .await
        .expect("import");
    let mut spec = harness.spec("agent-shape", pihub_contracts::Carrier::Codex);
    spec.skill_policy = Some(SkillPolicy::Only {
        ids: vec!["shape-skill".to_string()],
    });
    let created = harness
        .manager
        .create_agent(spec, true)
        .await
        .expect("create");

    let v2 = serde_json::to_value(
        harness
            .manager
            .agent(ApiView::V2, &created.id)
            .await
            .expect("v2 detail"),
    )
    .expect("serialize v2");
    assert_eq!(v2["tools"], json!(["read"]));
    assert_eq!(v2["extensions"], json!([]));
    let skills = v2["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["id"], json!("shape-skill"));
    assert!(skills[0]["sha256"].as_str().expect("sha256").len() == 64);
    assert!(
        v2.get("grants").is_none(),
        "authorization grants live in skills[], no separate field"
    );
    for raw in ["tools_json", "skills_json", "extensions_json"] {
        assert!(v2.get(raw).is_none(), "{raw} must stay private");
    }

    // v1 is the same hydrated projection for a legacy Agent.
    let legacy = harness
        .manager
        .create_agent(
            harness.spec("agent-shape-v1", pihub_contracts::Carrier::Pi),
            false,
        )
        .await
        .expect("create v1");
    let v1 = serde_json::to_value(
        harness
            .manager
            .agent(ApiView::V1, &legacy.id)
            .await
            .expect("v1 detail"),
    )
    .expect("serialize v1");
    assert_eq!(v1["tools"], json!(["read"]));
    assert!(v1["skills"].is_array());

    // 2.1 emits `sharedWorkspaceWarning` as a boolean, never as a message.
    let events = harness
        .store
        .list_events(EventQuery {
            agent_id: Some(created.id.clone()),
            ..EventQuery::default()
        })
        .expect("events");
    let created_event = events
        .iter()
        .find(|event| event.event_type == "agent_created")
        .expect("agent_created");
    assert_eq!(created_event.payload["sharedWorkspaceWarning"], json!(true));
    assert_eq!(created_event.payload["workspaceMode"], json!("shared"));
    // The event row keeps its raw column text as well.
    assert!(created_event.payload_json.contains("sharedWorkspaceWarning"));

    common::remove_tree(&harness.home.path().join("skill-home/skills/objects"));
    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// 2.1 ordering: whoever observes a settled Agent already has `agent_settled`,
/// so the next `command_accepted` can never precede it.
#[tokio::test]
async fn settled_event_precedes_the_next_command_accepted() {
    let harness = harness("lifecycle-ordering");
    harness
        .manager
        .create_agent(
            harness.spec("agent-order", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let runtime = harness.factory.runtime();

    async fn events_of(harness: &Harness) -> Vec<pihub_contracts::StoredEvent> {
        harness
            .store
            .list_events(EventQuery {
                agent_id: Some("agent-order".to_string()),
                ..EventQuery::default()
            })
            .expect("events")
    }

    async fn wait_for(harness: &Harness, event_type: &str, count: usize) {
        for _ in 0..5_000 {
            let seen = events_of(harness)
                .await
                .iter()
                .filter(|event| event.event_type == event_type)
                .count();
            if seen >= count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{event_type} never reached count {count}");
    }

    for turn in 1..=3 {
        harness
            .manager
            .send("agent-order", RuntimeRequestKind::Prompt, "go")
            .await
            .expect("send");
        wait_for(&harness, "command_accepted", turn).await;
        runtime.send(RuntimeEvent::Event {
            event_type: "agent_settled".to_string(),
            payload: json!({"type": "agent_settled"}),
        });
        wait_for(&harness, "agent_settled", turn).await;

        let events = events_of(&harness).await;
        let settled_index = events
            .iter()
            .rposition(|event| event.event_type == "agent_settled")
            .expect("settled recorded");
        let accepted_after = events
            .iter()
            .skip(settled_index + 1)
            .filter(|event| event.event_type == "command_accepted")
            .count();
        assert_eq!(
            accepted_after, 0,
            "turn {turn}: a command was accepted before this settle was recorded"
        );
        // The Agent is observably idle only after its settle row exists.
        let agent = harness
            .store
            .get_agent("agent-order")
            .expect("get")
            .expect("agent");
        assert_eq!(agent.status, AgentStatus::Idle);
    }

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

/// 2.1 keeps v2 Agents invisible on every `/v1/agents/:id...` route
/// (`src/http-server.js:188`). This is a view rule, never a tamper signal.
#[tokio::test]
async fn v1_routes_refuse_a_v2_agent_like_2_1() {
    let harness = harness("lifecycle-v1-gate");
    harness
        .manager
        .create_agent(
            harness.spec("agent-v2", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create v2");
    harness
        .manager
        .create_agent(
            harness.spec("agent-v1", pihub_contracts::Carrier::Pi),
            false,
        )
        .await
        .expect("create v1");

    // v2 detail still works.
    assert!(matches!(
        harness.manager.agent(ApiView::V2, "agent-v2").await.expect("v2 detail"),
        pihub_contracts::AgentView::V2(_)
    ));
    // v1 detail refuses it as if it did not exist.
    let error = harness
        .manager
        .agent(ApiView::V1, "agent-v2")
        .await
        .expect_err("v1 detail must refuse a v2 agent");
    assert_eq!(error.code, "agent_not_found");

    // v1 lifecycle control refuses and leaves the live instance untouched.
    let error = harness
        .manager
        .control(ApiView::V1, "agent-v2", ControlAction::Stop)
        .await
        .expect_err("v1 control must refuse a v2 agent");
    assert_eq!(error.code, "agent_not_found");
    let row = harness
        .store
        .get_agent("agent-v2")
        .expect("get")
        .expect("agent");
    assert_eq!(row.status, AgentStatus::Idle);
    assert_eq!(
        harness.manager.managed_count(),
        2,
        "the refused v1 stop must not have stopped either instance"
    );

    // Agent-scoped v1 events refuse too; the global feed simply stays legacy-only.
    let error = harness
        .manager
        .events(
            ApiView::V1,
            EventQuery {
                agent_id: Some("agent-v2".to_string()),
                ..EventQuery::default()
            },
        )
        .await
        .expect_err("v1 events must refuse a v2 agent");
    assert_eq!(error.code, "agent_not_found");
    let global = harness
        .manager
        .events(ApiView::V1, EventQuery::default())
        .await
        .expect("global v1 events");
    assert!(global
        .iter()
        .all(|event| event.agent_id.as_deref() != Some("agent-v2")));

    // The legacy Agent is still served by v1.
    assert!(matches!(
        harness.manager.agent(ApiView::V1, "agent-v1").await.expect("v1 detail"),
        pihub_contracts::AgentView::V1(_)
    ));

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn recover_after_restart_marks_active_agents_crashed() {
    let harness = harness("lifecycle-recover");
    harness
        .manager
        .create_agent(
            harness.spec("agent-recover", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    // Simulate a daemon that died while the Agent was still active.
    harness
        .store
        .update_agent(
            "agent-recover",
            pihub_contracts::AgentPatch {
                status: Some(AgentStatus::Running),
                ..Default::default()
            },
        )
        .expect("update");
    harness.manager.recover_after_daemon_restart().expect("recover");
    let agent = harness.store.get_agent("agent-recover").expect("get").expect("agent");
    assert_eq!(agent.status, AgentStatus::Crashed);
    assert_eq!(agent.pid, None);
    assert!(agent.last_error.expect("error").contains("daemon restarted"));

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn skills_and_import_are_exposed_through_the_api() {
    let harness = harness("lifecycle-skills");
    let source = common::write_skill_package(harness.home.path(), "api-skill", "body");
    let imported = harness
        .manager
        .import_skill(pihub_contracts::SkillImportInput {
            id: "api-skill".to_string(),
            source_path: source,
            source_label: None,
        })
        .await
        .expect("import");
    assert_eq!(imported.id, "api-skill");
    let listed = harness.manager.skills().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "api-skill");
    assert_eq!(listed[0].digest, format!("sha256:{}", listed[0].current_sha256));
    assert_eq!(
        harness.manager.prompts().await.expect("prompts").principles.len(),
        4
    );

    let carriers = harness.manager.carriers().await.expect("carriers");
    assert_eq!(carriers.len(), 3);
    assert!(carriers.iter().all(|carrier| carrier.available));
    let providers = harness.manager.providers().await.expect("providers");
    assert!(!providers.switch.available);
    assert!(providers.apps.pi.is_empty());

    let manager_error = harness
        .manager
        .skill_manager(pihub_contracts::SkillManagerAction::Status, json!({}))
        .await
        .expect_err("no manager connection");
    assert_eq!(manager_error.code, "manager_unavailable");

    common::remove_tree(&harness.home.path().join("skill-home/skills/objects"));
    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn frozen_prompt_survives_a_restart_even_if_the_store_projection_is_lost() {
    let harness = harness("lifecycle-frozen");
    harness
        .manager
        .create_agent(
            harness.spec("agent-frozen", pihub_contracts::Carrier::Codex),
            true,
        )
        .await
        .expect("create");
    let agent = harness.store.get_agent("agent-frozen").expect("get").expect("agent");
    let frozen = harness.manager.frozen_prompt(&agent).expect("frozen prompt");
    assert_eq!(frozen.digest, GOLDEN_CODEX_DIGEST);

    // A record whose column projection is empty must still resolve from the
    // private file written at creation time.
    let mut without_projection = agent.clone();
    without_projection.prompt_config = None;
    let from_file = harness
        .manager
        .frozen_prompt(&without_projection)
        .expect("file fallback");
    assert_eq!(from_file.digest, GOLDEN_CODEX_DIGEST);

    let missing = harness
        .manager
        .frozen_prompt(&pihub_contracts::AgentRecord {
            prompt_config: None,
            ..agent.clone()
        });
    assert!(missing.is_some());

    let summary = harness
        .manager
        .prompt_config_of(&agent, true)
        .expect("summary");
    assert_eq!(summary.prompt_chars, frozen.prompt.encode_utf16().count());
    assert_eq!(summary.mode, pihub_contracts::PromptMode::Append);

    let unknown = pihub_contracts::AgentRecord {
        id: "no-such-agent".to_string(),
        prompt_config: None,
        ..agent
    };
    assert!(harness.manager.frozen_prompt(&unknown).is_none());

    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[tokio::test]
async fn create_verifies_frozen_skill_authorization_before_launch() {
    let harness = harness("lifecycle-skill-verify");
    let source = common::write_skill_package(harness.home.path(), "frozen-skill", "body");
    harness
        .manager
        .import_skill(pihub_contracts::SkillImportInput {
            id: "frozen-skill".to_string(),
            source_path: source,
            source_label: None,
        })
        .await
        .expect("import");
    let mut spec = harness.spec("agent-skill", pihub_contracts::Carrier::Codex);
    spec.skill_policy = Some(SkillPolicy::Only {
        ids: vec!["frozen-skill".to_string()],
    });
    harness
        .manager
        .create_agent(spec, true)
        .await
        .expect("create");
    let agent = harness.store.get_agent("agent-skill").expect("get").expect("agent");
    let authorization = agent.skill_authorization.expect("authorization");
    assert_eq!(authorization.grants.len(), 1);
    assert!(matches!(agent.skills[0], pihub_contracts::SkillRef::Grant { .. }));

    // Tampering with the frozen object makes the next start fail closed.
    let object = authorization.grants[0].path.clone();
    harness.manager.stop_agent("agent-skill").await.expect("stop");
    common::set_mode(&object, 0o700);
    common::set_mode(&object.join("SKILL.md"), 0o600);
    common::write_file(&object.join("SKILL.md"), "tampered\n");
    let error = harness
        .manager
        .restart_agent("agent-skill")
        .await
        .expect_err("tampered skill snapshot");
    assert_eq!(error.code, "skill_snapshot_changed");

    common::remove_tree(&harness.home.path().join("skill-home/skills/objects"));
    harness.manager.close_logs().await;
    harness.store.close().expect("close store");
}

#[test]
fn hub_error_helper_keeps_codes_on_the_wire() {
    let error = HubError::new("agent_not_running", "agent is not running: x");
    let envelope = pihub_contracts::Envelope::<Value>::from_result(Err(error));
    let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
    assert!(serialized.contains("\"agent_not_running\""));
}
