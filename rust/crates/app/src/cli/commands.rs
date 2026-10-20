//! Command implementations.
//!
//! Every Hub operation goes through the loopback control plane, so the CLI has
//! exactly the same "daemon must be running" semantics as 2.1 and never shares
//! the store with the daemon.

use std::time::{Duration, Instant};

use pihub_contracts::{ApiView, HubError, HubPaths, HubResult, LocalOps};
use serde_json::{json, Value};

use crate::cli::args::Parsed;
use crate::cli::manifest::{create_from_manifest, ManifestKind};
use crate::client::HubClient;
use crate::error;

/// `wait` polling cadence and default timeout (`src/cli.js:263`).
const WAIT_POLL: Duration = Duration::from_millis(250);
const WAIT_DEFAULT_SECONDS: f64 = 120.0;
const SETTLED: [&str; 5] = ["idle", "stopped", "crashed", "failed", "waiting_input"];

#[derive(Debug, Clone)]
pub enum Outcome {
    Value(Value),
    Text(String),
}

impl Outcome {
    pub fn value(self) -> Value {
        match self {
            Outcome::Value(value) => value,
            Outcome::Text(text) => Value::String(text),
        }
    }
}

fn value(value: Value) -> HubResult<Outcome> {
    Ok(Outcome::Value(value))
}

/// Dispatch one already-tokenised command.
pub async fn dispatch(
    command: &str,
    subcommand: Option<&str>,
    parsed: &Parsed,
    local: &dyn LocalOps,
    paths: &HubPaths,
) -> HubResult<Outcome> {
    match command {
        "daemon" => daemon_command(subcommand, local).await,
        "doctor" => value(local.doctor().await?),
        "config" => config_command(subcommand, local).await,
        "panel" => panel_command(parsed, local).await,
        "carriers" => match subcommand {
            Some("list") => {
                let client = HubClient::connect(paths)?;
                value(
                    client
                        .request(reqwest::Method::GET, "/v2/carriers", None)
                        .await?,
                )
            }
            _ => Err(error::command_failed("carriers requires list")),
        },
        "providers" => providers_command(subcommand, parsed, paths).await,
        "skills" => skills_command(subcommand, parsed, paths).await,
        "prompts" => match subcommand {
            Some("list") => {
                let client = HubClient::connect(paths)?;
                value(
                    client
                        .request(reqwest::Method::GET, "/v2/prompts", None)
                        .await?,
                )
            }
            _ => Err(error::command_failed("prompts requires list")),
        },
        "agents" => agents_command(subcommand, parsed, paths).await,
        "spawn" => spawn_command(parsed, paths).await,
        "list" | "ps" => {
            let client = HubClient::connect(paths)?;
            let all = if parsed.is_bool_set("all") { "true" } else { "false" };
            value(
                client
                    .request(
                        reqwest::Method::GET,
                        &format!("/v1/agents?all={all}"),
                        None,
                    )
                    .await?,
            )
        }
        "inspect" => {
            let id = positional_id(parsed, "inspect")?;
            let client = HubClient::connect(paths)?;
            value(
                client
                    .request(
                        reqwest::Method::GET,
                        &format!("/v1/agents/{}", encode(&id)),
                        None,
                    )
                    .await?,
            )
        }
        "prompt" | "send" | "steer" | "follow-up" => {
            let client = HubClient::connect(paths)?;
            send_message(&client, "/v1", parsed, command).await
        }
        "abort" | "stop" | "restart" => {
            let client = HubClient::connect(paths)?;
            control_agent(&client, "/v1", parsed, command).await
        }
        "wait" => {
            let timeout = parsed
                .value("timeout")
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(WAIT_DEFAULT_SECONDS);
            let client = HubClient::connect(paths)?;
            wait_for(&client, "/v1", &parsed.positional, timeout).await
        }
        "logs" => {
            let client = HubClient::connect(paths)?;
            events(&client, "/v1", parsed).await
        }
        other => Err(error::command_failed(format!("unknown command: {other}"))),
    }
}

async fn daemon_command(subcommand: Option<&str>, local: &dyn LocalOps) -> HubResult<Outcome> {
    match subcommand {
        Some("start") => value(local.daemon_start().await?),
        Some("status") => {
            let status = local.daemon_status().await?;
            let mut payload = json!({
                "running": status.running,
                "metadata": status.metadata,
            });
            if let Some(object) = payload.as_object_mut() {
                if let Some(health) = status.health {
                    object.insert(
                        "health".to_string(),
                        serde_json::to_value(health).unwrap_or(Value::Null),
                    );
                }
                if let Some(error) = status.error {
                    object.insert("error".to_string(), Value::String(error));
                }
            }
            value(payload)
        }
        Some("stop") => value(local.daemon_stop().await?),
        _ => Err(error::command_failed(
            "daemon requires start, stop, or status",
        )),
    }
}

async fn config_command(subcommand: Option<&str>, local: &dyn LocalOps) -> HubResult<Outcome> {
    match subcommand {
        Some("init") => value(local.config_init().await?),
        Some("validate") => value(local.config_validate().await?),
        Some("paths") => value(local.config_paths().await?),
        _ => Err(error::command_failed(
            "config requires init, validate, or paths",
        )),
    }
}

async fn panel_command(parsed: &Parsed, local: &dyn LocalOps) -> HubResult<Outcome> {
    let started = local.daemon_start().await?;
    let port = started
        .get("metadata")
        .and_then(|metadata| metadata.get("port"))
        .and_then(Value::as_u64)
        .ok_or_else(|| error::command_failed("daemon did not publish a port"))?;
    let url = format!("http://127.0.0.1:{port}/");
    let no_open = parsed.is_bool_set("no-open");
    let opened = !no_open && cfg!(target_os = "macos");
    if opened {
        let _ = std::process::Command::new("open")
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    value(json!({ "url": url, "opened": opened }))
}

async fn providers_command(
    subcommand: Option<&str>,
    parsed: &Parsed,
    paths: &HubPaths,
) -> HubResult<Outcome> {
    match subcommand {
        Some("list") => {
            let client = HubClient::connect(paths)?;
            value(
                client
                    .request(reqwest::Method::GET, "/v2/providers", None)
                    .await?,
            )
        }
        Some("add") => {
            if !parsed.is_bool_set("stdin") {
                return Err(error::command_failed(
                    "providers add requires --stdin so credentials do not enter command history",
                ));
            }
            let raw = crate::cli::read_stdin()?;
            let body: Value = serde_json::from_str(&raw).map_err(|cause| {
                error::invalid(format!("provider JSON is invalid: {cause}"))
            })?;
            let client = HubClient::connect(paths)?;
            value(
                client
                    .control_request("/v2/providers", Some(&body))
                    .await?,
            )
        }
        Some("switch") => {
            let app = parsed.value("app").ok_or_else(|| {
                error::command_failed("providers switch requires --app and --id")
            })?;
            let id = parsed.value("id").ok_or_else(|| {
                error::command_failed("providers switch requires --app and --id")
            })?;
            let client = HubClient::connect(paths)?;
            value(
                client
                    .control_request(&format!("/v2/providers/{}/{}/switch", encode(app), encode(id)),
                        Some(&json!({})),
                    )
                    .await?,
            )
        }
        _ => Err(error::command_failed(
            "providers requires list, add, or switch",
        )),
    }
}

async fn skills_command(
    subcommand: Option<&str>,
    parsed: &Parsed,
    paths: &HubPaths,
) -> HubResult<Outcome> {
    match subcommand {
        Some("list") => {
            let client = HubClient::connect(paths)?;
            value(
                client
                    .request(reqwest::Method::GET, "/v2/skills", None)
                    .await?,
            )
        }
        Some("import") => {
            let id = parsed
                .value("id")
                .ok_or_else(|| error::command_failed("skills import requires --id and --source"))?;
            let source = parsed.value("source").ok_or_else(|| {
                error::command_failed("skills import requires --id and --source")
            })?;
            let mut body = json!({ "id": id, "sourcePath": source });
            if let Some(label) = parsed.value("label")
                && let Some(object) = body.as_object_mut()
            {
                object.insert("sourceLabel".to_string(), Value::String(label.to_string()));
            }
            let client = HubClient::connect(paths)?;
            value(
                client
                    .control_request("/v2/skills/import", Some(&body))
                    .await?,
            )
        }
        Some("manager") => {
            let action = parsed.positional.first().map(String::as_str).unwrap_or("status");
            let client = HubClient::connect(paths)?;
            if action == "status" {
                let live = if parsed.is_bool_set("live") { "true" } else { "false" };
                return value(
                    client
                        .request(
                            reqwest::Method::GET,
                            &format!("/v2/skills/manager?live={live}"),
                            None,
                        )
                        .await?,
                );
            }
            if !matches!(action, "connect" | "sync" | "disconnect") {
                return Err(error::command_failed(
                    "skills manager requires status, connect, sync, or disconnect",
                ));
            }
            let mut body = serde_json::Map::new();
            for (flag, key) in [
                ("cli-path", "cliPath"),
                ("manager-base-dir", "managerBaseDir"),
                ("manager-skills-dir", "managerSkillsDir"),
                ("agent-key", "agentKey"),
                ("source", "source"),
            ] {
                if let Some(value) = parsed.value(flag) {
                    body.insert(key.to_string(), Value::String(value.to_string()));
                }
            }
            let mut selection = serde_json::Map::new();
            for (flag, key) in [
                ("preset", "presetIds"),
                ("ids", "ids"),
                ("tags", "tags"),
            ] {
                if let Some(values) = parsed.csv(flag) {
                    selection.insert(key.to_string(), json!(values));
                }
            }
            if !selection.is_empty() {
                body.insert("selection".to_string(), Value::Object(selection));
            }
            if action == "sync" && parsed.is_bool_set("force") {
                body.insert("force".to_string(), Value::Bool(true));
            }
            if action == "disconnect" && parsed.is_bool_set("purge-snapshot") {
                body.insert("purgeSnapshot".to_string(), Value::Bool(true));
            }
            value(
                client
                    .control_request(&format!("/v2/skills/manager/{action}"),
                        Some(&Value::Object(body)),
                    )
                    .await?,
            )
        }
        _ => Err(error::command_failed(
            "skills requires list, import, or manager",
        )),
    }
}

async fn agents_command(
    subcommand: Option<&str>,
    parsed: &Parsed,
    paths: &HubPaths,
) -> HubResult<Outcome> {
    let client = HubClient::connect(paths)?;
    match subcommand {
        Some("create") => {
            if let Some(manifest) = parsed.value("manifest") {
                return value(create_from_manifest(&client, manifest, ManifestKind::V2).await?);
            }
            let spec = crate::cli::spec::normalize_platform_spec(parsed)?;
            let body = serde_json::to_value(spec)?;
            value(
                client
                    .control_request("/v2/agents", Some(&body))
                    .await?,
            )
        }
        Some("list") => {
            let snapshot = client
                .request(reqwest::Method::GET, "/v2/snapshot", None)
                .await?;
            value(snapshot.get("agents").cloned().unwrap_or(Value::Array(Vec::new())))
        }
        Some("inspect") => {
            let id = positional_id(parsed, "agents inspect")?;
            value(
                client
                    .request(
                        reqwest::Method::GET,
                        &format!("/v2/agents/{}", encode(&id)),
                        None,
                    )
                    .await?,
            )
        }
        Some("prompt") => send_message(&client, "/v2", parsed, "prompt").await,
        Some("wait") => {
            let timeout = parsed
                .value("timeout")
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(WAIT_DEFAULT_SECONDS);
            wait_for(&client, "/v2", &parsed.positional, timeout).await
        }
        Some("logs") => events(&client, "/v2", parsed).await,
        Some(action @ ("abort" | "stop" | "restart")) => {
            control_agent(&client, "/v2", parsed, action).await
        }
        _ => Err(error::command_failed(
            "agents requires create, list, inspect, prompt, wait, logs, abort, stop, or restart",
        )),
    }
}

async fn spawn_command(parsed: &Parsed, paths: &HubPaths) -> HubResult<Outcome> {
    let client = HubClient::connect(paths)?;
    if let Some(manifest) = parsed.value("manifest") {
        return value(create_from_manifest(&client, manifest, ManifestKind::V1).await?);
    }
    let spec = crate::cli::spec::normalize_create_spec(parsed)?;
    let body = serde_json::to_value(spec)?;
    value(
        client
            .control_request("/v1/agents", Some(&body))
            .await?,
    )
}

async fn send_message(
    client: &HubClient,
    namespace: &str,
    parsed: &Parsed,
    command: &str,
) -> HubResult<Outcome> {
    let mut positional = parsed.positional.clone();
    if positional.is_empty() {
        return Err(error::command_failed(match namespace {
            "/v2" => "agents prompt requires AGENT_ID",
            _ => "prompt requires AGENT_ID",
        }));
    }
    let id = positional.remove(0);
    let action = if command == "send" { "prompt" } else { command };
    let message = if parsed.is_bool_set("stdin") {
        crate::cli::read_stdin()?
    } else {
        positional.join(" ")
    };
    value(
        client
            .control_request(&format!("{namespace}/agents/{}/{action}", encode(&id)),
                Some(&json!({ "message": message })),
            )
            .await?,
    )
}

async fn control_agent(
    client: &HubClient,
    namespace: &str,
    parsed: &Parsed,
    action: &str,
) -> HubResult<Outcome> {
    let id = positional_id(parsed, action)?;
    value(
        client
            .control_request(&format!("{namespace}/agents/{}/{action}", encode(&id)),
                Some(&json!({})),
            )
            .await?,
    )
}

async fn events(client: &HubClient, namespace: &str, parsed: &Parsed) -> HubResult<Outcome> {
    let id = positional_id(parsed, "logs")?;
    let after = parsed.value("after").unwrap_or("0");
    value(
        client
            .request(
                reqwest::Method::GET,
                &format!("{namespace}/agents/{}/events?after={after}", encode(&id)),
                None,
            )
            .await?,
    )
}

async fn wait_for(
    client: &HubClient,
    namespace: &str,
    ids: &[String],
    timeout_seconds: f64,
) -> HubResult<Outcome> {
    if ids.is_empty() {
        return Err(error::command_failed(match namespace {
            "/v2" => "agents wait requires AGENT_ID",
            _ => "wait requires AGENT_ID",
        }));
    }
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(0.0));
    loop {
        let mut agents = Vec::with_capacity(ids.len());
        for id in ids {
            agents.push(
                client
                    .request(
                        reqwest::Method::GET,
                        &format!("{namespace}/agents/{}", encode(id)),
                        None,
                    )
                    .await?,
            );
        }
        let settled = agents.iter().all(|agent| {
            agent
                .get("status")
                .and_then(Value::as_str)
                .map(|status| SETTLED.contains(&status))
                .unwrap_or(false)
        });
        if settled {
            return value(Value::Array(agents));
        }
        if Instant::now() >= deadline {
            return Err(HubError::new(
                "wait_timeout",
                format!("timed out waiting for agents after {timeout_seconds}s"),
            )
            .with_exit_code(4));
        }
        tokio::time::sleep(WAIT_POLL).await;
    }
}

fn positional_id(parsed: &Parsed, label: &str) -> HubResult<String> {
    parsed
        .positional
        .first()
        .cloned()
        .ok_or_else(|| error::command_failed(format!("{label} requires AGENT_ID")))
}

fn encode(value: &str) -> String {
    crate::forms::percent_encode(value)
}

/// Exposed for tests: which API namespace a command uses.
pub fn namespace_for(command: &str) -> ApiView {
    match command {
        "agents" => ApiView::V2,
        _ => ApiView::V1,
    }
}
