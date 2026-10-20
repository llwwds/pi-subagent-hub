use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::HubResult;
use crate::ids::ProviderApp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    ApiKey,
    Bearer,
}

/// Add-provider input. `api_key` must never be logged or echoed back.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInput {
    pub app: ProviderApp,
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model_list: Vec<String>,
    pub default_model: String,
    #[serde(default)]
    pub api_format: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<AuthMode>,
}

impl std::fmt::Debug for ProviderInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderInput")
            .field("app", &self.app)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model_list", &self.model_list)
            .field("default_model", &self.default_model)
            .field("api_format", &self.api_format)
            .field("auth_mode", &self.auth_mode)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub model_list: Vec<String>,
    pub default_model: Option<String>,
    pub api_format: Option<String>,
    pub auth_mode: Option<AuthMode>,
    pub configured: bool,
    pub ready: bool,
    /// `None` for Pi, matching 2.1.
    pub is_current: Option<bool>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDiagnostics {
    pub available: bool,
    pub version: Option<String>,
    pub binary_path: PathBuf,
    pub config_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderApps {
    pub claude: Vec<Provider>,
    pub codex: Vec<Provider>,
    pub pi: Vec<Provider>,
}

/// `/v2/providers` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub switch: ProviderDiagnostics,
    pub apps: ProviderApps,
}

/// CC Switch wrapper implemented by `pihub-runtime` and injected into core.
#[async_trait::async_trait]
pub trait ProviderService: Send + Sync {
    async fn diagnose(&self) -> ProviderDiagnostics;
    async fn list(&self, app: ProviderApp) -> HubResult<Vec<Provider>>;
    async fn list_all(&self) -> HubResult<ProviderSnapshot>;
    async fn get(&self, app: ProviderApp, id: &str) -> HubResult<Option<Provider>>;
    async fn add(&self, input: ProviderInput) -> HubResult<Provider>;
    /// Pi is deliberately unsupported (`CC_SWITCH_UNSUPPORTED`).
    async fn switch(&self, app: ProviderApp, id: &str) -> HubResult<Provider>;
}
