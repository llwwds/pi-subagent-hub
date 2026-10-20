use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::carrier::CarrierDiagnostic;
use crate::error::HubResult;
use crate::ids::{Carrier, RuntimeRequestKind, ThinkingLevel};
use crate::log::LogSink;
use crate::prompt::FrozenPromptConfig;
use crate::skill::{ResourceRef, SkillAuthorization, SkillRef};

/// Bounded runtime -> core event channel capacity.
pub const RUNTIME_EVENT_CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

/// Result of `start()` (Pi `get_state`, or the equivalent carrier snapshot).
#[derive(Debug, Clone, Default)]
pub struct RuntimeState {
    pub session_id: Option<String>,
    pub session_file: Option<String>,
    pub model: Option<ModelRef>,
    pub thinking: Option<ThinkingLevel>,
    pub is_streaming: bool,
}

/// Everything a runtime adapter needs; it never touches the store.
#[derive(Debug, Clone)]
pub struct RuntimeLaunch {
    pub agent_id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub workspace_mode: String,
    pub carrier: Carrier,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub session_id: String,
    pub session_dir: PathBuf,
    pub agent_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub state_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub tools: Vec<String>,
    pub skills: Vec<SkillRef>,
    pub extensions: Vec<ResourceRef>,
    pub authorization: Option<SkillAuthorization>,
    pub prompt: Option<FrozenPromptConfig>,
}

/// Raw runtime signal. Core owns persistence and status transitions.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// Carrier event object as received (including `carrier_event` wrappers).
    Event { event_type: String, payload: Value },
    ProtocolError { message: String, line: Option<String> },
    ProcessError { message: String },
    Exit {
        code: Option<i32>,
        signal: Option<String>,
        at: String,
    },
}

#[async_trait::async_trait]
pub trait Runtime: Send + Sync {
    fn pid(&self) -> Option<u32>;
    fn is_alive(&self) -> bool;
    async fn start(&self) -> HubResult<RuntimeState>;
    async fn request(&self, kind: RuntimeRequestKind, payload: Value) -> HubResult<Value>;
    async fn stop(&self) -> HubResult<()>;
}

#[async_trait::async_trait]
pub trait RuntimeFactory: Send + Sync {
    async fn diagnose(&self, carrier: Carrier) -> HubResult<CarrierDiagnostic>;
    async fn list(&self) -> HubResult<Vec<CarrierDiagnostic>>;

    /// Create a runtime. Events must be sent through `events`; the factory
    /// implementation must not write the database.
    fn create(
        &self,
        launch: RuntimeLaunch,
        events: mpsc::Sender<RuntimeEvent>,
        log: Option<Arc<dyn LogSink>>,
    ) -> HubResult<Arc<dyn Runtime>>;
}
