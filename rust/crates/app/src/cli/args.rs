//! 2.1-compatible argument scanning.
//!
//! `src/cli.js:parseArgs` accepts any `--name value` pair and a small set of
//! bare boolean flags; unknown flags are ignored rather than rejected, and only
//! a trailing flag without a value is an error. Reproducing that exactly keeps
//! the CLI tolerant of the same inputs, which a strict parser would reject.

use std::collections::BTreeMap;

use pihub_contracts::{HubError, HubResult};

use crate::error;

/// Flags that never consume the following argument.
pub const BOOLEAN_FLAGS: [&str; 8] = [
    "json",
    "stdin",
    "no-open",
    "all",
    "live",
    "force",
    "purge-snapshot",
    "system-prompt-stdin",
];

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    options: BTreeMap<String, Option<String>>,
    pub positional: Vec<String>,
}

impl Parsed {
    pub fn is_flag(&self, key: &str) -> bool {
        matches!(self.options.get(key), Some(_))
    }

    pub fn is_bool_set(&self, key: &str) -> bool {
        matches!(self.options.get(key), Some(None) | Some(Some(_)))
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.options.get(key).and_then(|value| value.as_deref())
    }

    pub fn has_value(&self, key: &str) -> bool {
        self.value(key).is_some()
    }

    pub fn json_mode(&self) -> bool {
        self.is_bool_set("json")
    }

    /// Split a comma separated option into trimmed, non-empty values.
    pub fn csv(&self, key: &str) -> Option<Vec<String>> {
        self.value(key).map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
    }
}

pub fn parse_args(args: &[String]) -> HubResult<Parsed> {
    let mut options: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut positional = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let value = &args[index];
        let Some(key) = value.strip_prefix("--") else {
            positional.push(value.clone());
            index += 1;
            continue;
        };
        if BOOLEAN_FLAGS.contains(&key) {
            options.insert(key.to_string(), None);
            index += 1;
            continue;
        }
        let Some(next) = args.get(index + 1) else {
            return Err(error::command_failed(format!("missing value for {value}")));
        };
        options.insert(key.to_string(), Some(next.clone()));
        index += 2;
    }
    Ok(Parsed { options, positional })
}

/// True when `--json` appears anywhere in the raw argv (used to decide the
/// failure channel before parsing can fail).
pub fn wants_json(args: &[String]) -> bool {
    args.iter().any(|value| value == "--json")
}

/// Commands whose second token is a subcommand (`daemon start`, ...).
pub const SUBCOMMAND_COMMANDS: [&str; 7] = [
    "daemon", "config", "carriers", "providers", "skills", "agents", "prompts",
];

/// Reject a `HubError` carrying an unexpected code from an argument reader.
pub fn bool_value(parsed: &Parsed, key: &str) -> bool {
    parsed.is_bool_set(key)
}

pub fn require<'a>(parsed: &'a Parsed, key: &str, message: &str) -> HubResult<&'a str> {
    parsed
        .value(key)
        .ok_or_else(|| HubError::new(error::CODE_COMMAND_FAILED, message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> HubResult<Parsed> {
        parse_args(&values.iter().map(|value| value.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn boolean_flags_do_not_consume_values() {
        let parsed = parse(&["list", "--json", "--all"]).unwrap();
        assert!(parsed.json_mode());
        assert!(parsed.is_bool_set("all"));
        assert_eq!(parsed.positional, vec!["list"]);
    }

    #[test]
    fn valued_flags_consume_the_next_token() {
        let parsed = parse(&["spawn", "--name", "reviewer", "--model", "gw/model-a"]).unwrap();
        assert_eq!(parsed.value("name"), Some("reviewer"));
        assert_eq!(parsed.value("model"), Some("gw/model-a"));
    }

    #[test]
    fn trailing_flag_without_value_is_an_error() {
        let failure = parse(&["spawn", "--name"]).unwrap_err();
        assert_eq!(failure.code, "command_failed");
        assert_eq!(failure.message, "missing value for --name");
    }

    #[test]
    fn unknown_flags_are_accepted_like_2_1() {
        let parsed = parse(&["list", "--bogus", "1", "--json"]).unwrap();
        assert_eq!(parsed.value("bogus"), Some("1"));
        assert!(parsed.json_mode());
    }

    #[test]
    fn csv_trims_and_drops_empty_entries() {
        let parsed = parse(&["agents", "create", "--skills", "a, b ,,"]).unwrap();
        assert_eq!(parsed.csv("skills").unwrap(), vec!["a", "b"]);
    }
}
