//! Carrier system-prompt injection.
//!
//! 2.1 only ever *appends* to a carrier's own instructions:
//! - `pi`     -> `--append-system-prompt <text>`
//! - `claude` -> `--append-system-prompt <text>`
//! - `codex`  -> `-c developer_instructions=<JSON string>` (one extra developer
//!   layer; base developer parts stay untouched)
//!
//! There is no replace channel, so a carrier keeps its native constraints.

use pihub_contracts::{Carrier, FrozenPromptConfig, PromptChannel, PromptMode};

pub const APPEND_SYSTEM_PROMPT_FLAG: &str = "--append-system-prompt";

/// Launch arguments for the injected prompt. Empty when nothing is injected.
///
/// Every carrier has an append-or-developer channel (`Carrier::prompt_channel`),
/// so injection is never rejected and there is no fallback branch: the Hub only
/// ever *appends*, it never replaces a carrier's own instructions.
pub fn prompt_launch_args(carrier: Carrier, frozen: Option<&FrozenPromptConfig>) -> Vec<String> {
    let Some(frozen) = frozen else {
        return Vec::new();
    };
    if frozen.mode != PromptMode::Append || frozen.prompt.is_empty() {
        return Vec::new();
    }
    match carrier.prompt_channel() {
        PromptChannel::Append => vec![
            APPEND_SYSTEM_PROMPT_FLAG.to_string(),
            frozen.prompt.clone(),
        ],
        // One argv element: Codex parses `-c` values as TOML, and a JSON string
        // is a valid TOML basic string, so newlines/quotes survive verbatim.
        PromptChannel::Developer => vec![
            "-c".to_string(),
            format!(
                "developer_instructions={}",
                serde_json::to_string(&frozen.prompt).expect("json string")
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::prompt_launch_args;
    use pihub_contracts::{Carrier, FrozenPromptConfig, PromptChannel, PromptMode};

    fn frozen(mode: PromptMode, prompt: &str) -> FrozenPromptConfig {
        FrozenPromptConfig {
            schema_version: 1,
            mode,
            carrier: Carrier::Pi,
            channel: None,
            support: None,
            principle_ids: Vec::new(),
            custom_text: String::new(),
            prompt: prompt.to_string(),
            digest: "sha256:0".to_string(),
            resolved_at: None,
        }
    }

    #[test]
    fn channels_come_from_the_contract() {
        assert_eq!(Carrier::Pi.prompt_channel(), PromptChannel::Append);
        assert_eq!(Carrier::Claude.prompt_channel(), PromptChannel::Append);
        assert_eq!(Carrier::Codex.prompt_channel(), PromptChannel::Developer);
    }

    #[test]
    fn nothing_is_injected_for_an_empty_or_absent_record() {
        assert!(prompt_launch_args(Carrier::Pi, None).is_empty());
        assert!(prompt_launch_args(Carrier::Codex, Some(&frozen(PromptMode::None, ""))).is_empty());
        assert!(prompt_launch_args(Carrier::Claude, Some(&frozen(PromptMode::Append, ""))).is_empty());
    }

    #[test]
    fn append_carriers_receive_one_literal_argv_value() {
        let record = frozen(PromptMode::Append, "line one\n\"quoted\" and 中文");
        let args = prompt_launch_args(Carrier::Claude, Some(&record));
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--append-system-prompt");
        assert_eq!(args[1], "line one\n\"quoted\" and 中文");
        assert!(!args[0].contains('\n'));
    }

    #[test]
    fn codex_receives_a_toml_safe_json_developer_instruction() {
        let record = frozen(PromptMode::Append, "a\nb \"c\"\n");
        let args = prompt_launch_args(Carrier::Codex, Some(&record));
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "developer_instructions=\"a\\nb \\\"c\\\"\\n\"");
    }
}
