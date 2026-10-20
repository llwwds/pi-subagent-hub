use serde_json::Value;

use crate::agent::{AgentSpec, AgentView, ControlResult, Snapshot};
use crate::carrier::CarrierDiagnostic;
use crate::error::HubResult;
use crate::ids::{ApiView, ControlAction, RuntimeRequestKind};
use crate::prompt::PromptCatalog;
use crate::provider::{Provider, ProviderInput, ProviderSnapshot};
use crate::skill::{SkillImportInput, SkillImportResult, SkillSummary};
use crate::store::{EventQuery, StoredEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillManagerAction {
    Status,
    Connect,
    Sync,
    Disconnect,
}

impl SkillManagerAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Connect => "connect",
            Self::Sync => "sync",
            Self::Disconnect => "disconnect",
        }
    }
}

/// The app-facing boundary. `pihub-app` depends on this trait and never sees
/// `Store`; `pihub-core` implements it over the manager.
#[async_trait::async_trait]
pub trait HubApi: Send + Sync {
    async fn snapshot(&self, view: ApiView) -> HubResult<Snapshot>;
    /// `/v1/agents?all=...`; always the legacy-only view.
    async fn list_agents(&self, include_stopped: bool) -> HubResult<Vec<AgentView>>;
    async fn agent(&self, view: ApiView, id: &str) -> HubResult<AgentView>;
    async fn create(&self, view: ApiView, spec: AgentSpec) -> HubResult<AgentView>;
    async fn send(
        &self,
        id: &str,
        kind: RuntimeRequestKind,
        message: &str,
    ) -> HubResult<Value>;
    async fn control(
        &self,
        view: ApiView,
        id: &str,
        action: ControlAction,
    ) -> HubResult<ControlResult>;
    async fn events(&self, view: ApiView, query: EventQuery) -> HubResult<Vec<StoredEvent>>;
    async fn carriers(&self) -> HubResult<Vec<CarrierDiagnostic>>;
    async fn providers(&self) -> HubResult<ProviderSnapshot>;
    async fn add_provider(&self, input: ProviderInput) -> HubResult<Provider>;
    async fn switch_provider(&self, app: crate::ids::ProviderApp, id: &str)
    -> HubResult<Provider>;
    async fn skills(&self) -> HubResult<Vec<SkillSummary>>;
    async fn import_skill(&self, input: SkillImportInput) -> HubResult<SkillImportResult>;
    /// `input` is the original 2.1 request body; the response is the original
    /// status object (plus `result` for mutations).
    async fn skill_manager(&self, action: SkillManagerAction, input: Value) -> HubResult<Value>;
    async fn prompts(&self) -> HubResult<PromptCatalog>;
}
