use serde::{Deserialize, Serialize};

use crate::error::HubResult;
use crate::ids::Carrier;

/// The four built-in principles, in declaration order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptPrinciple {
    pub id: String,
    pub title: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCatalog {
    pub principles: Vec<PromptPrinciple>,
    pub default_principle_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptConfigInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principle_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    Append,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptChannel {
    Append,
    Developer,
}

/// Frozen prompt configuration stored in `prompt_config_json` and the private
/// `prompt-config.json` file. `schemaVersion` must be 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenPromptConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub mode: PromptMode,
    pub carrier: Carrier,
    #[serde(default)]
    pub channel: Option<PromptChannel>,
    #[serde(default)]
    pub support: Option<PromptChannel>,
    pub principle_ids: Vec<String>,
    #[serde(default)]
    pub custom_text: String,
    pub prompt: String,
    pub digest: String,
    #[serde(default)]
    pub resolved_at: Option<String>,
}

fn default_schema_version() -> u32 {
    1
}

/// Public, non-secret prompt view. Text is opt-in per Agent detail.
/// `custom_text_chars` / `prompt_chars` use UTF-16 code units (JS `.length`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSummary {
    pub mode: PromptMode,
    #[serde(default)]
    pub carrier: Option<Carrier>,
    #[serde(default)]
    pub channel: Option<PromptChannel>,
    #[serde(default)]
    pub support: Option<PromptChannel>,
    pub principle_ids: Vec<String>,
    pub custom_text_chars: usize,
    pub prompt_chars: usize,
    pub digest: String,
    #[serde(default)]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Pure prompt resolution service. No I/O and no async.
pub trait PromptService: Send + Sync {
    fn list(&self) -> PromptCatalog;

    fn resolve(
        &self,
        input: Option<&PromptConfigInput>,
        carrier: Carrier,
        resolved_at: Option<String>,
    ) -> HubResult<FrozenPromptConfig>;

    /// Parse a stored column/file value. Returns `None` for empty or malformed
    /// input, mirroring `readFrozenPromptConfig`.
    fn read_frozen(&self, raw: Option<&str>) -> Option<FrozenPromptConfig>;

    fn summary(&self, frozen: &FrozenPromptConfig, include_text: bool) -> PromptSummary;
}
