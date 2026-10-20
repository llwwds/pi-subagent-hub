//! System-prompt resolution and freezing (`src/prompts.js`).
//!
//! Additive injection only: the Hub never replaces a carrier's own system
//! constraints. Everything here is pure: no I/O, no async, no environment.

use pihub_contracts::{
    canonical_json, sha256_prefixed, utf16_len, Carrier, FrozenPromptConfig, HubError, HubResult,
    PromptCatalog, PromptConfigInput, PromptMode, PromptPrinciple, PromptService, PromptSummary,
};
use serde_json::json;

use crate::fsutil;

pub const MAX_CUSTOM_TEXT: usize = 8_000;
pub const MAX_PROMPT_TEXT: usize = 20_000;

/// (id, title, text) in declaration order; the order is also the canonical
/// selection order used by the digest.
pub const PRINCIPLES: [(&str, &str, &str); 4] = [
    (
        "first-principles",
        "第一性原理",
        "先回到问题的基本事实和约束，从最根本的原理推导方案；不要因为现有实现或惯例如此就默认接受。",
    ),
    (
        "high-cohesion-low-coupling",
        "高内聚低耦合",
        "让相关逻辑集中在同一处，模块之间只通过最小、明确、稳定的接口协作；避免隐式共享状态和跨层依赖。",
    ),
    (
        "behavior-preserving-refactor",
        "保证功能完全不变前提下的局部重构",
        "可以在外部行为、接口和可观察结果完全不变的前提下做局部重构；重构与功能变更分开，并给出行为未变的验证。",
    ),
    (
        "no-unnecessary-entities",
        "如无必要勿增实体",
        "不为假设中的未来需求增加抽象、配置、依赖或中间层；现有结构能解决就不新建。",
    ),
];

const HEADER: &str = "以下内容由 pi-subagent-hub 追加在载体原有系统提示末尾，只做追加，不替换也不覆盖载体自带的系统约束；如与原有约束冲突，以载体原有约束为准。";

pub fn default_principle_ids() -> Vec<String> {
    PRINCIPLES
        .iter()
        .map(|(id, _, _)| (*id).to_string())
        .collect()
}

fn invalid(message: impl Into<String>) -> HubError {
    HubError::new("invalid_prompt_config", message)
}

/// CRLF/CR folding, control-character stripping, JS trimming, UTF-16 limit.
pub fn normalize_custom_text(value: Option<&str>) -> HubResult<String> {
    let Some(value) = value else {
        return Ok(String::new());
    };
    let mut folded = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            folded.push('\n');
            continue;
        }
        let code = character as u32;
        let stripped = code <= 0x08
            || code == 0x0b
            || code == 0x0c
            || (0x0e..=0x1f).contains(&code)
            || code == 0x7f;
        if stripped {
            continue;
        }
        folded.push(character);
    }
    let trimmed = fsutil::js_trim(&folded).to_string();
    if utf16_len(&trimmed) > MAX_CUSTOM_TEXT {
        return Err(invalid(format!(
            "promptConfig.customText must be at most {MAX_CUSTOM_TEXT} characters"
        )));
    }
    Ok(trimmed)
}

fn normalize_principle_ids(value: Option<&Vec<String>>) -> HubResult<Vec<String>> {
    let Some(value) = value else {
        return Ok(default_principle_ids());
    };
    let mut selected: Vec<String> = Vec::new();
    for entry in value {
        let id = fsutil::js_trim(entry).to_string();
        if !PRINCIPLES.iter().any(|(known, _, _)| *known == id) {
            let known: Vec<&str> = PRINCIPLES.iter().map(|(id, _, _)| *id).collect();
            return Err(invalid(format!(
                "unknown prompt principle: {entry:?}; known ids: {}",
                known.join(", ")
            )));
        }
        if !selected.iter().any(|existing| existing == &id) {
            selected.push(id);
        }
    }
    // Canonical order keeps the digest stable regardless of UI ordering.
    Ok(PRINCIPLES
        .iter()
        .map(|(id, _, _)| (*id).to_string())
        .filter(|id| selected.iter().any(|selected| selected == id))
        .collect())
}

fn build_prompt_text(principle_ids: &[String], custom_text: &str) -> String {
    let mut blocks: Vec<String> = Vec::new();
    if !principle_ids.is_empty() {
        let lines: Vec<String> = principle_ids
            .iter()
            .filter_map(|id| {
                PRINCIPLES
                    .iter()
                    .find(|(known, _, _)| known == id)
                    .map(|(_, title, text)| format!("- {title}：{text}"))
            })
            .collect();
        blocks.push(format!("### 工作原则\n{}", lines.join("\n")));
    }
    if !custom_text.is_empty() {
        blocks.push(format!("### 用户追加要求\n{custom_text}"));
    }
    if blocks.is_empty() {
        return String::new();
    }
    format!("{HEADER}\n\n{}", blocks.join("\n\n"))
}

fn prompt_digest(
    mode: PromptMode,
    carrier: Carrier,
    principle_ids: &[String],
    custom_text: &str,
    prompt: &str,
) -> String {
    let mode = match mode {
        PromptMode::Append => "append",
        PromptMode::None => "none",
    };
    let document = json!({
        "schemaVersion": 1,
        "mode": mode,
        "carrier": carrier.as_str(),
        "principleIds": principle_ids,
        "customText": custom_text,
        "prompt": prompt,
    });
    sha256_prefixed(canonical_json(&document))
}

/// Port of `resolvePromptConfig`, including the "explicit selection" rule:
/// `principleIds` or a non-empty `customText` replaces the four-principle
/// default, while an empty config means "no injection".
pub fn resolve_prompt_config(
    input: Option<&PromptConfigInput>,
    carrier: Carrier,
    resolved_at: Option<String>,
) -> HubResult<FrozenPromptConfig> {
    let custom_text = normalize_custom_text(input.and_then(|input| input.custom_text.as_deref()))?;
    let explicit_selection =
        input.is_some_and(|input| input.principle_ids.is_some()) || !custom_text.is_empty();
    let principle_ids = if explicit_selection {
        let requested = input.and_then(|input| input.principle_ids.as_ref());
        match requested {
            Some(ids) => normalize_principle_ids(Some(ids))?,
            None => Vec::new(),
        }
    } else {
        default_principle_ids()
    };
    let prompt = build_prompt_text(&principle_ids, &custom_text);
    if utf16_len(&prompt) > MAX_PROMPT_TEXT {
        return Err(invalid(format!(
            "resolved system prompt must be at most {MAX_PROMPT_TEXT} characters"
        )));
    }
    let mode = if prompt.is_empty() {
        PromptMode::None
    } else {
        PromptMode::Append
    };
    let support = carrier.prompt_channel();
    let digest = prompt_digest(mode, carrier, &principle_ids, &custom_text, &prompt);
    Ok(FrozenPromptConfig {
        schema_version: 1,
        mode,
        carrier,
        channel: Some(support),
        support: Some(support),
        principle_ids,
        custom_text,
        prompt,
        digest,
        resolved_at,
    })
}

/// Parse a stored column/file value; empty or malformed input is `None`.
pub fn read_frozen_prompt_config(raw: Option<&str>) -> Option<FrozenPromptConfig> {
    let text = raw?;
    if text.trim().is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if !value.is_object() || value.as_object().is_some_and(|map| map.is_empty()) {
        return None;
    }
    let frozen: FrozenPromptConfig = serde_json::from_value(value).ok()?;
    if frozen.schema_version != 1 || frozen.digest.is_empty() {
        return None;
    }
    Some(frozen)
}

/// Public, non-secret view; text is opt-in because it can be long.
pub fn prompt_summary(frozen: &FrozenPromptConfig, include_text: bool) -> PromptSummary {
    let support = frozen.support.or(Some(frozen.carrier.prompt_channel()));
    let mut summary = PromptSummary {
        mode: frozen.mode,
        carrier: Some(frozen.carrier),
        channel: frozen.channel.or(support),
        support,
        principle_ids: frozen.principle_ids.clone(),
        custom_text_chars: utf16_len(&frozen.custom_text),
        prompt_chars: utf16_len(&frozen.prompt),
        digest: frozen.digest.clone(),
        resolved_at: frozen.resolved_at.clone(),
        custom_text: None,
        prompt: None,
    };
    if include_text {
        summary.custom_text = Some(frozen.custom_text.clone());
        summary.prompt = Some(frozen.prompt.clone());
    }
    summary
}

/// The pure default implementation used by the manager.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultPromptService;

impl PromptService for DefaultPromptService {
    fn list(&self) -> PromptCatalog {
        PromptCatalog {
            principles: PRINCIPLES
                .iter()
                .map(|(id, title, text)| PromptPrinciple {
                    id: (*id).to_string(),
                    title: (*title).to_string(),
                    text: (*text).to_string(),
                })
                .collect(),
            default_principle_ids: default_principle_ids(),
        }
    }

    fn resolve(
        &self,
        input: Option<&PromptConfigInput>,
        carrier: Carrier,
        resolved_at: Option<String>,
    ) -> HubResult<FrozenPromptConfig> {
        resolve_prompt_config(input, carrier, resolved_at)
    }

    fn read_frozen(&self, raw: Option<&str>) -> Option<FrozenPromptConfig> {
        read_frozen_prompt_config(raw)
    }

    fn summary(&self, frozen: &FrozenPromptConfig, include_text: bool) -> PromptSummary {
        prompt_summary(frozen, include_text)
    }
}
