//! Hub log: one bounded file family, private modes, safe protocol summaries,
//! delta filtering helpers and stderr line splitting.

mod common;

use std::sync::Arc;

use pihub_contracts::{Carrier, HubPaths, LogLevel, LogRecord, LogSink};
use pihub_core::logging::{resolve_hub_log_dir, AgentLog, HubLog, HubLogLimits, StderrLineSplitter};
use serde_json::Value;

fn hub_log(label: &str) -> (tempfile::TempDir, Arc<HubLog>, std::path::PathBuf) {
    let home = common::temp_home(label);
    let paths = HubPaths::resolve(home.path(), "/app-root");
    let dir = resolve_hub_log_dir(&paths);
    let log = Arc::new(
        HubLog::with_limits(
            &dir,
            HubLogLimits {
                max_entry_bytes: 2048,
                ..HubLogLimits::default()
            },
        )
        .expect("log"),
    );
    (home, log, dir)
}

#[tokio::test]
async fn log_writes_one_private_json_family_with_scoped_adapters() {
    let (_home, log, dir) = hub_log("log-basic");
    let adapter = AgentLog::new(log.clone(), "agent-a", Carrier::Claude, "rpc");
    adapter.protocol("in", &serde_json::json!({"id": "1", "type": "prompt", "message": "sk-live-TOPSECRET"}));
    adapter.protocol(
        "out",
        &serde_json::json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "hello world"}]}}),
    );
    adapter.note("started", Some(serde_json::json!({"pid": 42})), LogLevel::Info);
    adapter.stderr_line("first line", false);
    adapter.stderr_line("second line", false);
    log.close().await.expect("close");

    let file = dir.join("hub.log");
    assert_eq!(
        pihub_core::fsutil::mode_string(pihub_core::fsutil::file_mode(&file).expect("mode")),
        "600"
    );
    assert_eq!(
        pihub_core::fsutil::mode_string(pihub_core::fsutil::file_mode(&dir).expect("mode")),
        "700"
    );
    let text = std::fs::read_to_string(&file).expect("read log");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect();
    assert_eq!(lines.len(), 5);
    for line in &lines {
        assert_eq!(line["agentId"], serde_json::json!("agent-a"));
        assert_eq!(line["carrier"], serde_json::json!("claude"));
        assert_eq!(line["source"], serde_json::json!("rpc"));
        assert!(line["ts"].is_string());
        assert!(line["kind"].is_string());
    }
    let protocol: Vec<&Value> = lines
        .iter()
        .filter(|line| line["kind"] == serde_json::json!("protocol"))
        .collect();
    assert_eq!(protocol.len(), 2);
    let first_summary = &protocol[0]["summary"];
    let mut keys: Vec<String> = first_summary["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .map(|value| value.as_str().expect("key").to_string())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["id".to_string(), "message".to_string(), "type".to_string()]);

    // No message text or credential may reach the diagnostic log.
    assert!(!text.contains("TOPSECRET"));
    assert!(!text.contains("hello world"));
    let stderr_lines = lines
        .iter()
        .filter(|line| line["kind"] == serde_json::json!("stderr"))
        .count();
    assert_eq!(stderr_lines, 2);
    assert!(log.stats()["written"].as_u64().expect("written") >= 5);
}

#[tokio::test]
async fn oversized_entries_are_capped_and_marked_truncated() {
    let home = common::temp_home("log-cap");
    let paths = HubPaths::resolve(home.path(), "/app-root");
    let dir = resolve_hub_log_dir(&paths);
    let log = HubLog::with_limits(
        &dir,
        HubLogLimits {
            max_entry_bytes: 512,
            ..HubLogLimits::default()
        },
    )
    .expect("log");
    log.record(LogRecord {
        kind: Some("protocol_raw".to_string()),
        level: Some(LogLevel::Debug),
        message: Some("x".repeat(4000)),
        data: Some(serde_json::json!({"marker": "RAW-MARKER", "blob": "y".repeat(4000)})),
        ..LogRecord::default()
    });
    log.close().await.expect("close");

    let text = std::fs::read_to_string(dir.join("hub.log")).expect("read");
    assert!(text.len() <= 513, "entry must respect the byte cap");
    let line: Value = serde_json::from_str(text.trim()).expect("json");
    assert_eq!(line["truncated"], serde_json::json!(true));
    assert!(line["originalBytes"].as_u64().expect("bytes") > 4000);
}

#[tokio::test]
async fn rotation_keeps_the_family_inside_its_limits() {
    let home = common::temp_home("log-rotate");
    let paths = HubPaths::resolve(home.path(), "/app-root");
    let dir = resolve_hub_log_dir(&paths);
    let limits = HubLogLimits {
        max_segment_bytes: 1024,
        max_rotated_files: 2,
        max_entry_bytes: 512,
        max_string_chars: 256,
        ..HubLogLimits::default()
    };
    assert_eq!(limits.total_capacity_bytes(), 1024 * 3);
    let log = HubLog::with_limits(&dir, limits).expect("log");
    for index in 0..80 {
        log.record(LogRecord {
            kind: Some("agent_note".to_string()),
            message: Some(format!("entry-{index}-{}", "y".repeat(180))),
            ..LogRecord::default()
        });
    }
    log.close().await.expect("close");

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(names.contains(&"hub.log".to_string()));
    let rotated: Vec<String> = names
        .iter()
        .filter(|name| **name != "hub.log")
        .cloned()
        .collect();
    assert!(
        rotated.len() <= 2,
        "retained rotated files must stay bounded: {rotated:?}"
    );
    assert!(
        !names.iter().any(|name| name.ends_with(".gz")),
        "rotation must not compress"
    );
    let rotated_bytes: u64 = rotated
        .iter()
        .map(|name| {
            std::fs::metadata(dir.join(name))
                .expect("metadata")
                .len()
        })
        .sum();
    assert!(rotated_bytes <= 2 * 1024 + 1024, "rotated bytes {rotated_bytes}");
    for name in &names {
        assert_eq!(
            pihub_core::fsutil::mode_string(
                pihub_core::fsutil::file_mode(&dir.join(name)).expect("mode")
            ),
            "600",
            "{name} must be private"
        );
    }
}

#[tokio::test]
async fn stderr_splitter_handles_partial_lines_and_caps_runaway_input() {
    let mut splitter = StderrLineSplitter::new(64);
    assert!(splitter.push("first li").is_empty());
    let lines = splitter.push("ne\nsecond\n");
    assert_eq!(
        lines,
        vec![
            ("first line".to_string(), false),
            ("second".to_string(), false)
        ]
    );
    assert!(splitter.push("tail-without-newline").is_empty());
    let capped = splitter.push(&"b".repeat(200));
    assert_eq!(capped.len(), 1);
    assert!(capped[0].1, "oversized partial line must be flagged");
    // Same shape as the 2.1 LineSink: capped text plus a truncation marker.
    assert!(capped[0].0.starts_with("tail-without-newlineb"));
    assert!(capped[0].0.contains("[truncated"));
    assert!(splitter.finish().is_none());

    let mut trailing = StderrLineSplitter::new(64);
    trailing.push("unterminated\r");
    assert_eq!(trailing.finish(), Some(("unterminated".to_string(), false)));
}

#[test]
fn protocol_summaries_never_copy_message_text() {
    let summary = pihub_core::logging::summarize_protocol(
        "in",
        &serde_json::json!({
            "type": "prompt",
            "message": {"role": "user", "content": [{"type": "text", "text": "secret-text"}]},
            "assistantMessageEvent": {"type": "text_delta", "delta": "hidden"},
        }),
        512,
    );
    let serialized = serde_json::to_string(&summary).expect("serialize");
    assert!(!serialized.contains("secret-text"));
    assert!(!serialized.contains("hidden"));
    assert_eq!(summary["message"]["role"], serde_json::json!("user"));
    assert_eq!(summary["message"]["contentBlocks"], serde_json::json!(1));
    assert_eq!(summary["deltaChars"], serde_json::json!(6));
    assert_eq!(summary["deltaType"], serde_json::json!("text_delta"));
}

#[test]
fn log_limits_reject_unsafe_file_names() {
    let home = common::temp_home("log-limits");
    let paths = HubPaths::resolve(home.path(), "/app-root");
    let dir = resolve_hub_log_dir(&paths);
    let error = HubLog::with_limits(
        &dir,
        HubLogLimits {
            file_name: "../escape.log".to_string(),
            ..HubLogLimits::default()
        },
    )
    .expect_err("unsafe file name");
    assert_eq!(error.code, "invalid_log_limits");

    let error = HubLog::with_limits(
        &dir,
        HubLogLimits {
            max_rotated_files: 0,
            ..HubLogLimits::default()
        },
    )
    .expect_err("zero retained files");
    assert_eq!(error.code, "invalid_log_limits");
}
