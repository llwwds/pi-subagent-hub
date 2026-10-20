use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::HubResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonMetadata {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub version: String,
    pub started_at: String,
    pub app_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonLockRecord {
    pub pid: u32,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRecord {
    pub token: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub metadata: Option<DaemonMetadata>,
    pub health: Option<Health>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Offline CLI operations. `pihub-app` implements or receives this from the bin
/// assembly; config/doctor never require a running daemon.
#[async_trait::async_trait]
pub trait LocalOps: Send + Sync {
    async fn config_init(&self) -> HubResult<Value>;
    async fn config_validate(&self) -> HubResult<Value>;
    async fn config_paths(&self) -> HubResult<Value>;
    async fn doctor(&self) -> HubResult<Value>;
    async fn daemon_start(&self) -> HubResult<Value>;
    async fn daemon_stop(&self) -> HubResult<Value>;
    async fn daemon_status(&self) -> HubResult<DaemonStatus>;
}
