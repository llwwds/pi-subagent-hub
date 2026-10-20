//! Pi RPC carrier behaviour against a fake NDJSON peer.

mod support;

use std::time::Duration;

use pihub_contracts::{Runtime, RuntimeEvent, RuntimeRequestKind};
use pihub_runtime::env::RuntimeEnv;
use pihub_runtime::pi_rpc::{PiRpcProcess, PiSpec};
use serde_json::json;
use pihub_runtime::protocol_log::{RAW_MAX_BYTES, RAW_MAX_CHARS};
use support::{
    CaptureSink, MARKER_PROMPT, TempRoot, collect_until, event_types, launch, node_program,
    payload_text,
};
use tokio::sync::mpsc;

async fn settle_events(events: &mut mpsc::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
    collect_until(events, |event| {
        matches!(event, RuntimeEvent::Event { event_type, .. } if event_type == "agent_settled")
    })
    .await
}

fn pi_spec(root: &TempRoot) -> PiSpec {
    pi_spec_with(root, false)
}

fn pi_spec_with(root: &TempRoot, protocol_debug: bool) -> PiSpec {
    support::project(root.path());
    let mut launch = launch(root.path(), pihub_contracts::Carrier::Pi, "agent-one");
    launch.skills = vec![support::skill(root.path(), "gallery-skill")];
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-pi-rpc.js");
    let path = std::env::var("PATH").unwrap_or_default();
    PiSpec {
        launch,
        program: node_program(),
        cli_path: fixture,
        env: RuntimeEnv::from_strs([("PATH", path), ("HOME", root.join("home").to_string_lossy().into_owned())]),
        protocol_debug,
    }
}

async fn harness(root: &TempRoot) -> (PiRpcProcess, mpsc::Receiver<RuntimeEvent>) {
    let (sender, receiver) = mpsc::channel(128);
    let runtime = PiRpcProcess::new(pi_spec(root), sender, None);
    (runtime, receiver)
}

async fn harness_with(
    spec: PiSpec,
    log: Option<std::sync::Arc<dyn pihub_contracts::LogSink>>,
) -> (PiRpcProcess, mpsc::Receiver<RuntimeEvent>) {
    let (sender, receiver) = mpsc::channel(128);
    let runtime = PiRpcProcess::new(spec, sender, log);
    (runtime, receiver)
}

#[tokio::test]
async fn start_correlates_requests_and_streams_isolated_events() {
    let root = TempRoot::new("pi-basic");
    let (runtime, mut events) = harness(&root).await;

    let state = runtime.start().await.expect("start");
    assert_eq!(state.session_id.as_deref(), Some("fake-session"));
    let model = state.model.expect("model");
    assert_eq!(model.provider, "fake");
    assert_eq!(model.id, "fake-model");
    assert!(!state.is_streaming);
    assert!(runtime.is_alive());
    assert!(runtime.pid().is_some());

    let response = runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "hello"}))
        .await
        .expect("prompt accepted");
    assert_eq!(response["type"], "response");
    assert_eq!(response["success"], true);
    assert_eq!(response["command"], "prompt");

    let collected = collect_until(&mut events, |event| {
        matches!(event, RuntimeEvent::Event { event_type, .. } if event_type == "agent_settled")
    })
    .await;
    let types = event_types(&collected);
    assert!(types.contains(&"agent_start".to_string()), "{types:?}");
    assert!(types.contains(&"message_update".to_string()), "{types:?}");
    assert!(types.contains(&"message_end".to_string()), "{types:?}");
    assert!(types.contains(&"agent_settled".to_string()), "{types:?}");
    assert_eq!(
        payload_text(&collected, "message_end").as_deref(),
        Some("fake hello")
    );

    runtime.stop().await.expect("stop");
    let exit = collect_until(&mut events, |event| {
        matches!(event, RuntimeEvent::Exit { .. })
    })
    .await;
    assert!(!runtime.is_alive());
    assert!(event_types(&exit).contains(&"exit".to_string()));
}

#[tokio::test]
async fn the_argv_keeps_every_isolation_flag_and_a_frozen_skill() {
    let root = TempRoot::new("pi-argv");
    let (runtime, _events) = harness(&root).await;
    runtime.start().await.expect("start");
    let response = runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "one"}))
        .await
        .expect("prompt");
    assert_eq!(response["success"], true);

    // The fake peer records the argv it received inside the session dir.
    let session_dir = root.join("agents/agent-one/sessions");
    let log = std::fs::read_to_string(session_dir.join("commands.jsonl")).expect("command log");
    let first: serde_json::Value =
        serde_json::from_str(log.lines().next().expect("first command")).expect("json");
    let argv: Vec<String> = first["_argv"]
        .as_array()
        .expect("argv")
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(argv.contains(&"--mode".to_string()));
    assert_eq!(argv[argv.iter().position(|a| a == "--mode").unwrap() + 1], "rpc");
    assert!(argv.contains(&"--no-skills".to_string()));
    assert!(argv.contains(&"--no-extensions".to_string()));
    assert!(argv.contains(&"--no-approve".to_string()));
    assert_eq!(
        argv[argv.iter().position(|a| a == "--tools").unwrap() + 1],
        "read"
    );
    assert_eq!(
        argv[argv.iter().position(|a| a == "--session-id").unwrap() + 1],
        "33333333-3333-4333-8333-333333333333"
    );
    let skill_path = argv
        .iter()
        .position(|a| a == "--skill")
        .map(|index| argv[index + 1].clone())
        .expect("skill flag");
    assert!(skill_path.ends_with("gallery-skill"), "{skill_path}");

    runtime.stop().await.expect("stop");
}

#[tokio::test]
async fn process_exit_rejects_every_pending_request_with_the_2_1_message() {
    let root = TempRoot::new("pi-exit");
    let (runtime, mut events) = harness(&root).await;
    runtime.start().await.expect("start");

    let error = runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "crash"}))
        .await
        .expect_err("pending request must be rejected");
    assert_eq!(error.code, "pi_process_exited");
    assert!(
        error
            .message
            .contains("Pi process exited before response (code=1, signal=null)"),
        "{}",
        error.message
    );

    let exit = collect_until(&mut events, |event| {
        matches!(event, RuntimeEvent::Exit { .. })
    })
    .await;
    assert!(event_types(&exit).contains(&"exit".to_string()));
    assert!(!runtime.is_alive());
    assert_eq!(runtime.pid(), None);

    // A request after the exit fails fast instead of hanging.
    let after = runtime
        .request(RuntimeRequestKind::GetState, json!({}))
        .await
        .expect_err("not running");
    assert_eq!(after.code, "pi_not_running");
}

#[tokio::test]
async fn a_verb_without_a_response_times_out_instead_of_hanging() {
    let root = TempRoot::new("pi-timeout");
    let (runtime, _events) = harness(&root).await;
    runtime.start().await.expect("start");
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "hang"}))
        .await
        .expect("prompt accepted");

    // The fake peer deliberately never answers `abort` after a hang prompt.
    let started = std::time::Instant::now();
    let error = runtime
        .request(RuntimeRequestKind::Abort, json!({}))
        .await
        .expect_err("abort must time out");
    assert_eq!(error.code, "pi_rpc_timeout");
    assert_eq!(error.message, "Pi RPC timeout for abort");
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "the 5s timeout must actually be applied: {:?}",
        started.elapsed()
    );

    // The runtime is still usable afterwards and shuts down cleanly.
    runtime.stop().await.expect("stop");
}

#[tokio::test]
async fn malformed_output_is_reported_as_a_protocol_error_and_does_not_kill_the_run() {
    let root = TempRoot::new("pi-protocol");
    let (runtime, mut events) = harness(&root).await;
    runtime.start().await.expect("start");
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": "garbage"}))
        .await
        .expect("prompt accepted");

    let collected = collect_until(&mut events, |event| {
        matches!(event, RuntimeEvent::ProtocolError { .. })
    })
    .await;
    let error = collected
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ProtocolError { message, line } => Some((message.clone(), line.clone())),
            _ => None,
        })
        .expect("protocol error");
    assert_eq!(error.1.as_deref(), Some("this is not json"));
    assert!(runtime.is_alive(), "a bad line must not end the runtime");
    runtime.stop().await.expect("stop");
}

#[tokio::test]
async fn two_agents_keep_their_sessions_events_and_processes_separate() {
    let root = TempRoot::new("pi-isolation");
    support::project(root.path());

    let build = |provider: &str| -> PiSpec {
        let mut launch = launch(root.path(), pihub_contracts::Carrier::Pi, "agent-one");
        launch.provider = Some(provider.to_string());
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("fake-pi-rpc.js");
        PiSpec {
            launch,
            program: node_program(),
            cli_path: fixture,
            env: RuntimeEnv::from_strs([
                ("PATH", std::env::var("PATH").unwrap_or_default()),
                ("HOME", root.join("home").to_string_lossy().into_owned()),
            ]),
            protocol_debug: false,
        }
    };

    let mut other_launch = build("provider-b").launch;
    other_launch.agent_id = "agent-two".to_string();
    other_launch.state_dir = root.join("agents/agent-two/state");
    other_launch.session_dir = root.join("agents/agent-two/sessions");
    other_launch.tmp_dir = root.join("agents/agent-two/tmp");
    other_launch.agent_dir = root.join("agents/agent-two/pi-agent");
    for directory in [
        other_launch.state_dir.clone(),
        other_launch.session_dir.clone(),
        other_launch.tmp_dir.clone(),
        other_launch.agent_dir.clone(),
    ] {
        std::fs::create_dir_all(directory).expect("dir");
    }
    let other_spec = PiSpec {
        launch: other_launch,
        ..build("provider-a")
    };

    let (sender_a, mut events_a) = mpsc::channel(64);
    let (sender_b, mut events_b) = mpsc::channel(64);
    let first = PiRpcProcess::new(build("provider-a"), sender_a, None);
    let second = PiRpcProcess::new(other_spec, sender_b, None);

    first.start().await.expect("first start");
    second.start().await.expect("second start");
    assert_ne!(first.pid(), second.pid());

    first
        .request(RuntimeRequestKind::Prompt, json!({"message": "alpha"}))
        .await
        .expect("first prompt");
    second
        .request(RuntimeRequestKind::Prompt, json!({"message": "beta"}))
        .await
        .expect("second prompt");

    let collected_a = collect_until(&mut events_a, |event| {
        matches!(event, RuntimeEvent::Event { event_type, .. } if event_type == "agent_settled")
    })
    .await;
    let collected_b = collect_until(&mut events_b, |event| {
        matches!(event, RuntimeEvent::Event { event_type, .. } if event_type == "agent_settled")
    })
    .await;
    assert_eq!(
        payload_text(&collected_a, "message_end").as_deref(),
        Some("fake alpha")
    );
    assert_eq!(
        payload_text(&collected_b, "message_end").as_deref(),
        Some("fake beta")
    );

    first.stop().await.expect("stop first");
    second.stop().await.expect("stop second");
    assert!(!first.is_alive());
    assert!(!second.is_alive());
}

#[tokio::test]
async fn starting_twice_is_rejected() {
    let root = TempRoot::new("pi-double-start");
    let (runtime, _events) = harness(&root).await;
    runtime.start().await.expect("start");
    let error = runtime.start().await.expect_err("second start");
    assert_eq!(error.code, "pi_already_started");
    runtime.stop().await.expect("stop");
}

#[tokio::test]
async fn hub_log_keeps_prompt_bodies_out_in_normal_mode() {
    let root = TempRoot::new("pi-log-normal");
    let sink = CaptureSink::new();
    let (runtime, mut events) = harness_with(pi_spec_with(&root, false), Some(sink.clone())).await;
    runtime.start().await.expect("start");
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": MARKER_PROMPT}))
        .await
        .expect("prompt accepted");
    settle_events(&mut events).await;
    runtime.stop().await.expect("stop");

    let dump = sink.dump();
    assert!(
        !dump.contains(MARKER_PROMPT),
        "the prompt body must never reach hub.log: {dump}"
    );
    // Summaries carry identity/size only, and `data` is never populated.
    assert!(sink.data_payloads().is_empty(), "normal mode sets no data payload");
    let kinds = sink.kinds();
    assert!(kinds.iter().any(|kind| kind == "protocol"), "{kinds:?}");
    assert!(!kinds.iter().any(|kind| kind == "protocol_raw"), "{kinds:?}");
    assert!(dump.contains("\"type\":\"prompt\""), "{dump}");
    assert!(dump.contains("\"bytes\":"), "{dump}");
}

#[tokio::test]
async fn raw_protocol_logging_is_opt_in_and_bounded() {
    let root = TempRoot::new("pi-log-debug");
    let sink = CaptureSink::new();
    let (runtime, mut events) = harness_with(pi_spec_with(&root, true), Some(sink.clone())).await;
    runtime.start().await.expect("start");
    let long_prompt = format!("{MARKER_PROMPT}{}", "x".repeat(5_000));
    runtime
        .request(RuntimeRequestKind::Prompt, json!({"message": long_prompt}))
        .await
        .expect("prompt accepted");
    settle_events(&mut events).await;
    runtime.stop().await.expect("stop");

    let kinds = sink.kinds();
    assert!(
        kinds.iter().any(|kind| kind == "protocol_raw"),
        "debug mode must opt in explicitly: {kinds:?}"
    );
    let payloads = sink.data_payloads();
    assert!(!payloads.is_empty());
    for payload in &payloads {
        assert!(
            payload.chars().count() <= RAW_MAX_CHARS + 16,
            "raw payload must stay bounded: {} chars",
            payload.chars().count()
        );
        assert!(payload.len() <= RAW_MAX_BYTES + 64, "raw payload must stay bounded");
    }
    assert!(
        sink.dump().contains(MARKER_PROMPT),
        "the opt-in debug mode records the frame"
    );
}
