//! Loopback HTTP API and SSR panel host.
//!
//! The router mirrors the 2.1 server: unauthenticated `/`, `/app.js`,
//! `/styles.css` and `/v1/health`; a `Bearer` control token for everything
//! else; `{ok,data,error}` envelopes; `cache-control: no-store`; an
//! `application/json; charset=utf-8` content type; and a 1 MiB body limit.

mod dispatch;
mod ui;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use pihub_contracts::{HubApi, HubPaths, APP_VERSION};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::error;
use crate::forms;

/// Every handler in this module answers with a fully built body.
pub(crate) type Response = axum::http::Response<Body>;

/// `1 MiB`, matching `src/http-server.js:36-45`.
pub const BODY_LIMIT: usize = 1024 * 1024;

/// Strict CSP used for the panel document and for every served asset.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; base-uri 'none'; frame-ancestors 'none'";

pub const PANEL_CSS: &str = include_str!("../../assets/styles.css");
pub const PANEL_JS: &str = include_str!("../../assets/app.js");

/// Shared server state. `HubApi` is the only Hub dependency.
pub struct ServerState {
    pub api: Arc<dyn HubApi>,
    pub paths: HubPaths,
    pub token: String,
    pub stop: Arc<Notify>,
    pub version: String,
}

impl ServerState {
    pub fn new(api: Arc<dyn HubApi>, paths: HubPaths, token: impl Into<String>) -> Self {
        Self {
            api,
            paths,
            token: token.into(),
            stop: Arc::new(Notify::new()),
            version: APP_VERSION.to_string(),
        }
    }

    pub fn with_stop(
        api: Arc<dyn HubApi>,
        paths: HubPaths,
        token: impl Into<String>,
        stop: Arc<Notify>,
    ) -> Self {
        Self {
            api,
            paths,
            token: token.into(),
            stop,
            version: APP_VERSION.to_string(),
        }
    }

    fn authorized(&self, authorization: Option<&HeaderValue>) -> bool {
        let Some(value) = authorization.and_then(|value| value.to_str().ok()) else {
            return false;
        };
        value == format!("Bearer {}", self.token)
    }
}

/// Build the full router.
pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(panel_page))
        .route("/app.js", get(panel_js))
        .route("/styles.css", get(panel_css))
        .route("/v1/health", get(health))
        .fallback(fallback)
        .with_state(state)
}

async fn health() -> Response {
    ok(crate::client::health_payload(std::process::id()))
}

async fn panel_page(State(state): State<Arc<ServerState>>, request: Request<Body>) -> Response {
    let query = request.uri().query().unwrap_or("");
    let fields = forms::Fields::parse(query);
    match crate::dashboard::render(&fields, state.api.as_ref(), &state.token).await {
        Ok(markup) => html_response(markup),
        Err(error) => html_response(crate::dashboard::render_error_page(&error.message, &state.token)),
    }
}

async fn panel_css() -> Response {
    asset_response("text/css; charset=utf-8", PANEL_CSS)
}

async fn panel_js() -> Response {
    asset_response("text/javascript; charset=utf-8", PANEL_JS)
}

async fn fallback(State(state): State<Arc<ServerState>>, request: Request<Body>) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    if path.starts_with("/ui/") {
        return ui::handle(state, method, path, request).await;
    }
    if !state.authorized(request.headers().get(header::AUTHORIZATION)) {
        return fail(
            StatusCode::UNAUTHORIZED,
            error::CODE_UNAUTHORIZED,
            "valid local control token required",
            None,
        );
    }
    dispatch::handle(state, method, path, request).await
}

/// Read and parse a JSON body, enforcing the 1 MiB limit.
pub(crate) async fn read_json_body(request: Request<Body>) -> Result<Value, Response> {
    let bytes = axum::body::to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| body_too_large())?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice::<Value>(&bytes).map_err(|cause| {
        fail(
            StatusCode::BAD_REQUEST,
            error::CODE_REQUEST_FAILED,
            cause.to_string(),
            None,
        )
    })
}

pub(crate) async fn read_form_body(
    request: Request<Body>,
) -> Result<forms::Fields, Response> {
    let bytes = axum::body::to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| body_too_large())?;
    Ok(forms::Fields::parse(&String::from_utf8_lossy(&bytes)))
}

fn body_too_large() -> Response {
    fail(
        StatusCode::BAD_REQUEST,
        error::CODE_REQUEST_FAILED,
        "request body exceeds 1 MiB",
        None,
    )
}

/// `{"ok":true,"data":...,"error":null}` with a trailing newline.
pub(crate) fn ok(data: Value) -> Response {
    ok_status(data, StatusCode::OK)
}

pub(crate) fn created(data: Value) -> Response {
    ok_status(data, StatusCode::CREATED)
}

pub(crate) fn ok_status(data: Value, status: StatusCode) -> Response {
    json_response(status, json!({ "ok": true, "data": data, "error": null }))
}

pub(crate) fn fail(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    details: Option<Value>,
) -> Response {
    json_response(
        status,
        json!({
            "ok": false,
            "data": null,
            "error": {
                "code": code,
                "message": message.into(),
                "retryable": status.as_u16() >= 500,
                "details": details,
            },
        }),
    )
}

/// Map a `HubError` to the 2.1 status/body pair.
///
/// The 2.1 server wrapped every handler throw in a single
/// `400 request_failed` envelope: an unknown Agent is the only case that keeps
/// its own identity (`404 agent_not_found`). The internal code is therefore not
/// a wire code — it is dropped here, while the message and details survive.
pub(crate) fn hub_failure(hub_error: &pihub_contracts::HubError) -> Response {
    let status = StatusCode::from_u16(error::status_for(&hub_error.code))
        .unwrap_or(StatusCode::BAD_REQUEST);
    let code = if hub_error.code == error::CODE_AGENT_NOT_FOUND {
        error::CODE_AGENT_NOT_FOUND
    } else {
        error::CODE_REQUEST_FAILED
    };
    fail(
        status,
        code,
        hub_error.message.clone(),
        hub_error.details.clone(),
    )
}

pub(crate) fn json_response(status: StatusCode, body: Value) -> Response {
    let mut serialized = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    serialized.push(b'\n');
    axum::http::response::Builder::new()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(serialized))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn html_response(markup: String) -> Response {
    axum::http::response::Builder::new()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY)
        .header("x-content-type-options", "nosniff")
        .body(Body::from(markup))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn asset_response(content_type: &str, body: &'static str) -> Response {
    axum::http::response::Builder::new()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, body.len().to_string())
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .header(header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY)
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Query helpers shared by the API and the panel.
pub(crate) fn query_i64(fields: &forms::Fields, key: &str, fallback: i64) -> i64 {
    fields
        .get(key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(fallback)
}

pub(crate) fn query_usize(fields: &forms::Fields, key: &str, fallback: usize) -> usize {
    fields
        .get(key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(fallback)
}

/// Percent-decode a single path segment (Node did `decodeURIComponent`).
pub(crate) fn segment(value: &str) -> String {
    forms::percent_decode(value)
}

/// `Method` convenience for the dispatcher's method matching.
pub(crate) fn is(method: &Method, expected: &Method) -> bool {
    method == expected
}
