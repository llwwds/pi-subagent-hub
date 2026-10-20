//! Server-rendered panel actions.
//!
//! Every panel mutation is a plain HTML form POST carrying the control token in
//! a hidden `_token` field, so the dashboard works with JavaScript disabled.
//! After the mutation the handler redirects back to the SSR page (`303`), which
//! re-reads the Hub through `HubApi`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use pihub_contracts::{
    AgentSpec, ApiView, Carrier, ControlAction, PromptConfigInput, ProviderApp, ProviderInput,
    RuntimeRequestKind, SkillManagerAction, SkillPolicy, ThinkingLevel,
};
use serde_json::json;

use super::{fail, read_form_body, segment, ServerState, Response};
use crate::error;
use crate::forms;

pub(crate) async fn handle(
    state: Arc<ServerState>,
    method: Method,
    path: String,
    request: Request<Body>,
) -> Response {
    let segments: Vec<String> = path
        .trim_matches('/')
        .split('/')
        .filter(|value| !value.is_empty())
        .map(segment)
        .collect();
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();

    if method != Method::POST {
        return fail(
            StatusCode::NOT_FOUND,
            error::CODE_NOT_FOUND,
            "route not found",
            None,
        );
    }
    let fields = match read_form_body(request).await {
        Ok(fields) => fields,
        Err(response) => return response,
    };
    if fields.get("_token") != Some(state.token.as_str()) {
        return fail(
            StatusCode::UNAUTHORIZED,
            error::CODE_UNAUTHORIZED,
            "valid local control token required",
            None,
        );
    }

    match parts.as_slice() {
        ["ui", "agents", "create"] => create_agent(&state, &fields).await,
        ["ui", "agents", id, action] => agent_action(&state, id, action, &fields).await,
        ["ui", "skills", "manager", action] => manager_action(&state, action, &fields).await,
        ["ui", "providers", "add"] => add_provider(&state, &fields).await,
        ["ui", "providers", app, id, "switch"] => switch_provider(&state, app, id).await,
        _ => fail(
            StatusCode::NOT_FOUND,
            error::CODE_NOT_FOUND,
            "route not found",
            None,
        ),
    }
}

async fn create_agent(state: &ServerState, fields: &forms::Fields) -> Response {
    let name = fields.get_trimmed("name").unwrap_or_default().to_string();
    let cwd = fields.get_trimmed("cwd").unwrap_or_default().to_string();
    if name.is_empty() || cwd.is_empty() {
        return back_to_create("名称和工作目录必填。");
    }
    let carrier: Carrier = match fields.get_trimmed("carrier").unwrap_or("pi").parse() {
        Ok(carrier) => carrier,
        Err(hub_error) => return back_to_create(&hub_error.message),
    };
    let provider = fields.get_trimmed("provider").map(str::to_string);
    let manual_model = fields.get_trimmed("model-manual").map(str::to_string);
    let selected_model = fields.get_trimmed("model-choice").map(str::to_string);
    let model_value = manual_model.clone().or(selected_model).filter(|value| !value.is_empty());
    let (provider, model) = match (provider, model_value) {
        (Some(provider), model) => (Some(provider), model),
        (None, Some(model)) => match model.split_once('/') {
            Some((provider, id)) if !provider.is_empty() => {
                (Some(provider.to_string()), Some(id.to_string()))
            }
            _ => (None, Some(model)),
        },
        (None, None) => (None, None),
    };
    let thinking = match fields.get_trimmed("thinking") {
        Some(value) => match value.parse::<ThinkingLevel>() {
            Ok(level) => Some(level),
            Err(hub_error) => return back_to_create(&hub_error.message),
        },
        None => None,
    };
    let skill_policy = match fields.get("skill-mode").unwrap_or("all") {
        "only" => SkillPolicy::Only {
            ids: fields
                .get_all("skill-id")
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        _ => SkillPolicy::All,
    };
    let prompt_config = PromptConfigInput {
        principle_ids: Some(
            fields
                .get_all("principle")
                .into_iter()
                .map(str::to_string)
                .collect(),
        ),
        custom_text: Some(fields.get("system-prompt").unwrap_or_default().to_string()),
    };
    let spec = AgentSpec {
        agent_id: None,
        name: Some(name),
        cwd: Some(cwd.into()),
        workspace_mode: Some("shared".to_string()),
        provider,
        model,
        thinking,
        prompt: fields
            .get_trimmed("prompt")
            .map(str::to_string)
            .filter(|value| !value.is_empty()),
        carrier,
        skill_policy: Some(skill_policy),
        prompt_config: Some(prompt_config),
    };
    match state.api.create(ApiView::V2, spec).await {
        Ok(view) => {
            let id = match &view {
                pihub_contracts::AgentView::V1(agent) => agent.id.clone(),
                pihub_contracts::AgentView::V2(agent) => agent.agent.id.clone(),
            };
            redirect(&format!(
                "/?view=agents&agent={}&toast={}",
                forms::percent_encode(&id),
                forms::percent_encode("Agent 已创建")
            ))
        }
        Err(hub_error) => back_to_create(&hub_error.message),
    }
}

async fn agent_action(
    state: &ServerState,
    id: &str,
    action: &str,
    fields: &forms::Fields,
) -> Response {
    let target = format!("/?view=agents&agent={}", forms::percent_encode(id));
    match action {
        "prompt" | "steer" | "follow-up" => {
            let message = fields.get("message").unwrap_or_default().trim().to_string();
            if message.is_empty() {
                return redirect(&format!("{target}&error={}", forms::percent_encode("消息不能为空")));
            }
            let kind = match action {
                "prompt" => RuntimeRequestKind::Prompt,
                "steer" => RuntimeRequestKind::Steer,
                _ => RuntimeRequestKind::FollowUp,
            };
            match state.api.send(id, kind, &message).await {
                Ok(_) => redirect(&format!("{target}&toast={}", forms::percent_encode("指令已接收"))),
                Err(hub_error) => redirect(&format!(
                    "{target}&error={}",
                    forms::percent_encode(&hub_error.message)
                )),
            }
        }
        "abort" | "stop" | "restart" => {
            let control = match action {
                "abort" => ControlAction::Abort,
                "stop" => ControlAction::Stop,
                _ => ControlAction::Restart,
            };
            let label = match action {
                "abort" => "中止",
                "stop" => "停止",
                _ => "重启",
            };
            // The panel manages platform Agents too, so it must not ask for the
            // legacy view: `ApiView::V1` is an entity gate in core (it applies
            // the legacy-only visibility rule), and using it here made every
            // stop/restart/abort on a /v2 Agent answer 404. 2.1's UI calls the
            // manager directly, i.e. existence only. The legacy response shape
            // is unaffected because control() always returns AgentView::V1.
            match state.api.control(ApiView::V2, id, control).await {
                Ok(_) => redirect(&format!(
                    "{target}&toast={}",
                    forms::percent_encode(&format!("{label}指令已接收"))
                )),
                Err(hub_error) => redirect(&format!(
                    "{target}&error={}",
                    forms::percent_encode(&hub_error.message)
                )),
            }
        }
        _ => fail(
            StatusCode::NOT_FOUND,
            error::CODE_NOT_FOUND,
            "route not found",
            None,
        ),
    }
}

async fn manager_action(state: &ServerState, action: &str, fields: &forms::Fields) -> Response {
    let (kind, label) = match action {
        "connect" => (SkillManagerAction::Connect, "Skills Manager 已连接"),
        "sync" => (SkillManagerAction::Sync, "Skill 快照已同步"),
        "disconnect" => (SkillManagerAction::Disconnect, "已断开 Skills Manager"),
        _ => {
            return fail(
                StatusCode::NOT_FOUND,
                error::CODE_NOT_FOUND,
                "route not found",
                None,
            );
        }
    };
    let mut body = json!({});
    if kind == SkillManagerAction::Status {
        body = json!({ "live": false });
    }
    let selection = {
        let mut selection = serde_json::Map::new();
        for (field, key) in [
            ("preset", "presetIds"),
            ("ids", "ids"),
            ("tags", "tags"),
        ] {
            if let Some(value) = fields.get_trimmed(field) {
                let list: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect();
                selection.insert(key.to_string(), json!(list));
            }
        }
        selection
    };
    if !selection.is_empty()
        && let Some(object) = body.as_object_mut()
    {
        object.insert("selection".to_string(), serde_json::Value::Object(selection));
    }
    if let (SkillManagerAction::Sync, Some(force)) = (kind, fields.get_trimmed("force"))
        && let Some(object) = body.as_object_mut()
    {
        object.insert("force".to_string(), json!(force == "true" || force == "1" || force == "on"));
    }
    match state.api.skill_manager(kind, body).await {
        Ok(_) => redirect(&format!(
            "/?view=skills&toast={}",
            forms::percent_encode(label)
        )),
        Err(hub_error) => redirect(&format!(
            "/?view=skills&error={}",
            forms::percent_encode(&hub_error.message)
        )),
    }
}

async fn add_provider(state: &ServerState, fields: &forms::Fields) -> Response {
    let app: ProviderApp = match fields.get_trimmed("app").unwrap_or("pi").parse() {
        Ok(app) => app,
        Err(hub_error) => return back_to_provider(&hub_error.message),
    };
    let model_list: Vec<String> = fields
        .get("modelList")
        .unwrap_or_default()
        .split(['\n', ','])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut deduped: Vec<String> = Vec::new();
    for model in model_list {
        if !deduped.contains(&model) {
            deduped.push(model);
        }
    }
    if deduped.is_empty() || deduped.len() > 100 {
        return back_to_provider("模型列表需要包含 1 到 100 个模型 ID。");
    }
    let default_model = fields
        .get_trimmed("defaultModel")
        .map(str::to_string)
        .or_else(|| deduped.first().cloned())
        .unwrap_or_default();
    if !default_model.is_empty() && !deduped.contains(&default_model) {
        return back_to_provider("默认模型必须出现在模型列表中。");
    }
    let base_url = fields.get_trimmed("baseUrl").unwrap_or_default().to_string();
    match validate_base_url(&base_url) {
        Ok(()) => {}
        Err(message) => return back_to_provider(&message),
    }
    let auth_mode = match app {
        ProviderApp::Claude => Some(match fields.get_trimmed("authMode").unwrap_or("api-key") {
            "bearer" => pihub_contracts::AuthMode::Bearer,
            _ => pihub_contracts::AuthMode::ApiKey,
        }),
        _ => None,
    };
    let input = ProviderInput {
        app,
        id: fields.get_trimmed("id").unwrap_or_default().to_string(),
        name: fields.get_trimmed("name").unwrap_or_default().to_string(),
        base_url,
        api_key: fields.get("apiKey").unwrap_or_default().to_string(),
        model_list: deduped,
        default_model,
        // A carrier only accepts its own formats; a foreign value (e.g. a Pi
        // format submitted while Codex is selected) resolves to that carrier's
        // default instead of being forwarded.
        api_format: Some(crate::dashboard::normalize_api_format(
            app,
            fields.get_trimmed("apiFormat"),
        )),
        auth_mode,
    };
    match state.api.add_provider(input).await {
        Ok(_) => redirect(&format!(
            "/?view=providers&toast={}",
            forms::percent_encode("供应商已保存")
        )),
        Err(hub_error) => back_to_provider(&format!(
            "保存失败：{}；API Key 不会显示在错误提示中。",
            hub_error.message
        )),
    }
}

async fn switch_provider(state: &ServerState, app: &str, id: &str) -> Response {
    let parsed: ProviderApp = match app.parse() {
        Ok(app) => app,
        Err(hub_error) => {
            return redirect(&format!(
                "/?view=providers&error={}",
                forms::percent_encode(&hub_error.message)
            ));
        }
    };
    match state.api.switch_provider(parsed, id).await {
        Ok(_) => redirect(&format!(
            "/?view=providers&toast={}",
            forms::percent_encode("默认供应商已切换")
        )),
        Err(hub_error) => redirect(&format!(
            "/?view=providers&error={}",
            forms::percent_encode(&hub_error.message)
        )),
    }
}

fn validate_base_url(value: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(value).map_err(|_| "API URL 无效。".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("API URL 必须是未包含凭据和查询参数的 HTTP(S) 地址。".to_string());
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("API URL 必须是未包含凭据和查询参数的 HTTP(S) 地址。".to_string());
    }
    Ok(())
}

fn back_to_create(message: &str) -> Response {
    redirect(&format!(
        "/?view=agents&create=1&error={}",
        forms::percent_encode(message)
    ))
}

fn back_to_provider(message: &str) -> Response {
    redirect(&format!(
        "/?view=providers&create-provider=1&error={}",
        forms::percent_encode(message)
    ))
}

fn redirect(location: &str) -> Response {
    axum::http::response::Builder::new()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(Vec::new()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
