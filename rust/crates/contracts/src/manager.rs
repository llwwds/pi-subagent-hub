use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::HubResult;
use crate::skill::SkillPolicy;

/// Selection mirrors the Skills Manager CLI's read-only filters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSelection {
    #[serde(default)]
    pub preset_ids: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerConnectInput {
    #[serde(default)]
    pub cli_path: Option<PathBuf>,
    #[serde(default)]
    pub manager_base_dir: Option<PathBuf>,
    #[serde(default)]
    pub manager_skills_dir: Option<PathBuf>,
    #[serde(default)]
    pub agent_key: Option<String>,
    #[serde(default)]
    pub selection: ManagerSelection,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManagerSyncInput {
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerDisconnectInput {
    #[serde(default)]
    pub purge_snapshot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSkillSource {
    pub id: String,
    pub name: String,
    pub source_path: PathBuf,
}

/// Manual, read-only Skills Manager connection.
///
/// `status` / `connect` / `sync` / `disconnect` return the original 2.1 JSON
/// object (`Value`) because that shape includes several optional nested
/// diagnostics. `resolve_sources` is synchronous (filesystem + snapshot only).
#[async_trait::async_trait]
pub trait ManagerConnection: Send + Sync {
    async fn status(&self, live: bool) -> HubResult<Value>;
    async fn connect(&self, input: ManagerConnectInput) -> HubResult<Value>;
    async fn sync(&self, input: ManagerSyncInput) -> HubResult<Value>;
    async fn disconnect(&self, input: ManagerDisconnectInput) -> HubResult<Value>;
    fn resolve_sources(&self, policy: Option<&SkillPolicy>) -> HubResult<Vec<ManagerSkillSource>>;
}
