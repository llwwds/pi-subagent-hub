//! `/v1` and `/v2` route dispatch.
//!
//! Matching order mirrors `src/http-server.js`: the platform block first, then
//! the legacy block, then `POST /v1/daemon/stop`. Any method/path pair that the
//! 2.1 server would not handle yields `404 not_found`; an unknown Agent on a
//! route that does match yields `404 agent_not_found`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use pihub_contracts::{
    AgentSpec, AgentView, ApiView, ControlAction, EventQuery, ProviderInput, PublicAgent,
    RuntimeRequestKind, SkillImportInput, SkillManagerAction, Snapshot, SnapshotV1, SnapshotV2,
};
use serde_json::{json, Value};

use super::{
    created, fail, hub_failure, is, ok, query_i64, query_usize, read_json_body, segment,
    ServerState,
};
use super::Response;
use crate::error;
use crate::forms;

/// Runtime verbs exposed by the message routes.
const MESSAGE_ACTIONS: [&str; 3] = ["prompt", "steer", "follow-up"];

pub(crate) async fn handle(
    state: Arc<ServerState>,
    method: Method,
    path: String,
    request: Request<Body>,
) -> Response {
    let fields = forms::Fields::parse(request.uri().query().unwrap_or(""));
    let segments: Vec<String> = path
        .trim_matches('/')
        .split('/')
        .filter(|value| !value.is_empty())
        .map(segment)
        .collect();
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();

    match parts.as_slice() {
        ["v2", "snapshot"] if is(&method, &Method::GET) => v2_snapshot(&state, &fields).await,
        ["v2", "events"] if is(&method, &Method::GET) => {
            v2_events(&state, &fields, None).await
        }
        ["v2", "carriers"] if is(&method, &Method::GET) => v2_carriers(&state).await,
        ["v2", "providers"] if is(&method, &Method::GET) => v2_providers(&state).await,
        ["v2", "providers"] if is(&method, &Method::POST) => {
            v2_add_provider(&state, request).await
        }
        ["v2", "providers", app, id, "switch"] if is(&method, &Method::POST) => {
            v2_switch_provider(&state, app, id).await
        }
        ["v2", "prompts"] if is(&method, &Method::GET) => v2_prompts(&state).await,
        ["v2", "skills"] if is(&method, &Method::GET) => v2_skills(&state).await,
        ["v2", "skills", "manager"] if is(&method, &Method::GET) => {
            v2_skill_manager_status(&state, &fields).await
        }
        ["v2", "skills", "manager", action] if is(&method, &Method::POST) => {
            v2_skill_manager_action(&state, action, request).await
        }
        ["v2", "skills", "import"] if is(&method, &Method::POST) => {
            v2_import_skill(&state, request).await
        }
        ["v2", "agents"] if is(&method, &Method::POST) => v2_create_agent(&state, request).await,
        ["v2", "agents", id] => v2_agent(&state, &method, id, "", &fields, request).await,
        ["v2", "agents", id, action] => {
            v2_agent(&state, &method, id, action, &fields, request).await
        }
        ["v1", "snapshot"] if is(&method, &Method::GET) => v1_snapshot(&state).await,
        ["v1", "agents"] if is(&method, &Method::GET) => v1_list_agents(&state, &fields).await,
        ["v1", "agents"] if is(&method, &Method::POST) => v1_create_agent(&state, request).await,
        ["v1", "events"] if is(&method, &Method::GET) => v1_events(&state, &fields).await,
        ["v1", "agents", id] => v1_agent(&state, &method, id, "", &fields, request).await,
        ["v1", "agents", id, action] => {
            v1_agent(&state, &method, id, action, &fields, request).await
        }
        ["v1", "daemon", "stop"] if is(&method, &Method::POST) => daemon_stop(&state).await,
        _ => route_not_found(),
    }
}

fn route_not_found() -> Response {
    fail(
        StatusCode::NOT_FOUND,
        error::CODE_NOT_FOUND,
        "route not found",
        None,
    )
}

async fn v2_snapshot(state: &ServerState, _fields: &forms::Fields) -> Response {
    match state.api.snapshot(ApiView::V2).await {
        Ok(Snapshot::V2(snapshot)) => ok(to_value(&snapshot)),
        // The view shape is owned by core. Fabricating an empty platform view
        // from a legacy snapshot would silently report zero Agents, so the
        // mismatch is reported instead of flattened.
        Ok(Snapshot::V1(_)) => fail(
            StatusCode::BAD_REQUEST,
            "unsupported_view",
            "v2 snapshot requires the platform view; core returned the legacy view",
            None,
        ),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_events(
    state: &ServerState,
    fields: &forms::Fields,
    agent_id: Option<String>,
) -> Response {
    let query = EventQuery {
        agent_id,
        after: query_i64(fields, "after", 0),
        limit: query_usize(fields, "limit", 500),
        legacy_only: false,
    };
    match state.api.events(ApiView::V2, query).await {
        Ok(events) => ok(to_value(&events)),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_carriers(state: &ServerState) -> Response {
    match state.api.carriers().await {
        Ok(carriers) => ok(Value::Array(
            carriers.iter().map(|carrier| carrier.api_value()).collect(),
        )),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_providers(state: &ServerState) -> Response {
    match state.api.providers().await {
        Ok(snapshot) => ok(to_value(&snapshot)),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_add_provider(state: &ServerState, request: Request<Body>) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let input: ProviderInput = match serde_json::from_value(body) {
        Ok(input) => input,
        Err(cause) => return request_failed(cause.to_string()),
    };
    match state.api.add_provider(input).await {
        Ok(provider) => created(to_value(&provider)),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_switch_provider(state: &ServerState, app: &str, id: &str) -> Response {
    let app = match app.parse() {
        Ok(app) => app,
        Err(error) => return hub_failure(&error),
    };
    match state.api.switch_provider(app, id).await {
        Ok(provider) => ok(to_value(&provider)),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_prompts(state: &ServerState) -> Response {
    match state.api.prompts().await {
        Ok(catalog) => ok(to_value(&catalog)),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_skills(state: &ServerState) -> Response {
    match state.api.skills().await {
        Ok(skills) => ok(to_value(&skills)),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_skill_manager_status(state: &ServerState, fields: &forms::Fields) -> Response {
    let live = fields.get("live") == Some("true");
    match state
        .api
        .skill_manager(SkillManagerAction::Status, json!({ "live": live }))
        .await
    {
        Ok(value) => ok(value),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_skill_manager_action(
    state: &ServerState,
    action: &str,
    request: Request<Body>,
) -> Response {
    let action = match action {
        "connect" => SkillManagerAction::Connect,
        "sync" => SkillManagerAction::Sync,
        "disconnect" => SkillManagerAction::Disconnect,
        _ => return route_not_found(),
    };
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state.api.skill_manager(action, body).await {
        Ok(value) => ok(value),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_import_skill(state: &ServerState, request: Request<Body>) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let input: SkillImportInput = match serde_json::from_value(body) {
        Ok(input) => input,
        Err(cause) => return request_failed(cause.to_string()),
    };
    match state.api.import_skill(input).await {
        Ok(result) => created(to_value(&result)),
        Err(error) => hub_failure(&error),
    }
}

async fn v2_create_agent(state: &ServerState, request: Request<Body>) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let spec: AgentSpec = match serde_json::from_value(body) {
        Ok(spec) => spec,
        Err(cause) => return request_failed(cause.to_string()),
    };
    let created_view = match state.api.create(ApiView::V2, spec).await {
        Ok(view) => view,
        Err(error) => return hub_failure(&error),
    };
    // POST /v2/agents returns the detail view, i.e. with frozen prompt text.
    let id = view_id(&created_view);
    let view = match state.api.agent(ApiView::V2, &id).await {
        Ok(full) => full,
        Err(_) => created_view,
    };
    created(to_value(&view))
}

async fn v2_agent(
    state: &ServerState,
    method: &Method,
    id: &str,
    action: &str,
    fields: &forms::Fields,
    request: Request<Body>,
) -> Response {
    if action.is_empty() && is(method, &Method::GET) {
        return match state.api.agent(ApiView::V2, id).await {
            Ok(view) => ok(to_value(&view)),
            Err(error) => hub_failure(&error),
        };
    }
    if action == "events" && is(method, &Method::GET) {
        return v2_events(state, fields, Some(id.to_string())).await;
    }
    if MESSAGE_ACTIONS.contains(&action) && is(method, &Method::POST) {
        return send_message(state, ApiView::V2, id, action, request).await;
    }
    if is(method, &Method::POST) {
        match action {
            "abort" => return control(state, ApiView::V2, id, ControlAction::Abort).await,
            // `ApiView` is both a response shape and an entity gate: core's
            // control() applies the legacy-only visibility rule for `V1`, so the
            // v2 routes must ask for `V2`. The 2.1 legacy response shape is
            // preserved anyway - control() always returns
            // `ControlResult::Agent(AgentView::V1(..))` for stop/restart.
            "stop" => return control(state, ApiView::V2, id, ControlAction::Stop).await,
            "restart" => return control(state, ApiView::V2, id, ControlAction::Restart).await,
            _ => {}
        }
    }
    // Unknown action: the 2.1 server still resolved the Agent before falling
    // through, so an unknown id keeps reporting `agent_not_found`.
    match state.api.agent(ApiView::V2, id).await {
        Err(error) if error.code == error::CODE_AGENT_NOT_FOUND => hub_failure(&error),
        _ => route_not_found(),
    }
}

async fn v1_snapshot(state: &ServerState) -> Response {
    match state.api.snapshot(ApiView::V1).await {
        Ok(Snapshot::V1(snapshot)) => ok(to_value(&snapshot)),
        Ok(Snapshot::V2(snapshot)) => ok(to_value(&project_snapshot_v1(snapshot))),
        Err(error) => hub_failure(&error),
    }
}

async fn v1_list_agents(state: &ServerState, fields: &forms::Fields) -> Response {
    let include_stopped = fields.get("all") != Some("false");
    match state.api.list_agents(include_stopped).await {
        Ok(agents) => ok(to_value(&legacy_views(agents))),
        Err(error) => hub_failure(&error),
    }
}

async fn v1_create_agent(state: &ServerState, request: Request<Body>) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let spec: AgentSpec = match serde_json::from_value(body) {
        Ok(spec) => spec,
        Err(cause) => return request_failed(cause.to_string()),
    };
    match state.api.create(ApiView::V1, spec).await {
        Ok(view) => created(to_value(&view)),
        Err(error) => hub_failure(&error),
    }
}

async fn v1_events(state: &ServerState, fields: &forms::Fields) -> Response {
    let query = EventQuery {
        agent_id: None,
        after: query_i64(fields, "after", 0),
        limit: query_usize(fields, "limit", 500),
        legacy_only: true,
    };
    match state.api.events(ApiView::V1, query).await {
        Ok(events) => ok(to_value(&events)),
        Err(error) => hub_failure(&error),
    }
}

/// Resolve an Agent for the legacy namespace.
///
/// 2.1 hides platform Agents from `/v1` entirely: `GET|POST /v1/agents/:id...`
/// answers `404 agent_not_found` before any action runs when the row has a
/// frozen skill authorization. The legacy view does not carry that flag, so the
/// platform view decides. A platform view that is unavailable leaves the legacy
/// row reachable, keeping pre-platform Agents working.
async fn legacy_agent(state: &ServerState, id: &str) -> Result<AgentView, Response> {
    match state.api.agent(ApiView::V1, id).await {
        Ok(AgentView::V1(agent)) => {
            if is_platform_agent(state, id).await {
                Err(agent_not_found(id))
            } else {
                Ok(AgentView::V1(agent))
            }
        }
        Ok(AgentView::V2(_)) => Err(agent_not_found(id)),
        Err(error) => Err(hub_failure(&error)),
    }
}

async fn is_platform_agent(state: &ServerState, id: &str) -> bool {
    match state.api.agent(ApiView::V2, id).await {
        Ok(AgentView::V2(agent)) => {
            agent.skill_policy.mode != pihub_contracts::SkillPolicyViewMode::Legacy
        }
        _ => false,
    }
}

async fn v1_agent(
    state: &ServerState,
    method: &Method,
    id: &str,
    action: &str,
    fields: &forms::Fields,
    request: Request<Body>,
) -> Response {
    if action.is_empty() && is(method, &Method::GET) {
        return match legacy_agent(state, id).await {
            Ok(AgentView::V1(agent)) => ok(to_value(&agent)),
            Ok(AgentView::V2(_)) => agent_not_found(id),
            Err(response) => response,
        };
    }
    // Every remaining legacy route addresses the same Agent, and 2.1 refused a
    // platform Agent before dispatching the action. This matters most for
    // prompt/steer/follow-up: `HubApi::send()` carries no `ApiView`, so the
    // legacy visibility rule cannot be enforced downstream and must be applied
    // here, on the route, before the message is sent.
    if let Err(response) = legacy_agent(state, id).await {
        return response;
    }
    if action == "events" && is(method, &Method::GET) {
        let query = EventQuery {
            agent_id: Some(id.to_string()),
            after: query_i64(fields, "after", 0),
            limit: query_usize(fields, "limit", 500),
            legacy_only: true,
        };
        return match state.api.events(ApiView::V1, query).await {
            Ok(events) => ok(to_value(&events)),
            Err(error) => hub_failure(&error),
        };
    }
    if MESSAGE_ACTIONS.contains(&action) && is(method, &Method::POST) {
        return send_message(state, ApiView::V1, id, action, request).await;
    }
    if is(method, &Method::POST) {
        match action {
            "abort" => return control(state, ApiView::V1, id, ControlAction::Abort).await,
            "stop" => return control(state, ApiView::V1, id, ControlAction::Stop).await,
            "restart" => return control(state, ApiView::V1, id, ControlAction::Restart).await,
            _ => {}
        }
    }
    match state.api.agent(ApiView::V1, id).await {
        Err(error) if error.code == error::CODE_AGENT_NOT_FOUND => hub_failure(&error),
        _ => route_not_found(),
    }
}

async fn daemon_stop(state: &ServerState) -> Response {
    let stop = state.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        stop.notify_waiters();
    });
    ok(json!({ "stopping": true }))
}

async fn send_message(
    state: &ServerState,
    _view: ApiView,
    id: &str,
    action: &str,
    request: Request<Body>,
) -> Response {
    let kind = match action {
        "prompt" => RuntimeRequestKind::Prompt,
        "steer" => RuntimeRequestKind::Steer,
        _ => RuntimeRequestKind::FollowUp,
    };
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match state.api.send(id, kind, &message).await {
        Ok(value) => ok(value),
        Err(error) => hub_failure(&error),
    }
}

async fn control(
    state: &ServerState,
    view: ApiView,
    id: &str,
    action: ControlAction,
) -> Response {
    match state.api.control(view, id, action).await {
        Ok(result) => ok(to_value(&result)),
        Err(error) => hub_failure(&error),
    }
}

fn request_failed(message: String) -> Response {
    fail(
        StatusCode::BAD_REQUEST,
        error::CODE_REQUEST_FAILED,
        message,
        None,
    )
}

pub(crate) fn agent_not_found(id: &str) -> Response {
    fail(
        StatusCode::NOT_FOUND,
        error::CODE_AGENT_NOT_FOUND,
        format!("unknown agent: {id}"),
        None,
    )
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn view_id(view: &AgentView) -> String {
    match view {
        AgentView::V1(agent) => agent.id.clone(),
        AgentView::V2(agent) => agent.agent.id.clone(),
    }
}

/// Keep only legacy rows for the v1 list view.
fn legacy_views(views: Vec<AgentView>) -> Vec<PublicAgent> {
    views
        .into_iter()
        .filter_map(|view| match view {
            AgentView::V1(agent) => Some(agent),
            AgentView::V2(_) => None,
        })
        .collect()
}

/// Lossless narrowing for `/v1/snapshot`: `AgentViewV2` embeds the exact
/// `PublicAgent` legacy row, so no field is invented or dropped. (The reverse
/// direction is *not* lossless and is refused above.)
fn project_snapshot_v1(snapshot: SnapshotV2) -> SnapshotV1 {
    SnapshotV1 {
        version: snapshot.version,
        profile: snapshot.profile,
        agents: snapshot.agents.iter().map(|view| view.agent.clone()).collect(),
        event_cursor: snapshot.event_cursor,
    }
}
