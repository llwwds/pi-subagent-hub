use std::path::PathBuf;

use serde_json::{json, Value};
use pihub_contracts::{
    canonical_json, is_ephemeral_stream_event, is_safe_token, normalize_lexical, sha256_hex,
    sha256_prefixed, skill_authorization_digest, utf16_len, AgentRecord, AgentStatus, AgentView,
    Carrier,
    DesiredState, Envelope, FrozenPromptConfig, HubPaths, PromptMode, PublicAgent, SkillAuthorization,
    SkillGrant, SkillPolicyMode,
};

#[test]
fn canonical_json_sorts_keys_even_with_preserve_order() {
    let value = json!({"b": 1, "a": {"d": 2, "c": 3}, "arr": [{"z": 1, "y": 2}]});
    assert_eq!(
        canonical_json(&value),
        r#"{"a":{"c":3,"d":2},"arr":[{"y":2,"z":1}],"b":1}"#
    );
}

#[test]
fn utf16_length_matches_javascript() {
    assert_eq!(utf16_len("abc"), 3);
    assert_eq!(utf16_len("a😀b"), 4);
    assert_eq!(utf16_len("中文"), 2);
}

#[test]
fn skill_authorization_digest_is_sorted_json_stringify() {
    let grants = vec![
        SkillGrant {
            id: "b".into(),
            sha256: "11".into(),
            path: PathBuf::from("/x/b"),
        },
        SkillGrant {
            id: "a".into(),
            sha256: "00".into(),
            path: PathBuf::from("/x/a"),
        },
    ];
    let expected = sha256_prefixed(
        r#"{"mode":"only","grants":[{"id":"a","sha256":"00"},{"id":"b","sha256":"11"}]}"#,
    );
    assert_eq!(
        skill_authorization_digest(SkillPolicyMode::Only, &grants),
        expected
    );
    assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}

#[test]
fn ephemeral_filter_unwraps_bounded_carrier_event() {
    assert!(is_ephemeral_stream_event(&json!({"type": "message_update"})));
    assert!(is_ephemeral_stream_event(
        &json!({"type": "carrier_event", "value": {"type": "text_delta"}})
    ));
    assert!(!is_ephemeral_stream_event(
        &json!({"type": "item.completed"})
    ));
    assert!(!is_ephemeral_stream_event(&json!({"type": "tool_execution_start"})));

    let mut nested = json!({"type": "message_update"});
    for _ in 0..32 {
        nested = json!({"type": "carrier_event", "value": nested});
    }
    assert!(is_ephemeral_stream_event(&nested));
    nested = json!({"type": "carrier_event", "value": nested});
    assert!(!is_ephemeral_stream_event(&nested));
}

fn sample_record() -> AgentRecord {
    AgentRecord {
        id: "agent-123".into(),
        name: "agent".into(),
        cwd: PathBuf::from("/tmp/work"),
        workspace_mode: "shared".into(),
        provider: Some("my-gateway".into()),
        model: Some("model-a".into()),
        thinking: None,
        status: AgentStatus::Idle,
        desired_state: DesiredState::Running,
        pid: Some(42),
        session_id: "session-1".into(),
        session_file: None,
        session_dir: PathBuf::from("/tmp/home/agents/agent-123/sessions"),
        agent_dir: PathBuf::from("/tmp/home/agents/agent-123/pi-agent"),
        logs_dir: PathBuf::from("/tmp/home/agents/agent-123/logs"),
        profile_digest: "sha256:profile".into(),
        tools: vec!["read".into()],
        skills: Vec::new(),
        extensions: Vec::new(),
        carrier: Carrier::Pi,
        skill_authorization: Some(SkillAuthorization {
            mode: SkillPolicyMode::Only,
            grants: vec![SkillGrant {
                id: "review".into(),
                sha256: "aa".into(),
                path: PathBuf::from("/tmp/home/skills/objects/aa"),
            }],
            digest: "sha256:auth".into(),
        }),
        prompt_config: Some(FrozenPromptConfig {
            schema_version: 1,
            mode: PromptMode::None,
            carrier: Carrier::Pi,
            channel: Some(pihub_contracts::PromptChannel::Append),
            support: Some(pihub_contracts::PromptChannel::Append),
            principle_ids: Vec::new(),
            custom_text: String::new(),
            prompt: String::new(),
            digest: "sha256:prompt".into(),
            resolved_at: None,
        }),
        last_error: None,
        created_at: "2026-10-20T00:00:00.000Z".into(),
        updated_at: "2026-10-20T00:00:00.000Z".into(),
    }
}

#[test]
fn v1_and_v2_views_separate_internal_columns() {
    let record = sample_record();
    let v1 = serde_json::to_value(AgentView::V1(PublicAgent::from(&record))).unwrap();
    assert!(v1.get("carrier").is_none());
    assert!(v1.get("promptConfig").is_none());
    assert!(v1.get("agent_dir").is_none());
    assert!(v1.get("skill_authorization").is_none());

    let v2 = serde_json::to_value(AgentView::V2(
        pihub_contracts::AgentViewV2::from_record(
            &record,
            Some(pihub_contracts::PromptSummary {
                mode: PromptMode::None,
                carrier: Some(Carrier::Pi),
                channel: None,
                support: None,
                principle_ids: Vec::new(),
                custom_text_chars: 0,
                prompt_chars: 0,
                digest: "sha256:prompt".into(),
                resolved_at: None,
                custom_text: None,
                prompt: None,
            }),
        ),
    ))
    .unwrap();
    assert_eq!(v2["carrier"], "pi");
    assert_eq!(v2["skillPolicy"]["mode"], "only");
    assert_eq!(v2["skillIds"][0], "review");
    assert_eq!(v2["skillDigest"], "sha256:auth");
    assert_eq!(v2["promptConfig"]["mode"], "none");
    assert!(v2.get("agent_dir").is_none());
}

/// A/B/J parity with 2.1 `publicAgent()` / `publicAgentV2()`:
/// - both views keep the hydrated `tools`/`skills`/`extensions` columns and
///   only drop their `*_json` counterparts;
/// - for a frozen authorization `skills` carries the grant objects;
/// - `CarrierDiagnostic.reason` is `null`, never an absent key;
/// - `StoredEvent` keeps the raw `payload_json` text next to `payload`.
#[test]
fn public_views_keep_hydrated_profile_columns_like_21() {
    // 2.1 writes `skills_json = grants` at create time when an authorization
    // exists, so the hydrated row (the single source of truth) is what both
    // public views project.
    let mut record = sample_record();
    record.skills = vec![pihub_contracts::SkillRef::Grant {
        id: "review".into(),
        path: PathBuf::from("/tmp/home/skills/objects/aa"),
        sha256: "aa".into(),
    }];
    let v1 = serde_json::to_value(AgentView::V1(PublicAgent::from(&record))).unwrap();
    let v2 = serde_json::to_value(AgentView::V2(
        pihub_contracts::AgentViewV2::from_record(&record, None),
    ))
    .unwrap();

    for view in [&v1, &v2] {
        assert_eq!(view["tools"], json!(["read"]));
        assert_eq!(
            view["skills"],
            json!([{
                "id": "review",
                "path": "/tmp/home/skills/objects/aa",
                "sha256": "aa",
            }])
        );
        assert_eq!(view["extensions"], json!([]));
        assert!(view.get("tools_json").is_none());
        assert!(view.get("skills_json").is_none());
        assert!(view.get("extensions_json").is_none());
        // 20 keys of the 2.1 v1 shape are a subset of the v2 shape.
        assert!(view.get("session_dir").is_some());
    }

    let carrier = pihub_contracts::CarrierDiagnostic {
        id: Carrier::Pi,
        display_name: "Pi".into(),
        capabilities: pihub_contracts::CarrierCapabilities {
            messages: vec![pihub_contracts::RuntimeRequestKind::Prompt],
            events: true,
            skill_loading_selection: true,
            skill_isolation: true,
            system_prompt: None,
        },
        available: true,
        reason: None,
    };
    let row = carrier.api_value();
    assert!(row.get("reason").is_some(), "reason key must not be skipped");
    assert_eq!(row["reason"], Value::Null);
    assert_eq!(row["label"], "Pi");

    let event = pihub_contracts::StoredEvent {
        cursor: 7,
        event_id: "evt".into(),
        agent_id: None,
        event_type: "agent_created".into(),
        payload: json!({"name": "a"}),
        payload_json: "{\"name\":\"a\"}".into(),
        created_at: "2026-10-20T00:00:00.000Z".into(),
    };
    let row = serde_json::to_value(event).unwrap();
    assert_eq!(row["payload_json"], "{\"name\":\"a\"}");
    assert_eq!(row["payload"]["name"], "a");
}

#[test]
fn envelope_matches_wire_shape() {
    let success = serde_json::to_value(Envelope::success(json!({"x": 1}))).unwrap();
    assert_eq!(success, json!({"ok": true, "data": {"x": 1}, "error": null}));

    let failure = serde_json::to_value(Envelope::<Value>::failure(
        pihub_contracts::HubError::new("unauthorized", "token required").body(),
    ))
    .unwrap();
    assert_eq!(failure["ok"], false);
    assert_eq!(failure["data"], Value::Null);
    assert_eq!(failure["error"]["code"], "unauthorized");
    assert_eq!(failure["error"]["retryable"], false);
    assert_eq!(failure["error"]["details"], Value::Null);
}

#[test]
fn hub_home_must_be_outside_source_tree() {
    let paths = HubPaths::resolve("/tmp/pihub-home", "/Users/example/pi-subagent-hub");
    assert!(paths.is_state_home_outside_app());
    let equal = HubPaths::resolve("/Users/example/pi-subagent-hub", "/Users/example/pi-subagent-hub");
    assert!(!equal.is_state_home_outside_app());
    let nested = HubPaths::resolve(
        "/Users/example/pi-subagent-hub/.state",
        "/Users/example/pi-subagent-hub",
    );
    assert!(!nested.is_state_home_outside_app());
    assert_eq!(paths.database_path, PathBuf::from("/tmp/pihub-home/state/hub.sqlite"));
    assert_eq!(paths.models_path, PathBuf::from("/tmp/pihub-home/config/pi-shared/models.json"));
}

#[test]
fn carrier_prompt_channel_has_no_unreachable_none() {
    use pihub_contracts::PromptChannel;
    assert_eq!(Carrier::Pi.prompt_channel(), PromptChannel::Append);
    assert_eq!(Carrier::Claude.prompt_channel(), PromptChannel::Append);
    assert_eq!(Carrier::Codex.prompt_channel(), PromptChannel::Developer);
}

#[test]
fn safe_token_keeps_call_site_length_boundaries() {
    assert!(is_safe_token("a", 1));
    assert!(is_safe_token("a.b_c-d", 8));
    assert!(!is_safe_token(".a", 8));
    assert!(!is_safe_token("a/b", 8));
    assert!(!is_safe_token("", 8));
    assert!(!is_safe_token(&"a".repeat(81), 80));
    assert!(is_safe_token(&"a".repeat(101), 101));
    assert!(!is_safe_token(&"a".repeat(102), 101));
    assert_eq!(normalize_lexical(std::path::Path::new("/a/./b/../c")), std::path::PathBuf::from("/a/c"));
}

#[test]
fn control_result_uses_agent_view_without_duplicate_entity() {
    let record = sample_record();
    let result = pihub_contracts::ControlResult::from(AgentView::V2(
        pihub_contracts::AgentViewV2::from_record(&record, None),
    ));
    let value = serde_json::to_value(result).unwrap();
    assert_eq!(value["id"], "agent-123");
    assert_eq!(value["carrier"], "pi");
}

#[test]
fn event_query_clamps_zero_to_default_500() {
    let mut query = pihub_contracts::EventQuery::default();
    assert_eq!(query.clamped_limit(), 500);
    query.limit = 0;
    assert_eq!(query.clamped_limit(), 500);
    query.limit = 1;
    assert_eq!(query.clamped_limit(), 1);
    query.limit = 2001;
    assert_eq!(query.clamped_limit(), 2000);
}
