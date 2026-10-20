use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::HubResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillPolicyMode {
    All,
    Only,
}

/// Creation-time policy: `{mode:"all"}` or `{mode:"only",ids:[...]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum SkillPolicy {
    All,
    Only { ids: Vec<String> },
}

impl SkillPolicy {
    pub fn mode(&self) -> SkillPolicyMode {
        match self {
            Self::All => SkillPolicyMode::All,
            Self::Only { .. } => SkillPolicyMode::Only,
        }
    }

    pub fn ids(&self) -> &[String] {
        match self {
            Self::All => &[],
            Self::Only { ids } => ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillGrant {
    pub id: String,
    pub sha256: String,
    pub path: PathBuf,
}

/// Frozen grant set written to `skill_authorization_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillAuthorization {
    pub mode: SkillPolicyMode,
    pub grants: Vec<SkillGrant>,
    pub digest: String,
}

/// Profile resource entry `{path,sha256}` (pre-platform v1 profile).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRef {
    pub path: PathBuf,
    pub sha256: String,
}

/// `skills_json` has two historical shapes. Keep both without rewriting old rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SkillRef {
    Grant {
        id: String,
        path: PathBuf,
        sha256: String,
    },
    Profile {
        path: PathBuf,
        sha256: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillPolicyViewMode {
    All,
    Only,
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPolicyView {
    pub mode: SkillPolicyViewMode,
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillVersion {
    pub sha256: String,
    pub source: String,
    pub imported_at: String,
    pub file_count: u64,
    pub byte_count: u64,
}

/// `/v2/skills` catalog row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub digest: String,
    pub current_sha256: String,
    pub source: String,
    pub imported_at: String,
    pub versions: Vec<SkillVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillImportInput {
    pub id: String,
    pub source_path: PathBuf,
    #[serde(default)]
    pub source_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillImportResult {
    pub id: String,
    pub sha256: String,
    pub source: String,
    pub imported_at: String,
    pub file_count: u64,
    pub byte_count: u64,
    pub path: PathBuf,
}

/// File-system backed skill catalog. Implementations serialize imports with a
/// private lock and never follow symlinks inside a package or snapshot.
#[async_trait::async_trait]
pub trait SkillRegistry: Send + Sync {
    async fn list(&self) -> HubResult<Vec<SkillSummary>>;
    async fn import(&self, input: SkillImportInput) -> HubResult<SkillImportResult>;
    async fn resolve_authorization(
        &self,
        policy: Option<&SkillPolicy>,
    ) -> HubResult<SkillAuthorization>;
    async fn verify(&self, authorization: &SkillAuthorization) -> HubResult<()>;
}
