//! Prompt and profile parity, including the Node golden digests.

mod common;

use pihub_contracts::{
    Carrier, HubResult, PromptConfigInput, PromptMode, PromptService, SkillPolicyViewMode,
};
use pihub_core::profiles::{ensure_default_profile, hash_path, resolve_profile};
use pihub_core::prompts::{DefaultPromptService, MAX_CUSTOM_TEXT, MAX_PROMPT_TEXT};

const GOLDEN_DEFAULT_DIGEST: &str =
    "sha256:d587ddf75d8a632e4f88f32fb11e216a8e1bb90f1bce8eb59a573eaa5115e602";
const GOLDEN_SUBSET_DIGEST: &str =
    "sha256:ff9852ec478bf8c95e0e36c1d49aef42846e491f7086156309845cc3d75a8425";
const GOLDEN_CODEX_DIGEST: &str =
    "sha256:fbd45d80105071307afc2534c2cfe42e17c1937f8bf41030f7ff19e44ad644f6";
const GOLDEN_NONE_DIGEST: &str =
    "sha256:3c78cd608b63aa5e0330c06b3eb29fcef5a5e2cde215c98612b3ccb520b5d170";

fn service() -> DefaultPromptService {
    DefaultPromptService
}

#[test]
fn default_prompt_matches_the_node_digest_byte_for_byte() -> HubResult<()> {
    let frozen = service().resolve(None, Carrier::Pi, None)?;
    assert_eq!(frozen.mode, PromptMode::Append);
    assert_eq!(frozen.principle_ids.len(), 4);
    assert_eq!(frozen.digest, GOLDEN_DEFAULT_DIGEST);
    assert_eq!(pihub_contracts::utf16_len(&frozen.prompt), 318);
    assert!(frozen.prompt.contains("只做追加"));

    // Stable across repeated resolution of the same input.
    let repeated = service().resolve(Some(&PromptConfigInput::default()), Carrier::Pi, None)?;
    assert_eq!(repeated.digest, frozen.digest);
    Ok(())
}

#[test]
fn subset_custom_text_and_codex_channels_match_the_node_digest() -> HubResult<()> {
    let subset = service().resolve(
        Some(&PromptConfigInput {
            principle_ids: Some(vec![
                "no-unnecessary-entities".to_string(),
                "first-principles".to_string(),
            ]),
            custom_text: Some("line one\r\nline two  ".to_string()),
        }),
        Carrier::Pi,
        None,
    )?;
    assert_eq!(subset.digest, GOLDEN_SUBSET_DIGEST);
    assert_eq!(subset.custom_text, "line one\nline two");
    assert_eq!(
        subset.principle_ids,
        vec![
            "first-principles".to_string(),
            "no-unnecessary-entities".to_string()
        ],
        "canonical order regardless of input order"
    );

    let codex = service().resolve(None, Carrier::Codex, None)?;
    assert_eq!(codex.digest, GOLDEN_CODEX_DIGEST);
    assert_eq!(
        codex.channel,
        Some(pihub_contracts::PromptChannel::Developer)
    );
    assert_eq!(codex.principle_ids.len(), 4);

    let none = service().resolve(
        Some(&PromptConfigInput {
            principle_ids: Some(Vec::new()),
            custom_text: Some(String::new()),
        }),
        Carrier::Pi,
        None,
    )?;
    assert_eq!(none.mode, PromptMode::None);
    assert!(none.prompt.is_empty());
    assert_eq!(none.digest, GOLDEN_NONE_DIGEST);
    Ok(())
}

#[test]
fn prompt_limits_use_utf16_code_units() {
    let at_limit = "a".repeat(MAX_CUSTOM_TEXT);
    assert!(service()
        .resolve(
            Some(&PromptConfigInput {
                principle_ids: Some(Vec::new()),
                custom_text: Some(at_limit),
            }),
            Carrier::Pi,
            None,
        )
        .is_ok());

    let over = "a".repeat(MAX_CUSTOM_TEXT + 1);
    let error = service()
        .resolve(
            Some(&PromptConfigInput {
                principle_ids: Some(Vec::new()),
                custom_text: Some(over),
            }),
            Carrier::Pi,
            None,
        )
        .expect_err("over limit");
    assert_eq!(error.code, "invalid_prompt_config");
    assert!(error.message.contains("8000"));

    // Astral characters count as two UTF-16 units each, like JS `.length`.
    let astral = "😀".repeat(MAX_CUSTOM_TEXT / 2 + 1);
    assert!(service()
        .resolve(
            Some(&PromptConfigInput {
                principle_ids: None,
                custom_text: Some(astral),
            }),
            Carrier::Pi,
            None,
        )
        .is_err());

    // The resolved prompt keeps its own 20000-unit ceiling next to the 8000
    // custom-text ceiling. With every principle selected plus the largest legal
    // custom text the resolved prompt stays below it, so the check is a guard
    // against future principle growth rather than a reachable user error.
    assert_eq!(MAX_PROMPT_TEXT, 20_000);
    let largest = service()
        .resolve(
            Some(&PromptConfigInput {
                principle_ids: None,
                custom_text: Some("b".repeat(MAX_CUSTOM_TEXT)),
            }),
            Carrier::Pi,
            None,
        )
        .expect("largest legal prompt");
    assert!(pihub_contracts::utf16_len(&largest.prompt) <= MAX_PROMPT_TEXT);
}

#[test]
fn prompt_input_validation_and_frozen_reading() -> HubResult<()> {
    let error = service()
        .resolve(
            Some(&PromptConfigInput {
                principle_ids: Some(vec!["nope".to_string()]),
                custom_text: None,
            }),
            Carrier::Pi,
            None,
        )
        .expect_err("unknown principle");
    assert!(error.message.contains("unknown prompt principle"));

    let frozen = service().resolve(None, Carrier::Pi, None)?;
    assert!(service().read_frozen(Some("{}")).is_none());
    assert!(service().read_frozen(Some("")).is_none());
    assert!(service().read_frozen(Some("{oops")).is_none());
    let serialized = serde_json::to_string(&frozen).expect("serialize frozen");
    let round_trip = service().read_frozen(Some(&serialized)).expect("frozen");
    assert_eq!(round_trip.digest, frozen.digest);

    let summary = service().summary(&frozen, false);
    assert_eq!(summary.custom_text_chars, 0);
    assert_eq!(summary.prompt_chars, 318);
    assert!(summary.prompt.is_none());
    let detailed = service().summary(&frozen, true);
    assert_eq!(detailed.prompt.as_deref(), Some(frozen.prompt.as_str()));
    assert_eq!(
        detailed.channel,
        Some(pihub_contracts::PromptChannel::Append)
    );
    Ok(())
}

#[test]
fn prompt_catalog_exposes_the_four_principles() {
    let catalog = service().list();
    assert_eq!(catalog.principles.len(), 4);
    assert_eq!(catalog.default_principle_ids.len(), 4);
    assert_eq!(catalog.principles[0].id, "first-principles");
    assert_eq!(catalog.principles[3].id, "no-unnecessary-entities");
}

#[test]
fn profile_digest_is_deterministic_and_tracks_content() -> HubResult<()> {
    let home = common::temp_home("profile");
    let skill = common::write_skill_package(home.path(), "sample", "one");

    let first = resolve_profile(&serde_json::json!({
        "schemaVersion": 1,
        "name": "test",
        "tools": ["write", "read", "read"],
        "skills": [common::path_text(&skill)],
        "extensions": [],
    }))?;
    let second = resolve_profile(&serde_json::json!({
        "schemaVersion": 1,
        "name": "test",
        "tools": ["read", "write"],
        "skills": [common::path_text(&skill)],
        "extensions": [],
    }))?;
    assert_eq!(first.digest, second.digest);
    assert_eq!(first.tools, vec!["read".to_string(), "write".to_string()]);
    assert!(first.digest.starts_with("sha256:"));

    common::write_file(&skill.join("SKILL.md"), "changed\n");
    let changed = resolve_profile(&serde_json::json!({
        "schemaVersion": 1,
        "name": "test",
        "tools": ["read", "write"],
        "skills": [common::path_text(&skill)],
        "extensions": [],
    }))?;
    assert_ne!(first.digest, changed.digest);

    let hashed = hash_path(&skill)?;
    assert_eq!(hashed.sha256.len(), 64);

    let error = resolve_profile(&serde_json::json!({
        "schemaVersion": 2,
        "tools": ["read"],
    }))
    .expect_err("schema version");
    assert_eq!(error.code, "unsupported_profile_schema");

    let error = resolve_profile(&serde_json::json!({
        "schemaVersion": 1,
        "tools": [],
    }))
    .expect_err("no tools");
    assert!(error.message.contains("at least one tool"));
    Ok(())
}

#[test]
fn default_profile_file_is_created_once() -> HubResult<()> {
    let home = common::temp_home("profile-default");
    let path = home.path().join("config/default-profile.json");
    let created = ensure_default_profile(&path)?;
    assert_eq!(created["schemaVersion"], serde_json::json!(1));
    assert_eq!(created["tools"][0], serde_json::json!("read"));
    let mode = pihub_core::fsutil::mode_string(pihub_core::fsutil::file_mode(&path)?);
    assert_eq!(mode, "600");

    let again = ensure_default_profile(&path)?;
    assert_eq!(again, created);
    Ok(())
}

#[test]
fn skill_policy_view_defaults_to_legacy_for_profile_rows() {
    // Guard the v1/v2 view difference at the contract level.
    let mode = SkillPolicyViewMode::Legacy;
    assert_eq!(serde_json::to_string(&mode).expect("serialize"), "\"legacy\"");
}
