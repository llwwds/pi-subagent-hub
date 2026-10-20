//! CLI entry point.
//!
//! `run_cli` is the only function the bin assembly calls: it decides the
//! output channel (`--json` vs human), maps `HubError::exit_code`, and keeps
//! handled failures off stderr in machine mode.

pub mod args;
pub mod commands;
pub mod help;
pub mod manifest;
pub mod spec;

use std::io::Read;

use pihub_contracts::{HubError, HubPaths, LocalOps, APP_VERSION};
use serde_json::{json, Value};

use crate::error;
use commands::Outcome;

/// Read all of stdin and drop one trailing newline (`readStdin`).
pub fn read_stdin() -> Result<String, HubError> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|cause| error::command_failed(format!("cannot read stdin: {cause}")))?;
    Ok(buffer.strip_suffix('\n').unwrap_or(&buffer).to_string())
}

/// Run the CLI and return the process exit code.
pub async fn run_cli(args: Vec<String>, local: &dyn LocalOps, paths: &HubPaths) -> i32 {
    let json_mode = args::wants_json(&args);

    if args.is_empty() || args.iter().any(|value| value == "--help" || value == "-h") {
        println!("{}", help::help());
        return 0;
    }
    if matches!(args.first().map(String::as_str), Some("--version" | "-v")) {
        println!("{APP_VERSION}");
        return 0;
    }

    match dispatch(&args, local, paths).await {
        Ok(outcome) => {
            emit(outcome, json_mode);
            0
        }
        Err(failure) => {
            if json_mode {
                println!("{}", failure_envelope(&failure));
            } else {
                eprintln!("pihub: {}", failure.message);
            }
            failure.exit_code.unwrap_or(1)
        }
    }
}

async fn dispatch(
    args: &[String],
    local: &dyn LocalOps,
    paths: &HubPaths,
) -> Result<Outcome, HubError> {
    let command = args[0].as_str();
    let subcommand = args.get(1).map(String::as_str);
    let skip = if args::SUBCOMMAND_COMMANDS.contains(&command) {
        2.min(args.len())
    } else {
        1.min(args.len())
    };
    let parsed = args::parse_args(&args[skip..])?;
    commands::dispatch(command, subcommand, &parsed, local, paths).await
}

fn emit(outcome: Outcome, json_mode: bool) {
    match outcome {
        Outcome::Text(text) => println!("{text}"),
        Outcome::Value(value) => {
            if json_mode {
                println!("{}", success_envelope(&value));
            } else if let Value::String(text) = &value {
                println!("{text}");
            } else {
                match serde_json::to_string_pretty(&value) {
                    Ok(pretty) => println!("{pretty}"),
                    Err(_) => println!("{value}"),
                }
            }
        }
    }
}

/// `{"ok":true,"data":...,"error":null}` on one line.
pub fn success_envelope(data: &Value) -> String {
    serde_json::to_string(&json!({
        "ok": true,
        "data": data,
        "error": null,
    }))
    .unwrap_or_else(|_| "{\"ok\":true,\"data\":null,\"error\":null}".to_string())
}

/// `{"ok":false,"data":null,"error":{code,message,retryable:false,details:null}}`.
pub fn failure_envelope(failure: &HubError) -> String {
    serde_json::to_string(&json!({
        "ok": false,
        "data": null,
        "error": {
            "code": failure.code,
            "message": failure.message,
            "retryable": false,
            "details": failure.details,
        },
    }))
    .unwrap_or_else(|_| {
        "{\"ok\":false,\"data\":null,\"error\":{\"code\":\"command_failed\",\"message\":\"\",\"retryable\":false,\"details\":null}}"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_envelope_is_single_line() {
        let encoded = success_envelope(&json!({ "a": 1 }));
        assert!(!encoded.contains('\n'));
        let parsed: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["a"], 1);
        assert!(parsed["error"].is_null());
    }

    #[test]
    fn failure_envelope_matches_the_cli_contract() {
        let encoded = failure_envelope(&error::command_failed("unknown command: not-a-command"));
        let parsed: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            parsed,
            json!({
                "ok": false,
                "data": null,
                "error": {
                    "code": "command_failed",
                    "message": "unknown command: not-a-command",
                    "retryable": false,
                    "details": null,
                },
            })
        );
    }
}
