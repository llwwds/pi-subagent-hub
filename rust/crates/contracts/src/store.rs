use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{AgentPatch, AgentRecord};
use crate::error::HubResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListAgentsOptions {
    pub include_stopped: bool,
    pub legacy_only: bool,
}

impl Default for ListAgentsOptions {
    fn default() -> Self {
        Self {
            include_stopped: true,
            legacy_only: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub event_id: String,
    pub agent_id: Option<String>,
    pub event_type: String,
    pub payload: Value,
    pub created_at: String,
}

impl NewEvent {
    pub fn new(
        event_id: impl Into<String>,
        agent_id: Option<String>,
        event_type: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            agent_id,
            event_type: event_type.into(),
            payload,
            created_at: crate::util::now_iso(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub cursor: i64,
    pub event_id: String,
    pub agent_id: Option<String>,
    pub event_type: String,
    /// Parsed projection of [`Self::payload_json`].
    pub payload: Value,
    /// Raw stored text from the `payload_json` column. 2.1 returns the row
    /// unchanged (`{...row, payload}`) so clients still receive the exact
    /// bytes that were persisted, not a re-serialized object.
    pub payload_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct EventQuery {
    pub agent_id: Option<String>,
    pub after: i64,
    pub limit: usize,
    pub legacy_only: bool,
}

impl Default for EventQuery {
    fn default() -> Self {
        Self {
            agent_id: None,
            after: 0,
            limit: 500,
            legacy_only: false,
        }
    }
}

impl EventQuery {
    /// 2.1: `Math.max(1, Math.min(Number(limit) || 500, 2000))`.
    /// An explicit `0` therefore means the default 500, not 1.
    pub fn clamped_limit(&self) -> usize {
        let requested = if self.limit == 0 { 500 } else { self.limit };
        requested.clamp(1, 2000)
    }
}

/// Synchronous SQLite boundary. Implementations must not hold the connection
/// lock, a transaction, or a statement across an `.await`.
pub trait Store: Send + Sync {
    fn create_agent(&self, agent: &AgentRecord) -> HubResult<AgentRecord>;
    fn get_agent(&self, id: &str) -> HubResult<Option<AgentRecord>>;
    fn list_agents(&self, options: ListAgentsOptions) -> HubResult<Vec<AgentRecord>>;
    fn update_agent(&self, id: &str, patch: AgentPatch) -> HubResult<AgentRecord>;
    fn append_event(&self, event: NewEvent) -> HubResult<i64>;
    fn list_events(&self, query: EventQuery) -> HubResult<Vec<StoredEvent>>;
    fn latest_cursor(&self, legacy_only: bool) -> HubResult<i64>;
    fn mark_interrupted(&self) -> HubResult<()>;
    fn close(&self) -> HubResult<()>;
}
