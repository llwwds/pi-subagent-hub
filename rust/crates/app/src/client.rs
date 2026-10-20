//! Control-plane client.
//!
//! The CLI never links the manager: it talks to the running daemon over the
//! loopback HTTP API, exactly like `src/client.js`. The client therefore owns
//! the same two behaviours: read `state/daemon.json` + `state/control.json` for
//! the base URL and Bearer token, and unwrap the `{ok,data,error}` envelope.

use std::path::Path;

use pihub_contracts::{
    ControlRecord, DaemonMetadata, HubError, HubPaths, HubResult, APP_NAME, APP_VERSION,
    API_VERSION,
};
use serde_json::Value;

/// Connection details for a running daemon.
#[derive(Debug, Clone)]
pub struct HubClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl HubClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // Loopback only: never route through an environment proxy.
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base_url: base_url.into(),
            token: token.into(),
            http,
        }
    }

    /// Connect using the Hub's own state files.
    pub fn connect(paths: &HubPaths) -> HubResult<Self> {
        let daemon: Option<DaemonMetadata> = read_json(&paths.daemon_path)?;
        let control: Option<ControlRecord> = read_json(&paths.control_path)?;
        let (Some(daemon), Some(control)) = (daemon, control) else {
            return Err(daemon_not_running());
        };
        if daemon.pid == 0 || daemon.port == 0 {
            return Err(daemon_not_running());
        }
        Ok(Self::new(
            format!("http://127.0.0.1:{}", daemon.port),
            control.token,
        ))
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Send a request and return the unwrapped `data` field.
    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> HubResult<Value> {
        let mut request = self
            .http
            .request(method, format!("{}{}", self.base_url, path))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", self.token));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| {
            HubError::new(CODE_API_UNAVAILABLE, format!("hub request failed: {error}"))
                .retryable(true)
        })?;
        let status = response.status();
        let payload: Value = response.json().await.map_err(|error| {
            HubError::new(
                CODE_API_ERROR,
                format!("HTTP {}: {error}", status.as_u16()),
            )
        })?;
        let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !status.is_success() || !ok {
            let error = payload.get("error");
            let code = error
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str)
                .unwrap_or(CODE_API_ERROR);
            let message = error
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("Hub API failed with HTTP {}", status.as_u16()));
            let retryable = error
                .and_then(|value| value.get("retryable"))
                .and_then(Value::as_bool)
                .unwrap_or(status.is_server_error());
            let details = error.and_then(|value| value.get("details")).cloned();
            let mut hub_error = HubError::new(code, message).retryable(retryable);
            if let Some(details) = details
                && !details.is_null()
            {
                hub_error = hub_error.with_details(details);
            }
            return Err(hub_error);
        }
        Ok(payload.get("data").cloned().unwrap_or(Value::Null))
    }

    /// `GET /v1/health`, which is deliberately unauthenticated.
    pub async fn health(&self) -> HubResult<Value> {
        let response = self
            .http
            .get(format!("{}/v1/health", self.base_url))
            .send()
            .await
            .map_err(|error| {
                HubError::new(CODE_API_UNAVAILABLE, format!("hub health check failed: {error}"))
                    .retryable(true)
            })?;
        if !response.status().is_success() {
            return Err(HubError::new(
                CODE_API_ERROR,
                format!("Hub health check failed with HTTP {}", response.status().as_u16()),
            ));
        }
        let payload: Value = response.json().await.map_err(|error| {
            HubError::new(CODE_API_ERROR, format!("hub health response failed: {error}"))
        })?;
        Ok(payload.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Fail-closed guard for lifecycle *writes*.
    ///
    /// A Hub of another major version owns a different schema and a different
    /// isolation model, so driving it from this binary can corrupt state its
    /// own manager never expects to see. Writes therefore refuse unless the
    /// target daemon reports the same major version as this CLI. Reads and
    /// `/v1` server compatibility stay as they are, and there is no force
    /// switch: the operator manages that daemon with its own CLI.
    pub async fn ensure_control_compatible(&self) -> HubResult<()> {
        let health = self.health().await?;
        let expected = supported_major();
        match health.get("version").and_then(Value::as_str) {
            Some(version) if major_of(version) == Some(expected) => Ok(()),
            Some(version) => Err(HubError::new(
                CODE_VERSION_MISMATCH,
                format!(
                    "daemon reports {APP_NAME} {version}; this CLI ({APP_VERSION}) only controls \
                     {expected}.x daemons. Manage that instance with its own {version} CLI; no \
                     control was sent."
                ),
            )),
            None => Err(HubError::new(
                CODE_VERSION_UNKNOWN,
                "cannot read the daemon version from /v1/health; refusing to send controls because \
                 the target may belong to another major version. Check `pihub doctor --json`.",
            )),
        }
    }

    /// Every lifecycle *write* goes through here: same-major check first, then
    /// the POST. Reads keep using [`HubClient::request`] unchanged.
    pub async fn control_request(&self, path: &str, body: Option<&Value>) -> HubResult<Value> {
        self.ensure_control_compatible().await?;
        self.request(reqwest::Method::POST, path, body).await
    }
}

/// Major component of a dotted version, e.g. `3.0.0.0` -> `3`.
pub fn major_of(version: &str) -> Option<u64> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
}

/// Major version this build is allowed to control.
pub fn supported_major() -> u64 {
    major_of(APP_VERSION).unwrap_or(0)
}

pub const CODE_API_ERROR: &str = "api_error";
pub const CODE_API_UNAVAILABLE: &str = "api_unavailable";
/// The daemon answered, but belongs to another major version.
pub const CODE_VERSION_MISMATCH: &str = "daemon_version_unsupported";
/// The daemon answered without a usable version, so a write cannot be proven safe.
pub const CODE_VERSION_UNKNOWN: &str = "daemon_version_unknown";

pub fn daemon_not_running() -> HubError {
    HubError::new(
        CODE_API_UNAVAILABLE,
        "pi-subagent-hub daemon is not running",
    )
}

/// Local health payload written by `/v1/health`.
pub fn health_payload(pid: u32) -> Value {
    serde_json::json!({
        "name": APP_NAME,
        "version": APP_VERSION,
        "apiVersion": API_VERSION,
        "pid": pid,
    })
}

/// Read a JSON state file, treating absence and corruption the same way 2.1's
/// `readJson` does (missing -> `None`, unparsable -> error).
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> HubResult<Option<T>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(HubError::from(error)),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let value = serde_json::from_str(&raw)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, value: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, value).unwrap();
    }

    #[test]
    fn connect_requires_daemon_and_control_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = HubPaths::resolve(dir.path(), dir.path().join("app"));
        assert_eq!(
            HubClient::connect(&paths).unwrap_err().code,
            CODE_API_UNAVAILABLE
        );
        write(
            &paths.daemon_path,
            r#"{"pid":42,"host":"127.0.0.1","port":9001,"version":"3.0.0.0","startedAt":"t","appRoot":"/a"}"#,
        );
        write(&paths.control_path, r#"{"token":"secret-token","createdAt":"t"}"#);
        let client = HubClient::connect(&paths).unwrap();
        assert_eq!(client.base_url(), "http://127.0.0.1:9001");
    }

    #[test]
    fn major_parsing_accepts_real_version_strings() {
        assert_eq!(major_of("3.0.0.0"), Some(3));
        assert_eq!(major_of("2.0.0.0"), Some(2));
        assert_eq!(major_of("v3.1.4"), Some(3));
        assert_eq!(major_of(" 3 "), Some(3));
        assert_eq!(major_of("not-a-version"), None);
        assert_eq!(major_of(""), None);
        assert_eq!(supported_major(), 3);
    }

    #[test]
    fn health_payload_matches_contract() {
        let value = health_payload(7);
        assert_eq!(value["name"], APP_NAME);
        assert_eq!(value["version"], APP_VERSION);
        assert_eq!(value["apiVersion"], API_VERSION);
        assert_eq!(value["pid"], 7);
    }
}
