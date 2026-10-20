use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{AgentStatus, ApiView, Carrier, DesiredState, ThinkingLevel};
use crate::prompt::{FrozenPromptConfig, PromptConfigInput, PromptSummary};
use crate::skill::{
    ResourceRef, SkillAuthorization, SkillPolicy, SkillPolicyView, SkillPolicyViewMode, SkillRef,
};

/// Creation input. v1 requests omit platform fields; defaults are applied by core.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    /// Initial user prompt after the Agent is ready (not the system prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub carrier: Carrier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_policy: Option<SkillPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_config: Option<PromptConfigInput>,
}

/// Hydrated internal Agent row. Public views are derived from this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub workspace_mode: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub status: AgentStatus,
    pub desired_state: DesiredState,
    pub pid: Option<u32>,
    pub session_id: String,
    pub session_file: Option<String>,
    pub session_dir: PathBuf,
    pub agent_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub profile_digest: String,
    pub tools: Vec<String>,
    pub skills: Vec<SkillRef>,
    pub extensions: Vec<ResourceRef>,
    pub carrier: Carrier,
    pub skill_authorization: Option<SkillAuthorization>,
    /// Parsed from `prompt_config_json`; `None` means `{}`/invalid/absent.
    pub prompt_config: Option<FrozenPromptConfig>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Store update allowlist. Double options distinguish "leave unchanged" from
/// "set to SQL NULL": `None` = unchanged, `Some(None)` = NULL.
#[derive(Debug, Clone, Default)]
pub struct AgentPatch {
    pub name: Option<String>,
    pub status: Option<AgentStatus>,
    pub desired_state: Option<DesiredState>,
    pub pid: Option<Option<u32>>,
    pub session_file: Option<Option<String>>,
    pub last_error: Option<Option<String>>,
    pub provider: Option<Option<String>>,
    pub model: Option<Option<String>>,
    pub thinking: Option<Option<ThinkingLevel>>,
    /// Store writes `now_iso()` when omitted.
    pub updated_at: Option<String>,
}

/// v1 public Agent shape. Internal paths and frozen/raw columns are omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicAgent {
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub workspace_mode: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub status: AgentStatus,
    pub desired_state: DesiredState,
    pub pid: Option<u32>,
    pub session_id: String,
    pub session_file: Option<String>,
    pub session_dir: PathBuf,
    pub profile_digest: String,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// 2.1 `publicAgent()` keeps the hydrated `tools`/`skills`/`extensions`
    /// columns and only removes their `*_json` counterparts. For an Agent with
    /// a frozen skill authorization `skills` holds the grant objects
    /// `{id,path,sha256}`, otherwise the profile resources `{path,sha256}`.
    pub tools: Vec<String>,
    pub skills: Vec<SkillRef>,
    pub extensions: Vec<ResourceRef>,
}

impl From<&AgentRecord> for PublicAgent {
    fn from(agent: &AgentRecord) -> Self {
        Self {
            id: agent.id.clone(),
            name: agent.name.clone(),
            cwd: agent.cwd.clone(),
            workspace_mode: agent.workspace_mode.clone(),
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            thinking: agent.thinking,
            status: agent.status,
            desired_state: agent.desired_state,
            pid: agent.pid,
            session_id: agent.session_id.clone(),
            session_file: agent.session_file.clone(),
            session_dir: agent.session_dir.clone(),
            profile_digest: agent.profile_digest.clone(),
            last_error: agent.last_error.clone(),
            created_at: agent.created_at.clone(),
            updated_at: agent.updated_at.clone(),
            tools: agent.tools.clone(),
            skills: agent.skills.clone(),
            extensions: agent.extensions.clone(),
        }
    }
}

/// v2 public Agent shape: public fields plus carrier/skill policy and prompt summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentViewV2 {
    #[serde(flatten)]
    pub agent: PublicAgent,
    pub carrier: Carrier,
    pub skill_policy: SkillPolicyView,
    pub skill_ids: Vec<String>,
    pub skill_digest: Option<String>,
    pub prompt_config: Option<PromptSummary>,
}

impl AgentViewV2 {
    pub fn from_record(agent: &AgentRecord, prompt_config: Option<PromptSummary>) -> Self {
        let (skill_policy, skill_ids, skill_digest) = match &agent.skill_authorization {
            Some(authorization) => {
                let ids: Vec<String> = authorization
                    .grants
                    .iter()
                    .map(|grant| grant.id.clone())
                    .collect();
                let mode = match authorization.mode {
                    crate::skill::SkillPolicyMode::All => SkillPolicyViewMode::All,
                    crate::skill::SkillPolicyMode::Only => SkillPolicyViewMode::Only,
                };
                (
                    SkillPolicyView { mode, ids: ids.clone() },
                    ids,
                    Some(authorization.digest.clone()),
                )
            }
            None => (
                SkillPolicyView {
                    mode: SkillPolicyViewMode::Legacy,
                    ids: Vec::new(),
                },
                Vec::new(),
                None,
            ),
        };
        Self {
            agent: PublicAgent::from(agent),
            carrier: agent.carrier,
            skill_policy,
            skill_ids,
            skill_digest,
            prompt_config,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum AgentView {
    V1(PublicAgent),
    V2(AgentViewV2),
}

impl AgentView {
    pub fn for_api(view: ApiView, record: &AgentRecord, prompt: Option<PromptSummary>) -> Self {
        match view {
            ApiView::V1 => Self::V1(PublicAgent::from(record)),
            ApiView::V2 => Self::V2(AgentViewV2::from_record(record, prompt)),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ControlResult {
    Agent(AgentView),
    Rpc(serde_json::Value),
}

impl From<AgentView> for ControlResult {
    fn from(value: AgentView) -> Self {
        Self::Agent(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileView {
    pub name: String,
    pub digest: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotV1 {
    pub version: String,
    pub profile: ProfileView,
    pub agents: Vec<PublicAgent>,
    pub event_cursor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotV2 {
    pub version: String,
    pub profile: ProfileView,
    pub agents: Vec<AgentViewV2>,
    pub active_count: usize,
    pub managed_count: usize,
    pub event_cursor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Snapshot {
    V1(SnapshotV1),
    V2(SnapshotV2),
}
