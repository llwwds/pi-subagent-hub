//! Shared contracts for pi-subagent-hub.
//!
//! This crate is deliberately transport-neutral: it contains the public data
//! model and the narrow interfaces wired by `pihub-bin`. Implementations live
//! in `pihub-core` (store/lifecycle/prompts/skills/logging), `pihub-runtime`
//! (carriers/CC Switch) and `pihub-app` (CLI/HTTP/SSR).
//!
//! Rules that implementations must preserve:
//! - `Store` is synchronous. No SQLite statement or lock may be held across an
//!   `.await`; all async work happens outside store calls.
//! - Runtime adapters emit events through a bounded channel and never write the
//!   database. The core is the single logical event writer.
//! - Runtime/HTTP/provider payloads that belong to an external carrier are kept
//!   as `serde_json::Value` so their wire shape is not rewritten by Rust.
//! - Skill authorization digest and prompt digest use different algorithms;
//!   see [`util::skill_authorization_digest`], [`util::canonical_json`] and
//!   `docs/rust-contracts.md` in the repository.

pub mod agent;
pub mod api;
pub mod carrier;
pub mod error;
pub mod ids;
pub mod local;
pub mod log;
pub mod manager;
pub mod paths;
pub mod prompt;
pub mod provider;
pub mod runtime;
pub mod skill;
pub mod store;
pub mod util;

pub use async_trait::async_trait;

pub use agent::{
    AgentPatch, AgentRecord, AgentSpec, AgentView, AgentViewV2, ControlResult,
    ProfileView, PublicAgent, Snapshot, SnapshotV1, SnapshotV2,
};
pub use api::{HubApi, SkillManagerAction};
pub use carrier::{CarrierCapabilities, CarrierDiagnostic};
pub use error::{Envelope, ErrorBody, HubError, HubResult};
pub use ids::{
    API_VERSION, APP_NAME, APP_VERSION, AgentStatus, ApiView, Carrier, ControlAction,
    DEFAULT_TOOLS, DesiredState, ProviderApp, RELEASE_CHANNEL, RuntimeRequestKind, ThinkingLevel,
    assert_agent_id, is_valid_agent_id, CLI_NAME,
};
pub use local::{
    ControlRecord, DaemonLockRecord, DaemonMetadata, DaemonStatus, Health, LocalOps,
};
pub use log::{
    is_ephemeral_stream_event, LogLevel, LogRecord, LogSink, MAX_CARRIER_UNWRAP_DEPTH,
    STREAM_DELTA_TYPES,
};
pub use manager::{
    ManagerConnectInput, ManagerConnection, ManagerDisconnectInput, ManagerSelection,
    ManagerSkillSource, ManagerSyncInput,
};
pub use paths::{absolute_normalized, normalize_lexical, HubPaths};
pub use prompt::{
    FrozenPromptConfig, PromptCatalog, PromptChannel, PromptConfigInput, PromptMode,
    PromptPrinciple, PromptService, PromptSummary,
};
pub use provider::{
    AuthMode, Provider, ProviderApps, ProviderDiagnostics, ProviderInput, ProviderService,
    ProviderSnapshot,
};
pub use runtime::{
    ModelRef, Runtime, RuntimeEvent, RuntimeFactory, RuntimeLaunch, RuntimeState,
    RUNTIME_EVENT_CHANNEL_CAPACITY,
};
pub use skill::{
    ResourceRef, SkillAuthorization, SkillGrant, SkillImportInput, SkillImportResult, SkillPolicy,
    SkillPolicyMode, SkillPolicyView, SkillPolicyViewMode, SkillRef, SkillRegistry, SkillSummary,
    SkillVersion,
};
pub use store::{EventQuery, ListAgentsOptions, NewEvent, Store, StoredEvent};
pub use util::{
    canonical_json, is_safe_token, new_id, now_iso, sha256_hex, sha256_prefixed,
    skill_authorization_digest, utf16_len,
};
