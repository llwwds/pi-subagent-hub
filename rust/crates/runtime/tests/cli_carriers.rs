//! Codex / Claude CLI carrier behaviour: session resume, argv/profile
//! isolation, frozen skill mounts, abort and failure verdicts.

mod support;

use std::path::{Path, PathBuf};

use std::sync::Arc;

use pihub_contracts::{
    Carrier, FrozenPromptConfig, LogSink, PromptMode, Runtime, RuntimeEvent, RuntimeLaunch,
    RuntimeRequestKind,
};
use pihub_runtime::cli_runtime::{CcSwitchRoute, CliRuntime, CliSpec};
use pihub_runtime::env::RuntimeEnv;
use serde_json::json;
use pihub_runtime::protocol_log::{RAW_MAX_BYTES, RAW_MAX_CHARS};
use support::{
    CaptureSink, FAKE_CLI_SCRIPT, MARKER_PROMPT, TempRoot, TurnRecord, collect_until, event_types,
    launch, payload_text, read_turn_records, skill, write_script,
};
use tokio::sync::mpsc;

fn frozen_prompt(carrier: Carrier, text: &str) -> FrozenPromptConfig {
    FrozenPromptConfig {
        schema_version: 1,
        mode: PromptMode::Append,
        carrier,
        channel: None,
        support: None,
        principle_ids: Vec::new(),
        custom_text: String::new(),
        prompt: text.to_string(),
        digest: "sha256:fixture".to_string(),
        resolved_at: None,
    }
}

struct Fixture {
    root: TempRoot,
    codex: PathBuf,
    claude: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = TempRoot::new(label);
        support::project(root.path());
        // The fixture picks its protocol from its own file name, exactly like a
        // real `codex`/`claude` executable installed under its own name.
        let codex = write_script(&root.join("codex"), FAKE_CLI_SCRIPT);
        let claude = write_script(&root.join("claude"), FAKE_CLI_SCRIPT);
        let log = root.join("turn.log");
        Self {
            root,
            codex,
            claude,
            log,
        }
    }

    fn cli_for(&self, carrier: Carrier) -> PathBuf {
        match carrier {
            Carrier::Codex => self.codex.clone(),
            _ => self.claude.clone(),
        }
    }

    fn env(&self) -> RuntimeEnv {
        let mut env = RuntimeEnv::from_process();
        env.insert("FAKE_LOG", self.log.to_string_lossy().into_owned());
        env
    }

    fn spec(&self, launch: RuntimeLaunch) -> CliSpec {
        let executable = self.cli_for(launch.carrier);
        CliSpec {
            launch,
            executable,
            env: self.env(),
            hub_home: Some(self.root.join("hub-home")),
            hub_cli: None,
            node_dir: None,
            cc_switch: None,
            protocol_debug: false,
        }
    }

    fn spec_with_debug(&self, launch: RuntimeLaunch, protocol_debug: bool) -> CliSpec {
        let mut spec = self.spec(launch);
        spec.protocol_debug = protocol_debug;
        spec
    }

    fn records(&self) -> Vec<TurnRecord> {
        read_turn_records(&self.log)
    }
}

async fn harness(carrier: Carrier, spec: CliSpec) -> (CliRuntime, mpsc::Receiver<RuntimeEvent>) {
    harness_with(carrier, spec, None).await
}

async fn harness_with(
    carrier: Carrier,
    spec: CliSpec,
    log: Option<Arc<dyn LogSink>>,
) -> (CliRuntime, mpsc::Receiver<RuntimeEvent>) {
    let (sender, receiver) = mpsc::channel(128);
    let runtime = CliRuntime::new(carrier, spec, sender, log).expect("runtime");
    (runtime, receiver)
}

async fn settle(receiver: &mut mpsc::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
    collect_until(receiver, |event| {
        matches!(event, RuntimeEvent::Event { event_type, .. } if event_type == "agent_settled")
    })
    .await
}

#[tokio::test]
async fn codex_resumes_its_private_session_and_emits_normalized_messages() {
    let fixture = Fixture::new("codex");
    let mut spec = launch(fixture.root.path(), Carrier::Codex, "agent-codex");
    spec.skills = vec![skill(fixture.root.path(), "test-skill")];
    spec.prompt = Some(frozen_prompt(Carrier::Codex, "follow the principles"));
    let state_dir = spec.state_dir.clone();
    let (runtime, mut events) = harness(Carrier::Codex, fixture.spec(spec)).await;

    let state = runtime.start().await.expect("start");
    assert!(!state.is_streaming);
    let model = state.model.expect("model");
    assert_eq!(model.provider, "openai");
    assert_eq!(model.id, "unknown");
    assert_eq!(state.thinking, None);
    assert!(runtime.is_alive());
    assert_eq!(runtime.pid(), None, "no turn is running yet");

    let mut messages = Vec::new();
    for message in ["first", "second"] {
        let response = runtime
            .request(RuntimeRequestKind::Prompt, json!({"message": message}))
            .await
            .expect("prompt accepted");
        assert_eq!(response["success"], true);
        assert_eq!(response["data"]["accepted"], true);
        assert!(runtime.pid().is_some(), "a turn owns a pid");
        let collected = settle(&mut events).await;
        assert!(
            event_types(&collected).contains(&"agent_settled".to_string()),
            "no settle for {message}: {:?}",
            event_types(&collected)
        );
        messages.push(payload_text(&collected, "message_end").unwrap_or_default());
    }
    assert_eq!(messages, vec!["codex:first", "codex:second"]);
    assert_eq!(runtime.pid(), None, "the turn released its pid");

    let records = fixture.records();
    assert_eq!(records.len(), 2, "one process per turn");
    assert_eq!(records[0].prompt, "first");
    assert_eq!(records[1].prompt, "second");

    // First turn creates the session, second resumes it.
    assert!(records[0].has_arg("exec"));
    assert!(!records[0].has_arg("resume"));
    assert!(records[1].has_arg("resume"));
    assert!(records[1].has_arg("11111111-1111-4111-8111-111111111111"));
    // `resume <session> -` : the session id directly precedes the stdin marker.
    let tail = &records[1].args[records[1].args.len().saturating_sub(2)..];
    assert_eq!(
        tail,
        ["11111111-1111-4111-8111-111111111111".to_string(), "-".to_string()]
    );

    // Isolation: private home, codex cwd is the private home, project via --add-dir.
    for record in &records {
        assert!(
            Path::new(&record.home).starts_with(&state_dir),
            "HOME must be private: {}",
            record.home
        );
        assert_eq!(record.pwd, record.home, "codex runs in its private home");
        assert_eq!(
            record.arg_after("--add-dir"),
            Some(
                fixture
                    .root
                    .join("project")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(Path::new(&record.codex_home).starts_with(&state_dir));
        assert!(
            Path::new(&record.home)
                .join(".agents/skills/test-skill/SKILL.md")
                .exists(),
            "frozen skill mount"
        );
        assert!(
            std::fs::read_to_string(Path::new(&record.home).join("AGENTS.md"))
                .unwrap_or_default()
                .contains("user-selected project directory"),
            "private workspace note"
        );
        // Codex receives the extra developer layer, never a replaced prompt.
        let developer = record
            .arg_after("-c")
            .expect("developer instructions flag");
        assert!(
            developer.starts_with("developer_instructions=\""),
            "{developer}"
        );
    }

    runtime.stop().await.expect("stop");
    assert!(!runtime.is_alive());
}

#[tokio::test]
async fn claude_uses_bare_mode_its_own_mount_root_and_resume() {
    let fixture = Fixture::new("claude");
    let mut spec = launch(fixture.root.path(), Carrier::Claude, "agent-claude");
    spec.skills = vec![skill(fixture.root.path(), "test-skill")];
    spec.prompt = Some(frozen_prompt(Carrier::Claude, "append me"));
    let state_dir = spec.state_dir.clone();
    let project = spec.cwd.clone();
    let session_id = spec.session_id.clone();
    let (runtime, mut events) = harness(Carrier::Claude, fixture.spec(spec)).await;

    let state = runtime.start().await.expect("start");
    assert_eq!(state.model.expect("model").provider, "anthropic");

    let mut messages = Vec::new();
    for message in ["first", "second"] {
        runtime
            .request(RuntimeRequestKind::Prompt, json!({"message": message}))
            .await
            .expect("prompt accepted");
        messages.push(payload_text(&settle(&mut events).await, "message_end").unwrap_or_default());
    }
    assert_eq!(messages, vec!["claude:first", "claude:second"]);

    let records = fixture.records();
    assert_eq!(records.len(), 2);
    assert!(records[0].has_arg("--bare"));
    assert_eq!(records[0].arg_after("--permission-mode"), Some("dontAsk"));
    let allowed = records[0].arg_after("--allowedTools").expect("allowed tools");
    for tool in ["Read", "Edit", "Write", "Bash", "Skill"] {
        assert!(allowed.split(',').any(|entry| entry == tool), "{tool} missing");
    }
    assert_eq!(records[0].arg_after("--session-id"), Some(session_id.as_str()));
    assert!(!records[0].has_arg("--resume"));
    assert_eq!(
        records[1].arg_after("--resume"),
        Some("22222222-2222-4222-8222-222222222222")
    );
    assert_eq!(
        records[0].arg_after("--append-system-prompt"),
        Some("append me")
    );
    assert_eq!(records[0].pwd, project.to_string_lossy());

    // The skill mount root is a separate add-dir, never the credential home.
    let added = records[0].arg_after("--add-dir").expect("add-dir");
    assert!(Path::new(added).join(".claude/skills/test-skill/SKILL.md").exists());
    assert_ne!(Path::new(added).to_string_lossy(), records[0].claude_home);
    assert!(Path::new(&records[0].claude_home).starts_with(&state_dir));

    runtime.stop().await.expect("stop");
}

#[tokio::test]
async fn aborting_a_turn_keeps_the_runtime_resumable() {
    let fixture = Fixture::new("abort");
    let spec = launch(fixture.root.path(), Carrier::Codex, "agent-abort");
    let (runtime, mut events) = harness(Carrier::Codex, fixture.spec(spec)).await;
    runtime.start().await.expect("start");

    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "hang"}))
        .await
        .expect("prompt accepted");
    let response = runtime
        .request(RuntimeRequestKind::Abort, json!({}))
        .await
        .expect("abort");
    assert_eq!(response["data"]["aborted"], true);

    let collected = settle(&mut events).await;
    let settled = collected.iter().find_map(|event| match event {
        RuntimeEvent::Event {
            event_type,
            payload,
        } if event_type == "agent_settled" => Some(payload.clone()),
        _ => None,
    });
    assert_eq!(
        settled.and_then(|payload| payload.get("aborted").cloned()),
        Some(json!(true))
    );
    assert!(runtime.is_alive(), "an aborted turn must not kill the runtime");
    assert_eq!(runtime.pid(), None);

    // The next turn still starts (and would resume the persisted session).
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "after"}))
        .await
        .expect("prompt accepted");
    settle(&mut events).await;
    // The aborted turn may be killed before it ever writes (the same race the
    // 2.1 implementation has); what matters is that the runtime accepted and
    // completed a fresh turn afterwards.
    let records = fixture.records();
    assert!(
        records.iter().any(|record| record.prompt == "after"),
        "the follow-up turn must have run: {:?}",
        std::fs::read_to_string(&fixture.log)
    );
    runtime.stop().await.expect("stop");
}

#[tokio::test]
async fn a_turn_without_a_final_result_is_reported_as_a_process_error() {
    let fixture = Fixture::new("empty");
    let spec = launch(fixture.root.path(), Carrier::Codex, "agent-empty");
    let (runtime, mut events) = harness(Carrier::Codex, fixture.spec(spec)).await;
    runtime.start().await.expect("start");

    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "empty"}))
        .await
        .expect("prompt accepted");
    let collected = collect_until(&mut events, |event| {
        matches!(event, RuntimeEvent::Exit { .. })
    })
    .await;
    let message = collected
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ProcessError { message } => Some(message.clone()),
            _ => None,
        })
        .expect("process error");
    assert!(
        message.contains("no final result"),
        "unexpected message: {message}"
    );
    assert!(event_types(&collected).contains(&"exit".to_string()));
    assert!(!runtime.is_alive());
}

#[tokio::test]
async fn a_second_prompt_during_a_turn_is_refused() {
    let fixture = Fixture::new("busy");
    let spec = launch(fixture.root.path(), Carrier::Codex, "agent-busy");
    let (runtime, _events) = harness(Carrier::Codex, fixture.spec(spec)).await;
    runtime.start().await.expect("start");
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "hang"}))
        .await
        .expect("first prompt accepted");

    let error = runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "again"}))
        .await
        .expect_err("second prompt must be refused");
    assert_eq!(error.code, "carrier_busy");
    assert!(error.message.contains("already processing a turn"));

    let state = runtime
        .request(RuntimeRequestKind::GetState, json!({}))
        .await
        .expect("state");
    assert_eq!(state["data"]["isStreaming"], true);

    runtime.stop().await.expect("stop");
    assert!(!runtime.is_alive());
}

#[tokio::test]
async fn unsupported_verbs_and_invalid_configuration_fail_loudly() {
    let fixture = Fixture::new("validation");
    let mut spec = launch(fixture.root.path(), Carrier::Claude, "agent-validation");
    spec.thinking = Some(pihub_contracts::ThinkingLevel::Off);
    let (runtime, _events) = harness(Carrier::Claude, fixture.spec(spec)).await;
    let error = runtime.start().await.expect_err("thinking 'off' is not a Claude level");
    assert_eq!(error.code, "invalid_thinking");
    assert!(error.message.contains("does not support thinking level off"));

    let mut spec = launch(fixture.root.path(), Carrier::Codex, "agent-validation");
    spec.provider = Some("route-a".to_string());
    let (runtime, _events) = harness(Carrier::Codex, fixture.spec(spec)).await;
    let error = runtime.start().await.expect_err("provider without CC Switch route");
    assert_eq!(error.code, "invalid_provider");
    assert!(error.message.contains("does not support provider route-a"));

    let spec = launch(fixture.root.path(), Carrier::Codex, "agent-validation");
    let (runtime, _events) = harness(Carrier::Codex, fixture.spec(spec)).await;
    runtime.start().await.expect("start");
    let error = runtime
        .request(RuntimeRequestKind::Steer, json!({"message": "x"}))
        .await
        .expect_err("CLI carriers cannot steer");
    assert_eq!(error.code, "unsupported_request");
    assert!(error.message.contains("does not support steer"));
    runtime.stop().await.expect("stop");
}

#[tokio::test]
async fn cc_switch_turns_use_the_private_route_and_never_inherit_a_key() {
    for carrier in [Carrier::Codex, Carrier::Claude] {
        let fixture = Fixture::new("switch");
        let hub_cli = fixture.root.join("app/bin/pihub.js");
        std::fs::create_dir_all(hub_cli.parent().expect("parent")).expect("app bin");
        std::fs::write(&hub_cli, "#!/bin/sh\nexit 0\n").expect("hub cli");
        let route = CcSwitchRoute {
            executable: fixture.cli_for(carrier),
            config_dir: fixture.root.join("cc-switch/data"),
            home: fixture.root.join("cc-switch/home"),
            pi_dir: fixture.root.join("cc-switch/pi-agent"),
            codex_cli_path: fixture.codex.clone(),
            claude_cli_path: fixture.claude.clone(),
        };

        let mut spec = launch(fixture.root.path(), carrier, "agent-switch");
        spec.provider = Some("route-a".to_string());
        let state_dir = spec.state_dir.clone();
        let launch_spec = CliSpec {
            launch: spec,
            executable: fixture.cli_for(carrier),
            env: {
                let mut env = fixture.env();
                env.insert("ANTHROPIC_API_KEY", "must-not-inherit");
                env
            },
            hub_home: Some(fixture.root.join("hub-home")),
            hub_cli: Some(hub_cli.clone()),
            node_dir: None,
            cc_switch: Some(route.clone()),
            protocol_debug: false,
        };
        let (runtime, mut events) = harness(carrier, launch_spec).await;
        runtime.start().await.expect("start");

        runtime
            .request(RuntimeRequestKind::Prompt, json!({"message": "hello"}))
            .await
            .expect("prompt accepted");
        settle(&mut events).await;

        // A CC Switch turn sees only the private whitelist, so the fixture logs
        // into its private HOME instead of an inherited variable.
        let private_log = state_dir
            .join(format!("{}-home", carrier.as_str()))
            .join("pihub-turn.log");
        let records = read_turn_records(&private_log);
        let record = records.first().expect("one turn");
        assert_eq!(record.args[0], "start");
        assert_eq!(record.args[1], carrier.as_str());
        assert_eq!(record.args[2], "route-a");
        if carrier == Carrier::Codex {
            assert!(record.has_arg("--shared-sessions"));
        } else {
            assert!(record.has_arg("--bare"));
        }
        assert_eq!(record.anthropic_key, "", "host key must not be inherited");
        assert_eq!(
            record.hub_home,
            fixture.root.join("hub-home").to_string_lossy()
        );
        assert!(Path::new(&record.home).starts_with(&state_dir));
        assert!(Path::new(&record.codex_home).starts_with(&state_dir));
        assert!(Path::new(&record.claude_home).starts_with(&state_dir));

        // The private `pihub` route is first on PATH and points at this build.
        let head = record.path.split(':').next().expect("path head");
        assert_eq!(
            Path::new(head),
            Path::new(&record.home).join(".bin"),
            "PATH must start with the private bin dir"
        );
        let link = Path::new(head).join("pihub");
        assert_eq!(
            std::fs::read_link(&link).expect("pihub link"),
            hub_cli,
            "the private route must target this platform CLI"
        );
        runtime.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn codex_rejects_per_agent_thinking_on_the_cc_switch_route() {
    let fixture = Fixture::new("switch-thinking");
    let route = CcSwitchRoute {
        executable: fixture.codex.clone(),
        config_dir: fixture.root.join("cc-switch/data"),
        home: fixture.root.join("cc-switch/home"),
        pi_dir: fixture.root.join("cc-switch/pi-agent"),
        codex_cli_path: fixture.codex.clone(),
        claude_cli_path: fixture.claude.clone(),
    };
    let mut spec = launch(fixture.root.path(), Carrier::Codex, "agent-switch-thinking");
    spec.provider = Some("route-a".to_string());
    spec.thinking = Some(pihub_contracts::ThinkingLevel::High);
    let launch_spec = CliSpec {
        launch: spec,
        executable: fixture.codex.clone(),
        env: fixture.env(),
        hub_home: Some(fixture.root.join("hub-home")),
        hub_cli: None,
        node_dir: None,
        cc_switch: Some(route),
        protocol_debug: false,
    };
    let (runtime, _events) = harness(Carrier::Codex, launch_spec).await;
    let error = runtime.start().await.expect_err("thinking is provider-owned");
    assert_eq!(error.code, "invalid_thinking");
    assert!(error.message.contains("reasoning effort"));
}

#[tokio::test]
async fn cli_hub_log_keeps_the_prompt_out_in_normal_mode_and_bounds_debug_raw() {
    // Normal mode: no prompt body, no `data`, only protocol summaries.
    let fixture = Fixture::new("cli-log-normal");
    let sink = CaptureSink::new();
    let spec = fixture.spec_with_debug(
        launch(fixture.root.path(), Carrier::Codex, "agent-log"),
        false,
    );
    let (runtime, mut events) = harness_with(Carrier::Codex, spec, Some(sink.clone())).await;
    runtime.start().await.expect("start");
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": MARKER_PROMPT}))
        .await
        .expect("prompt accepted");
    settle(&mut events).await;
    runtime.stop().await.expect("stop");

    let dump = sink.dump();
    assert!(!dump.contains(MARKER_PROMPT), "prompt body leaked: {dump}");
    assert!(sink.data_payloads().is_empty(), "normal mode sets no data payload");
    let kinds = sink.kinds();
    assert!(kinds.iter().any(|kind| kind == "protocol"), "{kinds:?}");
    assert!(!kinds.iter().any(|kind| kind == "protocol_raw"), "{kinds:?}");
    // Outgoing carrier frames are summarized too (2 frames: thread.started, item.completed, turn.completed).
    assert!(dump.contains("\"type\":\"prompt\""), "{dump}");

    // Debug mode: raw frames, still bounded.
    let fixture = Fixture::new("cli-log-debug");
    let sink = CaptureSink::new();
    let spec = fixture.spec_with_debug(
        launch(fixture.root.path(), Carrier::Codex, "agent-log-debug"),
        true,
    );
    let (runtime, mut events) = harness_with(Carrier::Codex, spec, Some(sink.clone())).await;
    runtime.start().await.expect("start");
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": MARKER_PROMPT}))
        .await
        .expect("prompt accepted");
    settle(&mut events).await;
    runtime.stop().await.expect("stop");

    let kinds = sink.kinds();
    assert!(kinds.iter().any(|kind| kind == "protocol_raw"), "{kinds:?}");
    for payload in sink.data_payloads() {
        assert!(payload.chars().count() <= RAW_MAX_CHARS + 16, "{payload}");
        assert!(payload.len() <= RAW_MAX_BYTES + 64);
    }
    assert!(sink.dump().contains(MARKER_PROMPT), "debug raw keeps the frame");
}
