//! Server-rendered control panel.
//!
//! The 2.1 dashboard was a client-side SPA that re-derived every view from the
//! `/v2` JSON API. This module renders the same three views on the server:
//! all status derivation, labels, summaries and permission rendering happen in
//! Rust, and the browser only submits plain forms and reloads the page.

use pihub_contracts::{
    AgentViewV2, ApiView, CarrierDiagnostic, EventQuery, FrozenPromptConfig, HubApi, HubResult,
    PromptCatalog, PromptSummary, Provider, ProviderApp, ProviderSnapshot, PublicAgent,
    SkillManagerAction, SkillSummary, Snapshot, StoredEvent,
};
use serde_json::Value;

use crate::forms::Fields;

mod view;
pub use view::*;

/// Gather everything the panel needs and render it.
pub async fn render(fields: &Fields, api: &dyn HubApi, token: &str) -> HubResult<String> {
    let mut warnings: Vec<String> = Vec::new();

    let snapshot = match api.snapshot(ApiView::V2).await {
        Ok(Snapshot::V2(snapshot)) => Some(snapshot),
        Ok(Snapshot::V1(_)) => None,
        Err(error) => {
            warnings.push(error.message);
            None
        }
    };
    let carriers = api.carriers().await.unwrap_or_default();
    let skills = match api.skills().await {
        Ok(skills) => skills,
        Err(error) => {
            warnings.push(error.message);
            Vec::new()
        }
    };
    let prompts = match api.prompts().await {
        Ok(prompts) => prompts,
        Err(error) => {
            warnings.push(error.message);
            PromptCatalog {
                principles: Vec::new(),
                default_principle_ids: Vec::new(),
            }
        }
    };
    let manager = match api
        .skill_manager(SkillManagerAction::Status, serde_json::json!({ "live": false }))
        .await
    {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(error.message);
            None
        }
    };
    let providers = match api.providers().await {
        Ok(providers) => Some(providers),
        Err(error) => {
            warnings.push(error.message);
            None
        }
    };

    let agents: Vec<AgentViewV2> = snapshot
        .as_ref()
        .map(|snapshot| snapshot.agents.clone())
        .unwrap_or_default();
    let selected_id = fields
        .get_trimmed("agent")
        .map(str::to_string)
        .filter(|id| agents.iter().any(|agent| &agent.agent.id == id))
        .or_else(|| {
            fields
                .get_trimmed("agent")
                .map(str::to_string)
                .filter(|id| !id.is_empty())
        });

    let mut detail_events: Vec<StoredEvent> = Vec::new();
    if let Some(id) = selected_id.as_deref()
        && let Some(snapshot) = snapshot.as_ref()
    {
        let after = (snapshot.event_cursor - 1200).max(0);
        let query = EventQuery {
            agent_id: Some(id.to_string()),
            after,
            limit: 1200,
            legacy_only: false,
        };
        match api.events(ApiView::V2, query).await {
            Ok(events) => detail_events = events,
            Err(error) => warnings.push(error.message),
        }
    }

    // Resolve frozen prompt text for the selected Agent only; `/v2` list views
    // never carry prompt bodies.
    let frozen: Option<FrozenPromptConfig> = match selected_id.as_deref() {
        Some(id) => match api.agent(ApiView::V2, id).await {
            Ok(view) => frozen_from_view(&view),
            Err(_) => None,
        },
        None => None,
    };

    let query = QueryState {
        view: fields.get_trimmed("view").unwrap_or("agents").to_string(),
        agent: selected_id.clone(),
        q: fields.get("q").unwrap_or_default().to_string(),
        carrier: fields.get("carrier").unwrap_or_default().to_string(),
        status: fields.get("status").unwrap_or_default().to_string(),
        verbose: fields.get("verbose") == Some("1"),
    };

    let view = PanelView::build(PanelInputs {
        token,
        query,
        snapshot,
        carriers,
        skills,
        prompts,
        manager,
        providers,
        agents,
        detail_events,
        frozen,
        create_open: fields.is_set("create"),
        provider_open: fields.is_set("create-provider"),
        toast: fields.get("toast").unwrap_or_default().to_string(),
        error: fields.get("error").unwrap_or_default().to_string(),
        warnings,
    });
    view.render_markup()
}

/// Minimal fallback page used when the panel itself cannot be gathered.
pub fn render_error_page(message: &str, token: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta name=\"pihub-token\" content=\"{token}\"><title>Subagent Hub · 面板读取失败</title>\
<link rel=\"stylesheet\" href=\"/styles.css\"></head>\
<body><main><section class=\"page-view\"><span class=\"overline\">PANEL ERROR</span>\
<h2>面板读取失败</h2><p class=\"view-intro\">{}</p>\
<p><a class=\"button button-secondary\" href=\"/\">重试</a></p></section></main></body></html>",
        escape_html(message),
        token = escape_html(token)
    )
}

pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn frozen_from_view(view: &pihub_contracts::AgentView) -> Option<FrozenPromptConfig> {
    let summary: Option<PromptSummary> = match view {
        pihub_contracts::AgentView::V2(agent) => agent.prompt_config.clone(),
        pihub_contracts::AgentView::V1(_) => None,
    };
    let summary = summary?;
    // `None` above means the row predates frozen prompt config (legacy). Once a
    // summary exists it IS the frozen record, including the deliberate
    // "mode=none, nothing appended" outcome, which the digest proves was
    // resolved at creation. Only a summary without a digest is treated as
    // unrecorded, so a new agent is never mislabelled as legacy.
    if summary.digest.trim().is_empty() {
        return None;
    }
    let prompt = summary.prompt.clone().unwrap_or_default();
    Some(FrozenPromptConfig {
        schema_version: 1,
        mode: summary.mode,
        carrier: summary.carrier.unwrap_or(pihub_contracts::Carrier::Pi),
        channel: summary.channel,
        support: summary.support,
        principle_ids: summary.principle_ids.clone(),
        custom_text: summary.custom_text.clone().unwrap_or_default(),
        prompt,
        digest: summary.digest.clone(),
        resolved_at: summary.resolved_at.clone(),
    })
}

pub struct PanelInputs<'a> {
    token: &'a str,
    query: QueryState,
    snapshot: Option<pihub_contracts::SnapshotV2>,
    carriers: Vec<CarrierDiagnostic>,
    skills: Vec<SkillSummary>,
    prompts: PromptCatalog,
    manager: Option<Value>,
    providers: Option<ProviderSnapshot>,
    agents: Vec<AgentViewV2>,
    detail_events: Vec<StoredEvent>,
    frozen: Option<FrozenPromptConfig>,
    create_open: bool,
    provider_open: bool,
    toast: String,
    error: String,
    warnings: Vec<String>,
}

/// Shared display helpers.
pub(crate) fn status_meta(status: &str) -> (&'static str, &'static str) {
    match status {
        "provisioning" => ("准备中", "pending"),
        "starting" => ("启动中", "pending"),
        "running" => ("执行中", "running"),
        "idle" => ("待命", "idle"),
        "waiting_input" => ("等待输入", "waiting_input"),
        "stopping" => ("停止中", "pending"),
        "stopped" => ("已停止", "stopped"),
        "crashed" => ("崩溃", "failed"),
        "failed" => ("失败", "failed"),
        _ => ("未知", "unknown"),
    }
}

pub(crate) fn time_label(value: &str) -> String {
    if value.is_empty() {
        return "—".to_string();
    }
    match chrono::DateTime::parse_from_rfc3339(value) {
        Ok(parsed) => parsed
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M:%S")
            .to_string(),
        Err(_) => value.to_string(),
    }
}

pub(crate) fn short(value: &str, length: usize) -> String {
    if value.chars().count() > length {
        let truncated: String = value.chars().take(length).collect();
        format!("{truncated}…")
    } else if value.is_empty() {
        "—".to_string()
    } else {
        value.to_string()
    }
}

pub(crate) fn lifecycle_label(event_type: &str) -> Option<&'static str> {
    Some(match event_type {
        "agent_created" => "已创建",
        "agent_ready" => "已就绪",
        "agent_start" | "agent_started" => "开始执行",
        "agent_settled" => "进入待命",
        "status_changed" => "状态变化",
        "extension_ui_request" => "等待输入",
        "stop_requested" => "请求停止",
        "restart_requested" => "请求重启",
        "process_exit" => "进程退出",
        "agent_stopped" => "已停止",
        "agent_start_failed" => "启动失败",
        "process_error" => "进程异常",
        "agent_failed" => "运行失败",
        _ => return None,
    })
}

/// Port of the 2.1 `eventSummary` helper.
pub(crate) fn event_summary(event: &StoredEvent) -> String {
    let payload = &event.payload;
    let event_type = event.event_type.as_str();
    if event_type == "message_end"
        || payload.get("message").map(Value::is_object).unwrap_or(false)
    {
        if let Some(content) = payload.get("message").and_then(|value| value.get("content")) {
            if let Some(text) = content.as_str() {
                return short(text, 180);
            }
            if let Some(parts) = content.as_array() {
                let joined: Vec<String> = parts
                    .iter()
                    .map(|part| {
                        part.as_str()
                            .map(str::to_string)
                            .or_else(|| {
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .unwrap_or_default()
                    })
                    .filter(|text| !text.is_empty())
                    .collect();
                if !joined.is_empty() {
                    return short(&joined.join(" "), 180);
                }
            }
        }
    }
    if event_type == "message_update" {
        let update = payload
            .get("assistantMessageEvent")
            .cloned()
            .unwrap_or(Value::Null);
        for key in ["delta", "content", "type"] {
            if let Some(text) = update.get(key).and_then(Value::as_str) {
                return text.to_string();
            }
        }
        return "消息更新".to_string();
    }
    if event_type.starts_with("tool_execution") {
        let name = payload
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("工具");
        let error = payload
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return format!("{name}{}", if error { " · 错误" } else { "" });
    }
    if event_type == "agent_created" {
        let mode = payload
            .get("workspaceMode")
            .and_then(Value::as_str)
            .unwrap_or("shared");
        let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
        return format!("{mode} · {cwd}");
    }
    if event_type == "agent_ready" {
        let pid = payload
            .get("pid")
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".to_string());
        let session = payload
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        return format!("PID {pid} · session {}", short(session, 18));
    }
    if let Some(message) = payload.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    if let Some(status) = payload.get("status").and_then(Value::as_str) {
        return format!("状态：{}", status_meta(status).0);
    }
    if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
        return kind.to_string();
    }
    String::new()
}

pub(crate) fn carrier_label(carriers: &[CarrierDiagnostic], id: &str) -> String {
    carriers
        .iter()
        .find(|carrier| carrier.id.as_str() == id)
        .map(|carrier| carrier.display_name.clone())
        .unwrap_or_else(|| match id {
            "pi" => "Pi Agent CLI".to_string(),
            "codex" => "Codex CLI".to_string(),
            "claude" => "Claude Code CLI".to_string(),
            other => other.to_string(),
        })
}

pub(crate) fn model_label(agent: &PublicAgent) -> String {
    let provider = agent
        .provider
        .as_deref()
        .filter(|value| !value.is_empty() && *value != "unknown");
    let model = agent
        .model
        .as_deref()
        .filter(|value| !value.is_empty() && *value != "unknown");
    match (provider, model) {
        (Some(provider), Some(model)) => format!("{provider}/{model}"),
        (None, Some(model)) => model.to_string(),
        (Some(provider), None) => provider.to_string(),
        (None, None) => "载体默认".to_string(),
    }
}

pub(crate) fn provider_ready(provider: &Provider) -> bool {
    provider.configured && provider.ready
}

pub(crate) fn safe_base_url(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "未配置".to_string();
    };
    match reqwest::Url::parse(value) {
        Ok(url) => format!("{}{}", url.origin().ascii_serialization(), url.path()),
        Err(_) => "地址不可用".to_string(),
    }
}

pub(crate) fn app_label(app: ProviderApp) -> &'static str {
    match app {
        ProviderApp::Pi => "Pi Agent CLI",
        ProviderApp::Codex => "Codex CLI",
        ProviderApp::Claude => "Claude Code CLI",
    }
}

pub(crate) fn providers_for<'a>(
    snapshot: Option<&'a ProviderSnapshot>,
    app: ProviderApp,
) -> &'a [Provider] {
    let Some(snapshot) = snapshot else {
        return &[];
    };
    match app {
        ProviderApp::Pi => &snapshot.apps.pi,
        ProviderApp::Codex => &snapshot.apps.codex,
        ProviderApp::Claude => &snapshot.apps.claude,
    }
}

/// Describe the authorization that is actually frozen on the Agent.
///
/// The stored shape is not proof of operator intent: a request for "all" can be
/// resolved into a concrete grant set, so a `Only`-shaped record with one grant
/// must not be worded as "the operator chose exactly this one". The label
/// therefore counts the frozen grants; `all` additionally says the set was
/// frozen at creation time.
pub(crate) fn skill_policy_label(agent: &AgentViewV2) -> String {
    match agent.skill_policy.mode {
        pihub_contracts::SkillPolicyViewMode::Only => {
            if agent.skill_ids.is_empty() {
                "未冻结 Skill".to_string()
            } else {
                format!("已冻结 {} 个 Skill（创建时固定）", agent.skill_ids.len())
            }
        }
        pihub_contracts::SkillPolicyViewMode::All => {
            format!("全部 {} 个 Skill（创建时冻结）", agent.skill_ids.len())
        }
        pihub_contracts::SkillPolicyViewMode::Legacy => "旧版 Profile 授权".to_string(),
    }
}
