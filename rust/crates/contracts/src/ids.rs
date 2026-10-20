use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{HubError, HubResult};
use crate::prompt::PromptChannel;

pub const APP_NAME: &str = "pi-subagent-hub";
pub const CLI_NAME: &str = "pihub";
/// Source/wire version. Updated to 3.0.0.0 only at the final Rust milestone.
pub const APP_VERSION: &str = "3.0.0.0";
pub const RELEASE_CHANNEL: &str = "development";
pub const API_VERSION: u32 = 1;
pub const DEFAULT_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Provisioning,
    Starting,
    Idle,
    Running,
    WaitingInput,
    Stopping,
    Stopped,
    Crashed,
    Failed,
}

impl AgentStatus {
    pub const ACTIVE: [Self; 6] = [
        Self::Provisioning,
        Self::Starting,
        Self::Idle,
        Self::Running,
        Self::WaitingInput,
        Self::Stopping,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Crashed => "crashed",
            Self::Failed => "failed",
        }
    }

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }

    pub fn is_terminal(self) -> bool {
        !self.is_active()
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentStatus {
    type Err = HubError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "provisioning" => Ok(Self::Provisioning),
            "starting" => Ok(Self::Starting),
            "idle" => Ok(Self::Idle),
            "running" => Ok(Self::Running),
            "waiting_input" => Ok(Self::WaitingInput),
            "stopping" => Ok(Self::Stopping),
            "stopped" => Ok(Self::Stopped),
            "crashed" => Ok(Self::Crashed),
            "failed" => Ok(Self::Failed),
            _ => Err(HubError::new(
                "invalid_agent_status",
                format!("unknown agent status: {value}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

impl FromStr for DesiredState {
    type Err = HubError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "running" => Ok(Self::Running),
            "stopped" => Ok(Self::Stopped),
            _ => Err(HubError::new(
                "invalid_desired_state",
                format!("unknown desired state: {value}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl FromStr for ThinkingLevel {
    type Err = HubError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err(HubError::new(
                "invalid_thinking",
                format!("unsupported thinking level: {value}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Carrier {
    Pi,
    Codex,
    Claude,
}

impl Carrier {
    pub const ALL: [Self; 3] = [Self::Pi, Self::Codex, Self::Claude];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// Additive system-prompt channel for this carrier. Every supported
    /// carrier has exactly one channel; there is no unreachable `None`.
    pub fn prompt_channel(self) -> PromptChannel {
        match self {
            Self::Pi | Self::Claude => PromptChannel::Append,
            Self::Codex => PromptChannel::Developer,
        }
    }
}

impl Default for Carrier {
    fn default() -> Self {
        Self::Pi
    }
}

impl fmt::Display for Carrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Carrier {
    type Err = HubError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pi" => Ok(Self::Pi),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => Err(HubError::new(
                "invalid_carrier",
                format!("unknown carrier: {value}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderApp {
    Claude,
    Codex,
    Pi,
}

impl ProviderApp {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Pi];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }
}

impl fmt::Display for ProviderApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderApp {
    type Err = HubError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "pi" => Ok(Self::Pi),
            _ => Err(HubError::new(
                "invalid_provider_app",
                format!("unsupported provider app: {value}"),
            )),
        }
    }
}

impl From<Carrier> for ProviderApp {
    fn from(value: Carrier) -> Self {
        match value {
            Carrier::Pi => Self::Pi,
            Carrier::Codex => Self::Codex,
            Carrier::Claude => Self::Claude,
        }
    }
}

/// Runtime RPC verbs shared by all carriers. External payloads stay `Value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRequestKind {
    Prompt,
    Steer,
    FollowUp,
    Abort,
    ClearQueue,
    GetState,
}

impl RuntimeRequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Steer => "steer",
            Self::FollowUp => "follow_up",
            Self::Abort => "abort",
            Self::ClearQueue => "clear_queue",
            Self::GetState => "get_state",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlAction {
    Abort,
    Stop,
    Restart,
}

/// v1 is the legacy store view; v2 is the platform view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiView {
    V1,
    V2,
}

pub fn is_valid_agent_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 3 || bytes.len() > 128 {
        return false;
    }
    let first = bytes[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
    })
}

pub fn assert_agent_id(value: &str) -> HubResult<&str> {
    if is_valid_agent_id(value) {
        Ok(value)
    } else {
        Err(HubError::new(
            "invalid_agent_id",
            "agentId must be 3-128 characters using letters, digits, dot, underscore, or hyphen",
        ))
    }
}
