//! Creation-spec normalisation shared by `spawn`, `agents create`, and both
//! manifest readers (`src/cli.js:normalizeCreateSpec` and friends).

use std::path::PathBuf;

use pihub_contracts::{
    AgentSpec, Carrier, HubError, PromptConfigInput, SkillPolicy, ThinkingLevel,
};
use serde_json::Value;

use crate::cli::args::Parsed;
use crate::error;

/// `PROVIDER/MODEL` split: no slash means "model only", a leading slash means
/// "provider only" is invalid and is treated as a plain model.
pub fn parse_model(value: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(value) = value else {
        return (None, None);
    };
    match value.split_once('/') {
        Some((provider, model)) if !provider.is_empty() => (
            Some(provider.to_string()),
            Some(model.to_string()).filter(|model| !model.is_empty()),
        ),
        _ => (None, Some(value.to_string())),
    }
}

pub fn normalize_skill_policy(value: Option<&str>) -> SkillPolicy {
    match value {
        None | Some("all") | Some("") => SkillPolicy::All,
        Some("none") => SkillPolicy::Only { ids: Vec::new() },
        Some(list) => SkillPolicy::Only {
            ids: list
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect(),
        },
    }
}

/// Skill-policy mapping used by the 2.1 CLI: `all`, `none`, or a CSV of ids.
pub fn normalize_principle_ids(value: Option<&str>) -> Option<Vec<String>> {
    match value {
        None | Some("all") | Some("") => None,
        Some("none") => Some(Vec::new()),
        Some(list) => Some(
            list.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect(),
        ),
    }
}

/// At most one system-prompt source may be supplied.
pub fn read_system_prompt(parsed: &Parsed) -> Result<Option<String>, HubError> {
    let mut sources: Vec<(&str, Option<String>)> = Vec::new();
    if let Some(value) = parsed.value("system-prompt") {
        sources.push(("--system-prompt", Some(value.to_string())));
    }
    if let Some(value) = parsed.value("system-prompt-file") {
        sources.push(("--system-prompt-file", Some(value.to_string())));
    }
    if parsed.is_bool_set("system-prompt-stdin") {
        sources.push(("--system-prompt-stdin", None));
    }
    if sources.len() > 1 {
        return Err(error::command_failed(
            "use only one of --system-prompt, --system-prompt-file, or --system-prompt-stdin",
        ));
    }
    match sources.pop() {
        None => Ok(None),
        Some(("--system-prompt", Some(value))) => Ok(Some(value)),
        Some(("--system-prompt-stdin", _)) => Ok(Some(crate::cli::read_stdin()?)),
        Some((_, Some(path))) => std::fs::read_to_string(&path).map(Some).map_err(|cause| {
            error::command_failed(format!("cannot read system prompt file {path}: {cause}"))
        }),
        Some((_, None)) => Ok(None),
    }
}

pub fn normalize_prompt_config(parsed: &Parsed) -> Result<Option<PromptConfigInput>, HubError> {
    let custom_text = read_system_prompt(parsed)?;
    let principle_ids = normalize_principle_ids(parsed.value("principles"));
    if custom_text.is_none() && principle_ids.is_none() {
        return Ok(None);
    }
    Ok(Some(PromptConfigInput {
        principle_ids,
        custom_text,
    }))
}

pub fn thinking_from(parsed: &Parsed) -> Result<Option<ThinkingLevel>, HubError> {
    match parsed.value("thinking") {
        None | Some("") => Ok(None),
        Some(value) => value.parse().map(Some),
    }
}

pub fn carrier_from(value: Option<&str>) -> Result<Carrier, HubError> {
    match value {
        None | Some("") => Ok(Carrier::Pi),
        Some(value) => value.parse(),
    }
}

/// v1 `spawn` spec.
pub fn normalize_create_spec(parsed: &Parsed) -> Result<AgentSpec, HubError> {
    let (provider, model) = parse_model(parsed.value("model"));
    Ok(AgentSpec {
        agent_id: parsed.value("id").map(str::to_string),
        name: parsed.value("name").map(str::to_string),
        cwd: Some(
            parsed
                .value("cwd")
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir().map_err(HubError::from)?),
        ),
        workspace_mode: Some(
            parsed
                .value("workspace-mode")
                .unwrap_or("shared")
                .to_string(),
        ),
        provider: parsed
            .value("provider")
            .map(str::to_string)
            .or(provider),
        model,
        thinking: thinking_from(parsed)?,
        prompt: parsed.value("prompt").map(str::to_string),
        carrier: Carrier::Pi,
        skill_policy: None,
        prompt_config: None,
    })
}

/// v2 `agents create` spec.
pub fn normalize_platform_spec(parsed: &Parsed) -> Result<AgentSpec, HubError> {
    let mut spec = normalize_create_spec(parsed)?;
    spec.carrier = carrier_from(parsed.value("carrier"))?;
    spec.skill_policy = Some(normalize_skill_policy(parsed.value("skills")));
    spec.prompt_config = normalize_prompt_config(parsed)?;
    Ok(spec)
}

/// Merge a manifest-level `promptConfig` with a per-item override, matching
/// `src/cli.js:manifestPromptConfig`.
pub fn merge_prompt_config(
    base: Option<&Value>,
    item_config: Option<&Value>,
    item_system_prompt: Option<&str>,
    item_principles: Option<&Value>,
) -> Option<Value> {
    let mut merged = base
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(overrides) = item_config.and_then(Value::as_object) {
        for (key, value) in overrides {
            merged.insert(key.clone(), value.clone());
        }
        return Some(Value::Object(merged));
    }
    if let Some(text) = item_system_prompt {
        merged.insert("customText".to_string(), Value::String(text.to_string()));
        return Some(Value::Object(merged));
    }
    if let Some(principles) = item_principles {
        merged.insert("principleIds".to_string(), principles.clone());
        return Some(Value::Object(merged));
    }
    if merged.is_empty() {
        None
    } else {
        Some(Value::Object(merged))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_provider_and_model() {
        assert_eq!(
            parse_model(Some("gw/model-a")),
            (Some("gw".to_string()), Some("model-a".to_string()))
        );
        assert_eq!(parse_model(Some("model-a")), (None, Some("model-a".to_string())));
        assert_eq!(parse_model(Some("/model-a")), (None, Some("/model-a".to_string())));
        assert_eq!(parse_model(None), (None, None));
    }

    #[test]
    fn maps_skill_policy_modes() {
        assert!(matches!(normalize_skill_policy(None), SkillPolicy::All));
        assert!(matches!(normalize_skill_policy(Some("all")), SkillPolicy::All));
        match normalize_skill_policy(Some("none")) {
            SkillPolicy::Only { ids } => assert!(ids.is_empty()),
            SkillPolicy::All => panic!("expected only"),
        }
        match normalize_skill_policy(Some("a, b")) {
            SkillPolicy::Only { ids } => assert_eq!(ids, vec!["a", "b"]),
            SkillPolicy::All => panic!("expected only"),
        }
    }

    #[test]
    fn maps_principle_lists() {
        assert_eq!(normalize_principle_ids(None), None);
        assert_eq!(normalize_principle_ids(Some("all")), None);
        assert_eq!(normalize_principle_ids(Some("none")), Some(Vec::new()));
        assert_eq!(
            normalize_principle_ids(Some("a,b")),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn merges_manifest_prompt_config() {
        let base = serde_json::json!({ "customText": "base", "principleIds": ["a"] });
        let item = serde_json::json!({ "customText": "item" });
        let merged = merge_prompt_config(Some(&base), Some(&item), None, None).unwrap();
        assert_eq!(merged["customText"], "item");
        assert_eq!(merged["principleIds"], serde_json::json!(["a"]));

        let merged = merge_prompt_config(Some(&base), None, Some("text"), None).unwrap();
        assert_eq!(merged["customText"], "text");

        let merged = merge_prompt_config(
            None,
            None,
            None,
            Some(&serde_json::json!(["b"])),
        )
        .unwrap();
        assert_eq!(merged["principleIds"], serde_json::json!(["b"]));

        assert!(merge_prompt_config(None, None, None, None).is_none());
    }
}
