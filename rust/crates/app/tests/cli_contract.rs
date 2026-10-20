//! CLI contract tests.
//!
//! The CLI is a control-plane client, so these tests run it against a real
//! loopback daemon (a small fake here) and assert exit codes, the JSON
//! envelope channel, and exactly which requests reach the daemon.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use pihub_app::{run_cli, LocalOperations};
use pihub_contracts::HubPaths;
use serde_json::{json, Value};
use tokio::net::TcpListener;

const TOKEN: &str = "cli-test-token";

#[derive(Clone)]
struct Recorder {
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    /// Version the fake daemon reports from /v1/health. Empty omits the field.
    version: Arc<Mutex<String>>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            version: Arc::new(Mutex::new("3.0.0.0".to_string())),
        }
    }
}

impl Recorder {
    fn push(&self, path: &str, body: Value) {
        self.requests
            .lock()
            .unwrap()
            .push((path.to_string(), body));
    }

    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn body_for(&self, path: &str) -> Option<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, body)| body.clone())
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"))
}

fn envelope(data: Value) -> Json<Value> {
    Json(json!({ "ok": true, "data": data, "error": null }))
}

async fn health(State(recorder): State<Recorder>) -> Json<Value> {
    let version = recorder.version.lock().unwrap().clone();
    let mut data = json!({
        "name": "pi-subagent-hub",
        "apiVersion": 1,
        "pid": 1234,
    });
    if !version.is_empty() {
        data["version"] = Value::String(version);
    }
    envelope(data)
}

async fn daemon_api(
    State(recorder): State<Recorder>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    // Read the path from the URI so multi-segment routes need no Path tuple.
    let path = uri.path().trim_start_matches('/').to_string();
    if !authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "ok": false,
                "data": null,
                "error": { "code": "unauthorized", "message": "valid local control token required", "retryable": false, "details": null },
            })),
        );
    }
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    recorder.push(&path, body.clone());

    let data = match (method.clone(), path.as_str()) {
        (Method::GET, "v1/agents") => json!([{ "id": "agent-1", "status": "running" }]),
        (Method::GET, "v1/agents/agent-1") => json!({ "id": "agent-1", "status": "running" }),
        (Method::GET, "v2/agents/agent-1") => json!({ "id": "agent-1", "status": "running" }),
        (Method::POST, "v1/agents") | (Method::POST, "v2/agents") => {
            json!({ "id": "agent-created", "status": "provisioning" })
        }
        (Method::POST, _) if path.ends_with("/prompt") => {
            json!({ "kind": "prompt", "message": body.get("message").cloned() })
        }
        (Method::GET, "v2/snapshot") => json!({ "agents": [{ "id": "agent-1" }], "eventCursor": 3 }),
        (Method::GET, "v1/daemon/stop") | (Method::POST, "v1/daemon/stop") => {
            json!({ "stopping": true })
        }
        _ => json!({ "path": path }),
    };
    let status = if method == Method::POST {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (status, envelope(data))
}

async fn start_daemon(recorder: Recorder) -> (u16, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/agents", get(daemon_api).post(daemon_api))
        .route("/v1/agents/{id}", get(daemon_api))
        .route("/v1/agents/{id}/{action}", get(daemon_api).post(daemon_api))
        .route("/v1/daemon/stop", post(daemon_api))
        .route("/v2/snapshot", get(daemon_api))
        .route("/v2/agents", post(daemon_api))
        .route("/v2/agents/{id}", get(daemon_api))
        .route("/v2/agents/{id}/{action}", get(daemon_api).post(daemon_api))
        .route("/v2/carriers", get(daemon_api))
        .route("/v2/providers", get(daemon_api).post(daemon_api))
        .route("/v2/skills", get(daemon_api))
        .route("/v2/prompts", get(daemon_api))
        .route("/v2/skills/manager", get(daemon_api))
        .route("/v2/skills/manager/{action}", post(daemon_api))
        .route("/v2/skills/import", post(daemon_api))
        .route("/v2/providers/{app}/{id}/switch", post(daemon_api))
        .fallback(any(daemon_api))
        .with_state(recorder);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (port, handle)
}

struct Fixture {
    home: tempfile::TempDir,
    paths: HubPaths,
    recorder: Recorder,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A fixture with a live fake daemon and published state files.
async fn online() -> Fixture {
    online_with_version("3.0.0.0").await
}

/// Same, but the fake daemon reports `version` from /v1/health (and metadata).
async fn online_with_version(version: &str) -> Fixture {
    let recorder = Recorder::default();
    *recorder.version.lock().unwrap() = version.to_string();
    let (port, handle) = start_daemon(recorder.clone()).await;
    let home = tempfile::tempdir().unwrap();
    let paths = HubPaths::resolve(home.path(), "/nonexistent/app-root");
    std::fs::create_dir_all(paths.state_dir.as_path()).unwrap();
    std::fs::write(
        &paths.daemon_path,
        json!({
            "pid": std::process::id(),
            "host": "127.0.0.1",
            "port": port,
            "version": version,
            "startedAt": "2026-10-20T10:00:00.000Z",
            "appRoot": "/nonexistent/app-root",
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        &paths.control_path,
        json!({ "token": TOKEN, "createdAt": "2026-10-20T10:00:00.000Z" }).to_string(),
    )
    .unwrap();
    Fixture {
        home,
        paths,
        recorder,
        handle,
    }
}

/// A fixture with no daemon state.
fn offline() -> (tempfile::TempDir, HubPaths) {
    let home = tempfile::tempdir().unwrap();
    let paths = HubPaths::resolve(home.path(), "/nonexistent/app-root");
    (home, paths)
}

async fn run(fixture: &Fixture, args: &[&str]) -> i32 {
    let local = LocalOperations::new(fixture.paths.clone());
    run_cli(
        args.iter().map(|value| value.to_string()).collect(),
        &local,
        &fixture.paths,
    )
    .await
}

#[tokio::test]
async fn unknown_command_exits_one_in_machine_mode() {
    let fixture = online().await;
    assert_eq!(run(&fixture, &["not-a-command", "--json"]).await, 1);
    // The failure never reached the daemon.
    assert!(fixture.recorder.paths().is_empty());
}

#[test]
fn help_text_is_byte_identical_to_the_2_1_cli() {
    // Captured from `node bin/pihub.js --help` of the accepted 2.1 reference.
    let expected = include_str!("fixtures/pihub-help.txt");
    assert_eq!(pihub_app::cli::help::help(), expected);
}

#[tokio::test]
async fn help_and_version_always_exit_zero() {
    let fixture = online().await;
    assert_eq!(run(&fixture, &["--help"]).await, 0);
    assert_eq!(run(&fixture, &["-h"]).await, 0);
    assert_eq!(run(&fixture, &["--version"]).await, 0);
    assert_eq!(run(&fixture, &["-v"]).await, 0);
}

#[tokio::test]
async fn commands_require_a_running_daemon() {
    let (_home, paths) = offline();
    let local = LocalOperations::new(paths.clone());
    let code = run_cli(vec!["list".into(), "--json".into()], &local, &paths).await;
    assert_eq!(code, 1);
    let code = run_cli(
        vec!["carriers".into(), "list".into(), "--json".into()],
        &local,
        &paths,
    )
    .await;
    assert_eq!(code, 1);
}

#[tokio::test]
async fn daemon_status_and_config_work_offline() {
    let (_home, paths) = offline();
    let local = LocalOperations::new(paths.clone());
    assert_eq!(
        run_cli(vec!["daemon".into(), "status".into(), "--json".into()], &local, &paths).await,
        0
    );
    assert_eq!(
        run_cli(vec!["config".into(), "paths".into(), "--json".into()], &local, &paths).await,
        0
    );
    assert_eq!(
        run_cli(vec!["config".into(), "init".into(), "--json".into()], &local, &paths).await,
        0
    );
    assert_eq!(
        run_cli(vec!["config".into(), "validate".into(), "--json".into()], &local, &paths).await,
        0
    );
    // config init created the private AI API files.
    assert!(paths.models_path.exists());
    assert!(paths.auth_path.exists());
}

#[tokio::test]
async fn doctor_keeps_the_node_prerequisite_check_first() {
    // The Pi carrier is a Node bundle, so the first prerequisite check keeps its
    // 2.1 identity instead of an internal runtime marker.
    let (_home, paths) = offline();
    let local = LocalOperations::new(paths.clone());
    let report = local.doctor_report().await.unwrap();
    let checks = report["checks"].as_array().unwrap();
    assert_eq!(checks[0]["name"], "node");
    assert!(
        checks[0]["detail"].as_str().unwrap().starts_with('v')
            || checks[0]["detail"].as_str().unwrap().contains("node"),
        "detail should be a node version or an unavailability reason: {}",
        checks[0]["detail"]
    );
    // `pi-version` only appears when the pinned Pi bundle exists, so compare
    // against the 2.1 sequence with that optional check removed.
    let names: Vec<&str> = checks
        .iter()
        .map(|check| check["name"].as_str().unwrap())
        .filter(|name| *name != "pi-version")
        .collect();
    assert_eq!(
        names,
        vec![
            "node",
            "pi-lock",
            "pi-cli",
            "pi-commit",
            "deployment-boundary",
            "ai-api-config",
            "daemon",
        ],
        "doctor keeps the 2.1 check order with node first"
    );
}

#[tokio::test]
async fn doctor_daemon_check_uses_the_flat_health_shape() {
    // 2.1 reports `daemon.health` ({name,version,apiVersion,pid}) when the
    // daemon answers, and the literal "stopped" otherwise - never the nested
    // status object.
    let fixture = online().await;
    let local = LocalOperations::new(fixture.paths.clone());
    let report = local.doctor_report().await.unwrap();
    let checks = report["checks"].as_array().unwrap();
    let daemon = checks
        .iter()
        .find(|check| check["name"] == "daemon")
        .expect("daemon check present");
    assert_eq!(daemon["ok"], true);
    let detail = &daemon["detail"];
    assert_eq!(detail["name"], "pi-subagent-hub");
    assert_eq!(detail["apiVersion"], 1);
    assert!(detail["pid"].as_u64().unwrap() > 0);
    assert!(
        detail.get("metadata").is_none() && detail.get("running").is_none(),
        "must be the flat health object, not the nested status: {detail}"
    );

    // Offline => the literal string.
    let (_home, paths) = offline();
    let report = LocalOperations::new(paths.clone()).doctor_report().await.unwrap();
    let daemon = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "daemon")
        .expect("daemon check present");
    assert_eq!(daemon["ok"], false);
    assert_eq!(daemon["detail"], "stopped");
}

#[tokio::test]
async fn doctor_reports_checks_without_failing_the_process() {
    let (_home, paths) = offline();
    let local = LocalOperations::new(paths.clone());
    assert_eq!(
        run_cli(vec!["doctor".into(), "--json".into()], &local, &paths).await,
        0
    );
}

#[tokio::test]
async fn list_and_inspect_use_the_v1_namespace() {
    let fixture = online().await;
    assert_eq!(run(&fixture, &["list", "--json"]).await, 0);
    assert_eq!(run(&fixture, &["ps", "--all", "--json"]).await, 0);
    assert_eq!(run(&fixture, &["inspect", "agent-1", "--json"]).await, 0);
    let paths = fixture.recorder.paths();
    assert!(paths.starts_with(&["v1/agents".to_string()]));
}

#[tokio::test]
async fn platform_commands_use_the_v2_namespace() {
    let fixture = online().await;
    assert_eq!(run(&fixture, &["carriers", "list", "--json"]).await, 0);
    assert_eq!(run(&fixture, &["providers", "list", "--json"]).await, 0);
    assert_eq!(run(&fixture, &["prompts", "list", "--json"]).await, 0);
    assert_eq!(run(&fixture, &["skills", "list", "--json"]).await, 0);
    assert_eq!(run(&fixture, &["agents", "list", "--json"]).await, 0);
    assert_eq!(
        run(&fixture, &["agents", "inspect", "agent-1", "--json"]).await,
        0
    );
    let paths = fixture.recorder.paths();
    for expected in [
        "v2/carriers",
        "v2/providers",
        "v2/prompts",
        "v2/skills",
        "v2/snapshot",
        "v2/agents/agent-1",
    ] {
        assert!(
            paths.iter().any(|path| path == expected),
            "missing request to {expected} in {paths:?}"
        );
    }
}

#[tokio::test]
async fn spawn_splits_provider_and_model() {
    let fixture = online().await;
    assert_eq!(
        run(
            &fixture,
            &["spawn", "--name", "reviewer", "--cwd", "/tmp", "--model", "gw/model-a", "--json"],
        )
        .await,
        0
    );
    let body = fixture.recorder.body_for("v1/agents").unwrap();
    assert_eq!(body["provider"], "gw");
    assert_eq!(body["model"], "model-a");
    assert_eq!(body["workspaceMode"], "shared");
    assert_eq!(body["name"], "reviewer");
}

#[tokio::test]
async fn agents_create_sends_platform_fields() {
    let fixture = online().await;
    assert_eq!(
        run(
            &fixture,
            &[
                "agents", "create",
                "--carrier", "codex",
                "--name", "reviewer",
                "--cwd", "/tmp",
                "--skills", "none",
                "--principles", "none",
                "--system-prompt", "be brief",
                "--json",
            ],
        )
        .await,
        0
    );
    let body = fixture.recorder.body_for("v2/agents").unwrap();
    assert_eq!(body["carrier"], "codex");
    assert_eq!(body["skillPolicy"]["mode"], "only");
    assert_eq!(body["skillPolicy"]["ids"], json!([]));
    assert_eq!(body["promptConfig"]["principleIds"], json!([]));
    assert_eq!(body["promptConfig"]["customText"], "be brief");
}

#[tokio::test]
async fn skills_manager_and_import_forward_their_flags() {
    let fixture = online().await;
    assert_eq!(
        run(&fixture, &["skills", "manager", "status", "--json"]).await,
        0
    );
    assert_eq!(
        run(
            &fixture,
            &["skills", "manager", "sync", "--preset", "p1,p2", "--force", "--json"],
        )
        .await,
        0
    );
    assert_eq!(
        run(
            &fixture,
            &["skills", "import", "--id", "skill-a", "--source", "/tmp/skill-a", "--label", "A", "--json"],
        )
        .await,
        0
    );
    let sync = fixture.recorder.body_for("v2/skills/manager/sync").unwrap();
    assert_eq!(sync["selection"]["presetIds"], json!(["p1", "p2"]));
    assert_eq!(sync["force"], true);
    let import = fixture.recorder.body_for("v2/skills/import").unwrap();
    assert_eq!(import["id"], "skill-a");
    assert_eq!(import["sourcePath"], "/tmp/skill-a");
    assert_eq!(import["sourceLabel"], "A");
}

#[tokio::test]
async fn providers_switch_encodes_app_and_id() {
    let fixture = online().await;
    assert_eq!(
        run(&fixture, &["providers", "switch", "--app", "codex", "--id", "team api", "--json"]).await,
        0
    );
    assert!(fixture
        .recorder
        .paths()
        .iter()
        .any(|path| path == "v2/providers/codex/team%20api/switch"));
}

#[tokio::test]
async fn providers_add_requires_stdin_flag() {
    let fixture = online().await;
    assert_eq!(run(&fixture, &["providers", "add", "--json"]).await, 1);
    assert!(fixture.recorder.paths().is_empty());
}

#[tokio::test]
async fn wait_times_out_with_exit_code_four() {
    let fixture = online().await;
    assert_eq!(
        run(&fixture, &["wait", "agent-1", "--timeout", "0.2", "--json"]).await,
        4
    );
    assert_eq!(
        run(&fixture, &["agents", "wait", "agent-1", "--timeout", "0.2", "--json"]).await,
        4
    );
}

#[tokio::test]
async fn daemon_stop_reaches_the_daemon() {
    let fixture = online().await;
    assert_eq!(run(&fixture, &["daemon", "stop", "--json"]).await, 0);
    assert!(fixture
        .recorder
        .paths()
        .iter()
        .any(|path| path == "v1/daemon/stop"));
}

#[tokio::test]
async fn manifest_v1_creates_each_item_and_reports_per_item_errors() {
    let fixture = online().await;
    let manifest = fixture.home.path().join("manifest-v1.json");
    std::fs::write(
        &manifest,
        json!({
            "schemaVersion": 1,
            "groupName": "batch",
            "agents": [
                { "name": "one", "cwd": "/tmp", "model": "gw/model-a" },
                { "name": "two", "cwd": "/tmp", "model": { "provider": "gw", "id": "model-b" } }
            ]
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        run(&fixture, &["spawn", "--manifest", manifest.to_str().unwrap(), "--json"]).await,
        0
    );
    let posts = fixture
        .recorder
        .paths()
        .iter()
        .filter(|path| path.as_str() == "v1/agents")
        .count();
    assert_eq!(posts, 2, "each manifest item is its own request");
}

#[tokio::test]
async fn manifest_v2_rejects_the_wrong_schema_version() {
    let fixture = online().await;
    let manifest = fixture.home.path().join("manifest-bad.json");
    std::fs::write(
        &manifest,
        json!({ "schemaVersion": 1, "agents": [] }).to_string(),
    )
    .unwrap();
    assert_eq!(
        run(
            &fixture,
            &["agents", "create", "--manifest", manifest.to_str().unwrap(), "--json"],
        )
        .await,
        1
    );
    assert!(fixture.recorder.paths().is_empty());
}

#[tokio::test]
async fn prompt_with_stdin_flag_sends_the_message() {
    let fixture = online().await;
    assert_eq!(
        run(&fixture, &["agents", "prompt", "agent-1", "hello", "world", "--json"]).await,
        0
    );
    let body = fixture.recorder.body_for("v2/agents/agent-1/prompt").unwrap();
    assert_eq!(body["message"], "hello world");
}

#[tokio::test]
async fn control_commands_post_to_the_agent_action() {
    let fixture = online().await;
    for action in ["abort", "stop", "restart"] {
        assert_eq!(run(&fixture, &[action, "agent-1", "--json"]).await, 0);
    }
    let paths = fixture.recorder.paths();
    for action in ["abort", "stop", "restart"] {
        assert!(
            paths
                .iter()
                .any(|path| path == &format!("v1/agents/agent-1/{action}")),
            "missing {action} in {paths:?}"
        );
    }
}

#[tokio::test]
async fn cross_major_daemon_still_serves_reads() {
    let fixture = online_with_version("2.0.0.0").await;
    for args in [
        vec!["list", "--json"],
        vec!["ps", "--json"],
        vec!["inspect", "agent-1", "--json"],
        vec!["logs", "agent-1", "--json"],
        vec!["daemon", "status", "--json"],
        vec!["carriers", "list", "--json"],
        vec!["providers", "list", "--json"],
        vec!["prompts", "list", "--json"],
        vec!["skills", "list", "--json"],
        vec!["agents", "list", "--json"],
        vec!["agents", "inspect", "agent-1", "--json"],
        vec!["agents", "logs", "agent-1", "--json"],
    ] {
        assert_eq!(
            run(&fixture, &args).await,
            0,
            "read command {args:?} must keep working against a 2.x daemon"
        );
    }
    // `wait` is a read loop: it may still time out (exit 4) on a busy agent,
    // but it must not be refused by the write guard (exit 1).
    assert_eq!(
        run(&fixture, &["agents", "wait", "agent-1", "--timeout", "0.2", "--json"]).await,
        4,
        "wait must reach the daemon and poll, not be refused"
    );
}

#[tokio::test]
async fn cross_major_daemon_refuses_every_lifecycle_write() {
    let fixture = online_with_version("2.0.0.0").await;

    let writes: Vec<Vec<&str>> = vec![
        vec!["prompt", "agent-1", "hi", "--json"],
        vec!["send", "agent-1", "hi", "--json"],
        vec!["steer", "agent-1", "hi", "--json"],
        vec!["follow-up", "agent-1", "hi", "--json"],
        vec!["abort", "agent-1", "--json"],
        vec!["stop", "agent-1", "--json"],
        vec!["restart", "agent-1", "--json"],
        vec!["agents", "prompt", "agent-1", "hi", "--json"],
        vec!["agents", "abort", "agent-1", "--json"],
        vec!["agents", "stop", "agent-1", "--json"],
        vec!["agents", "restart", "agent-1", "--json"],
        vec!["daemon", "stop", "--json"],
        vec!["spawn", "--name", "n", "--cwd", "/tmp", "--json"],
        vec!["agents", "create", "--carrier", "pi", "--name", "n", "--cwd", "/tmp", "--json"],
        vec!["providers", "switch", "--app", "codex", "--id", "team", "--json"],
        vec!["skills", "import", "--id", "s", "--source", "/tmp/s", "--json"],
        vec!["skills", "manager", "connect", "--json"],
        vec!["skills", "manager", "sync", "--json"],
        vec!["skills", "manager", "disconnect", "--json"],
    ];
    for args in &writes {
        assert_eq!(
            run(&fixture, args).await,
            1,
            "write command {args:?} must be refused against a 2.x daemon"
        );
    }

    // Nothing mutating reached the daemon: only the read endpoints from other
    // tests plus the guard's own /v1/health probes are recorded.
    let recorded = fixture.recorder.paths();
    for forbidden in [
        "v1/daemon/stop",
        "v2/agents",
        "v1/agents",
        "v2/providers",
        "v2/skills/import",
        "v2/skills/manager/connect",
        "v2/skills/manager/sync",
        "v2/skills/manager/disconnect",
    ] {
        assert!(
            !recorded.iter().any(|path| path == forbidden),
            "the guard must stop {forbidden} before it is sent; recorded {recorded:?}"
        );
    }
    assert!(
        !recorded
            .iter()
            .any(|path| path.contains("/prompt") || path.contains("/abort") || path.contains("/stop") || path.contains("/restart")),
        "no lifecycle request may reach a 2.x daemon; recorded {recorded:?}"
    );
}

#[tokio::test]
async fn cross_major_refusal_is_a_clear_hub_error() {
    let fixture = online_with_version("2.0.0.0").await;
    let client = pihub_app::HubClient::connect(&fixture.paths).unwrap();
    let failure = client.ensure_control_compatible().await.unwrap_err();
    assert_eq!(failure.code, "daemon_version_unsupported");
    assert!(
        failure.message.contains("2.0.0.0") && failure.message.contains("3.0.0.0"),
        "message must name both versions: {}",
        failure.message
    );
    assert!(
        failure.message.contains("3.x"),
        "message must state the supported major: {}",
        failure.message
    );

    // Same-major daemons pass the guard.
    let compatible = online_with_version("3.0.0.0").await;
    let client = pihub_app::HubClient::connect(&compatible.paths).unwrap();
    assert!(client.ensure_control_compatible().await.is_ok());
}

#[tokio::test]
async fn unknown_daemon_version_refuses_writes() {
    let fixture = online_with_version("").await;
    // Reads still work.
    assert_eq!(run(&fixture, &["list", "--json"]).await, 0);
    // A write fails closed.
    assert_eq!(run(&fixture, &["agents", "prompt", "agent-1", "hi", "--json"]).await, 1);
    let client = pihub_app::HubClient::connect(&fixture.paths).unwrap();
    let failure = client.ensure_control_compatible().await.unwrap_err();
    assert_eq!(failure.code, "daemon_version_unknown");
    assert!(!fixture
        .recorder
        .paths()
        .iter()
        .any(|path| path.contains("/prompt")));
}

#[tokio::test]
async fn panel_reports_a_url_without_opening_a_browser() {
    // `panel` starts a daemon; with no real daemon entry point in this test
    // binary the start fails fast and reports an error instead of hanging.
    let (_home, paths) = offline();
    let local = LocalOperations::new(paths.clone());
    let code = run_cli(
        vec!["panel".into(), "--no-open".into(), "--json".into()],
        &local,
        &paths,
    )
    .await;
    assert_eq!(code, 1, "a failed daemon start must not exit zero");
}
