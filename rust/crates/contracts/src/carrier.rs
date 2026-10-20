use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{Carrier, RuntimeRequestKind};
use crate::prompt::PromptChannel;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarrierCapabilities {
    pub messages: Vec<RuntimeRequestKind>,
    pub events: bool,
    pub skill_loading_selection: bool,
    pub skill_isolation: bool,
    #[serde(default)]
    pub system_prompt: Option<PromptChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarrierDiagnostic {
    pub id: Carrier,
    pub display_name: String,
    pub capabilities: CarrierCapabilities,
    pub available: bool,
    /// 2.1 emits `reason: null` for an available carrier; the key must stay
    /// present, so this is deliberately not `skip_serializing_if`.
    #[serde(default)]
    pub reason: Option<String>,
}

impl CarrierDiagnostic {
    /// Exact `/v2/carriers` row: diagnostic fields plus `label` = displayName.
    pub fn api_value(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("carrier diagnostic serializes");
        if let Some(object) = value.as_object_mut() {
            object.insert("label".to_string(), Value::String(self.display_name.clone()));
        }
        value
    }
}
