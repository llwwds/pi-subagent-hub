//! Manifest readers (`schemaVersion` 1 -> `/v1/agents`, 2 -> `/v2/agents`).
//!
//! Batch creation is per-item fault tolerant: one failing row never aborts the
//! rest, and each row reports `{ok, agentId, agent}` or `{ok, agentId, error}`.

use pihub_contracts::{HubError, HubResult};
use serde_json::{json, Value};

use crate::cli::read_stdin;
use crate::client::HubClient;
use crate::error;

/// Which manifest dialect to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    V1,
    V2,
}

pub async fn create_from_manifest(
    client: &HubClient,
    path: &str,
    kind: ManifestKind,
) -> HubResult<Value> {
    let raw = if path == "-" {
        read_stdin()?
    } else {
        std::fs::read_to_string(path).map_err(|cause| {
            error::command_failed(format!("cannot read manifest {path}: {cause}"))
        })?
    };
    let manifest: Value = serde_json::from_str(&raw)
        .map_err(|cause| error::command_failed(format!("manifest is not valid JSON: {cause}")))?;

    let expected_version = match kind {
        ManifestKind::V1 => 1,
        ManifestKind::V2 => 2,
    };
    if manifest.get("schemaVersion").and_then(Value::as_u64) != Some(expected_version) {
        return Err(error::command_failed(match kind {
            ManifestKind::V1 => "manifest must have schemaVersion 1 and an agents array",
            ManifestKind::V2 => "platform manifest must have schemaVersion 2 and an agents array",
        }));
    }
    let Some(items) = manifest.get("agents").and_then(Value::as_array) else {
        return Err(error::command_failed(match kind {
            ManifestKind::V1 => "manifest must have schemaVersion 1 and an agents array",
            ManifestKind::V2 => "platform manifest must have schemaVersion 2 and an agents array",
        }));
    };

    // The batch is a write: refuse before the first item so a cross-major
    // daemon never receives a partial create.
    client.ensure_control_compatible().await?;

    let base_prompt_config = manifest.get("promptConfig").cloned();
    let mut results: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        let agent_id_hint = item
            .get("agentId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let body = match build_body(item, kind, base_prompt_config.as_ref()) {
            Ok(body) => body,
            Err(failure) => {
                results.push(json!({
                    "ok": false,
                    "agentId": agent_id_hint,
                    "error": { "code": failure.code, "message": failure.message },
                }));
                continue;
            }
        };
        let path = match kind {
            ManifestKind::V1 => "/v1/agents",
            ManifestKind::V2 => "/v2/agents",
        };
        match client
            .request(reqwest::Method::POST, path, Some(&body))
            .await
        {
            Ok(created) => {
                let id = created
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(agent_id_hint);
                results.push(json!({ "ok": true, "agentId": id, "agent": created }));
            }
            Err(failure) => {
                let display_id = failure
                    .details
                    .as_ref()
                    .and_then(|details| details.get("agentId"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(agent_id_hint);
                results.push(json!({
                    "ok": false,
                    "agentId": display_id,
                    "error": { "code": failure.code, "message": failure.message },
                }));
            }
        }
    }

    Ok(json!({
        "groupName": manifest.get("groupName").cloned().unwrap_or(Value::Null),
        "results": results,
    }))
}

/// Build one `POST /v1|/v2/agents` body from a manifest item.
fn build_body(item: &Value, kind: ManifestKind, base_prompt_config: Option<&Value>) -> HubResult<Value> {
    let model_field = item.get("model");
    let (model_provider, model_id, model_thinking) = match model_field {
        Some(Value::String(text)) => {
            let (provider, model) = crate::cli::spec::parse_model(Some(text));
            (provider, model, None)
        }
        Some(Value::Object(_)) => (
            model_field
                .and_then(|value| value.get("provider"))
                .and_then(Value::as_str)
                .map(str::to_string),
            model_field
                .and_then(|value| value.get("id"))
                .or_else(|| model_field.and_then(|value| value.get("model")))
                .and_then(Value::as_str)
                .map(str::to_string),
            model_field
                .and_then(|value| value.get("thinking"))
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        Some(Value::Null) | None => (None, None, None),
        Some(_) => {
            return Err(error::command_failed(
                "manifest item model must be a string or an object",
            ));
        }
    };

    let prompt_config = match kind {
        ManifestKind::V1 => item.get("promptConfig").cloned(),
        ManifestKind::V2 => crate::cli::spec::merge_prompt_config(
            base_prompt_config,
            item.get("promptConfig"),
            item.get("systemPrompt").and_then(Value::as_str),
            item.get("principles"),
        ),
    };

    let mut body = json!({
        "agentId": item.get("agentId").cloned().unwrap_or(Value::Null),
        "name": item.get("name").cloned().unwrap_or(Value::Null),
        "cwd": item.get("cwd").cloned().unwrap_or(Value::Null),
        "workspaceMode": item
            .get("workspaceMode")
            .and_then(Value::as_str)
            .unwrap_or("shared"),
        "provider": model_provider
            .or_else(|| item.get("provider").and_then(Value::as_str).map(str::to_string)),
        "model": model_id,
        "thinking": model_thinking.or_else(|| {
            item.get("thinking")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        "prompt": item.get("prompt").cloned().unwrap_or(Value::Null),
    });

    let object = body
        .as_object_mut()
        .ok_or_else(|| error::command_failed("manifest item must be a JSON object"))?;
    for key in ["agentId", "name", "cwd"] {
        if object.get(key).map(Value::is_null).unwrap_or(false) {
            object.remove(key);
        }
    }
    if object.get("prompt").map(Value::is_null).unwrap_or(false) {
        object.remove("prompt");
    }
    if let Some(prompt_config) = prompt_config
        && !prompt_config.is_null()
    {
        object.insert("promptConfig".to_string(), prompt_config);
    }
    if kind == ManifestKind::V2 {
        object.insert(
            "carrier".to_string(),
            Value::String(
                item.get("carrier")
                    .and_then(Value::as_str)
                    .unwrap_or("pi")
                    .to_string(),
            ),
        );
        object.insert(
            "skillPolicy".to_string(),
            item.get("skillPolicy")
                .cloned()
                .unwrap_or_else(|| json!({ "mode": "all" })),
        );
    }
    Ok(body)
}

/// Convenience used by the CLI's `--manifest` handling.
pub fn manifest_path(parsed: &crate::cli::args::Parsed) -> Option<String> {
    parsed.value("manifest").map(str::to_string)
}

/// A manifest with no `agents` array is a caller error, not a Hub error.
pub fn manifest_shape_error(kind: ManifestKind) -> HubError {
    error::command_failed(match kind {
        ManifestKind::V1 => "manifest must have schemaVersion 1 and an agents array",
        ManifestKind::V2 => "platform manifest must have schemaVersion 2 and an agents array",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_v1_body_from_string_model() {
        let item = json!({
            "agentId": "agent-1",
            "name": "one",
            "cwd": "/tmp",
            "model": "gw/model-a",
            "prompt": "hello"
        });
        let body = build_body(&item, ManifestKind::V1, None).unwrap();
        assert_eq!(body["provider"], "gw");
        assert_eq!(body["model"], "model-a");
        assert_eq!(body["workspaceMode"], "shared");
        assert!(body.get("carrier").is_none());
        assert!(body.get("skillPolicy").is_none());
    }

    #[test]
    fn builds_v2_body_with_platform_fields() {
        let item = json!({
            "name": "two",
            "cwd": "/tmp",
            "model": { "provider": "gw", "id": "model-b", "thinking": "high" },
            "carrier": "codex",
            "skillPolicy": { "mode": "only", "ids": ["s1"] },
            "systemPrompt": "be brief"
        });
        let base = json!({ "principleIds": ["first-principles"] });
        let body = build_body(&item, ManifestKind::V2, Some(&base)).unwrap();
        assert_eq!(body["provider"], "gw");
        assert_eq!(body["model"], "model-b");
        assert_eq!(body["thinking"], "high");
        assert_eq!(body["carrier"], "codex");
        assert_eq!(body["skillPolicy"]["ids"][0], "s1");
        assert_eq!(body["promptConfig"]["customText"], "be brief");
        assert_eq!(body["promptConfig"]["principleIds"][0], "first-principles");
    }
}
