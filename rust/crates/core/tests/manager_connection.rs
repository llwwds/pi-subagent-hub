//! Skills Manager connection: read-only CLI protocol, atomic connection,
//! snapshot, live drift, failure-keeps-snapshot and disconnect semantics.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pihub_contracts::{
    ManagerConnectInput, ManagerConnection, ManagerDisconnectInput, ManagerSelection,
    ManagerSyncInput, SkillPolicy,
};
use pihub_core::skills::manager_connection::{list_manager_skills, FsManagerConnection};
use serde_json::{json, Value};

/// Fixture CLI: serves the manager JSON contract from plain files and logs calls.
const FIXTURE_CLI: &str = r#"#!/bin/sh
root="$FIXTURE_MANAGER"
printf '%s\n' "$*" >> "$root/calls.log"
if [ "$1" = "--version" ]; then
  echo "skills-manager-cli 1.41.0"
  exit 0
fi
if [ -n "$FIXTURE_FAIL" ] && [ "$FIXTURE_FAIL" = "$1" ]; then
  echo "fixture failure for $1" >&2
  exit 1
fi
case "$1 $2" in
  "repo status") cat "$root/repo.json" ;;
  "presets list") cat "$root/presets.json" ;;
  "skills list") cat "$root/skills.json" ;;
  "agents list") cat "$root/agents.json" ;;
  "skills show") cat "$root/show-$3.json" 2>/dev/null || { echo "no SKILL.md for $3" >&2; exit 1; } ;;
  *) echo "unsupported fixture command: $*" >&2; exit 1 ;;
esac
"#;

struct Fixture {
    home: tempfile::TempDir,
    skills: PathBuf,
    bin: PathBuf,
    cli: PathBuf,
    connection: FsManagerConnection,
}

fn write_catalog(fixture: &Fixture, names: &[&str]) {
    let skills: Vec<Value> = names
        .iter()
        .map(|name| {
            json!({
                "id": format!("id-{name}"),
                "name": name,
                "description": format!("{name} from manager"),
                "path": fixture.skills.join(name),
                "enabled": true,
                "tags": ["common"],
                "presets": ["每日常用"],
                "preset_ids": ["p1"],
                "deployed_to": ["claude_code"],
                "source_type": "local",
            })
        })
        .collect();
    common::write_file(
        &fixture.home.path().join("skills.json"),
        &serde_json::to_string(&skills).expect("skills json"),
    );
}

fn setup(label: &str, names: &[&str]) -> Fixture {
    let home = common::temp_home(label);
    let manager = home.path().join("manager");
    let skills = manager.join("skills");
    common::mkdir(&skills);
    let bin = home.path().join("bin");
    common::mkdir(&bin);
    let cli = bin.join("skills-manager-cli");
    common::write_file_mode(&cli, FIXTURE_CLI, 0o755);
    for name in names {
        common::write_skill_package(&skills, name, &format!("{name} body"));
    }
    common::write_file(
        &home.path().join("repo.json"),
        &json!({
            "base_dir": manager,
            "skills_dir": skills,
            "db_path": manager.join("skills-manager.db"),
            "active_preset_id": "p1",
        })
        .to_string(),
    );
    common::write_file(
        &home.path().join("presets.json"),
        &json!([{"id": "p1", "name": "每日常用", "skill_count": names.len(), "active": true}]).to_string(),
    );
    common::write_file(&home.path().join("agents.json"), &json!([{"key": "pihub"}]).to_string());

    let mut fixture = Fixture {
        skills,
        bin,
        cli,
        connection: FsManagerConnection::new(home.path()).expect("placeholder connection"),
        home,
    };
    write_catalog(&fixture, names);
    fixture.connection = connection_for(&fixture, None);
    fixture
}

fn env_for(fixture: &Fixture, extra: Option<(&str, &str)>) -> BTreeMap<String, OsString> {
    let mut env = BTreeMap::new();
    env.insert(
        "HOME".to_string(),
        OsString::from(fixture.home.path().to_string_lossy().into_owned()),
    );
    env.insert(
        "PATH".to_string(),
        OsString::from(format!(
            "{}:{}",
            fixture.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )),
    );
    env.insert(
        "FIXTURE_MANAGER".to_string(),
        OsString::from(fixture.home.path().to_string_lossy().into_owned()),
    );
    if let Some((key, value)) = extra {
        env.insert(key.to_string(), OsString::from(value));
    }
    env
}

fn connection_for(fixture: &Fixture, fail: Option<(&str, &str)>) -> FsManagerConnection {
    FsManagerConnection::with_options(
        fixture.home.path(),
        Some(fixture.cli.clone()),
        Some(env_for(fixture, fail)),
        Some("darwin".to_string()),
        Some(Duration::from_secs(20)),
    )
    .expect("manager connection")
}

fn connect_input() -> ManagerConnectInput {
    ManagerConnectInput {
        selection: ManagerSelection {
            ids: Vec::new(),
            preset_ids: Vec::new(),
            tags: Vec::new(),
        },
        ..ManagerConnectInput::default()
    }
}

#[tokio::test]
async fn connect_records_a_snapshot_and_status_reports_no_drift() {
    let fixture = setup("mc-connect", &["alpha", "beta"]);
    let connected = fixture
        .connection
        .connect(connect_input())
        .await
        .expect("connect");
    assert_eq!(connected["connected"], json!(true));
    assert_eq!(connected["snapshot"]["skillCount"], json!(2));
    assert_eq!(
        connected["snapshot"]["selectedIds"],
        json!(["alpha", "beta"]),
        "selected ids are id-sorted"
    );
    assert_eq!(connected["snapshot"]["stale"], json!(false));
    assert_eq!(
        connected["config"]["cliPath"],
        json!(std::fs::canonicalize(&fixture.cli)
            .expect("canonical cli")
            .to_string_lossy())
    );
    assert_eq!(connected["config"]["managerVersion"], json!("1.41.0"));
    assert_eq!(
        connected["config"]["selection"],
        json!({"presetIds": [], "tags": [], "ids": []})
    );

    // The two PiHub-side records are private files.
    for name in ["manager-connection.json", "manager-catalog.json"] {
        let path = fixture.home.path().join("skills").join(name);
        assert_eq!(
            pihub_core::fsutil::mode_string(pihub_core::fsutil::file_mode(&path).expect("mode")),
            "600"
        );
    }

    let status = fixture.connection.status(false).await.expect("status");
    assert_eq!(status["connected"], json!(true));
    assert_eq!(status["snapshot"]["skillCount"], json!(2));
    assert!(status["snapshot"]["syncedAt"].is_string());
    assert_eq!(status["snapshot"]["missingSources"], json!([]));
    assert_eq!(status["source"]["skillsDirOk"], json!(true));

    let live = fixture.connection.status(true).await.expect("live status");
    assert_eq!(live["drift"]["changed"], json!(false));
    assert_eq!(live["drift"]["added"], json!([]));
    assert_eq!(live["drift"]["removed"], json!([]));
    assert_eq!(live["manager"]["available"], json!(true));

    let skills = list_manager_skills(&fixture.connection, None).expect("manager skills");
    assert_eq!(skills.len(), 2);
    assert_eq!(skills[0]["id"], json!("alpha"));

    // The CLI is only ever asked read-only questions.
    let calls = std::fs::read_to_string(fixture.home.path().join("calls.log")).expect("calls");
    for line in calls.lines() {
        let command: Vec<&str> = line.split_whitespace().collect();
        let head = command.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        assert!(
            [
                "--version",
                "repo status",
                "presets list",
                "skills list",
                "skills show",
                "agents list",
            ]
            .contains(&head.as_str()),
            "unexpected manager command: {line}"
        );
    }
}

#[tokio::test]
async fn live_status_reports_additions_and_missing_sources() {
    let fixture = setup("mc-drift", &["alpha", "beta"]);
    fixture
        .connection
        .connect(connect_input())
        .await
        .expect("connect");

    // A new library skill appears: reported as a drift addition, not applied.
    common::write_skill_package(&fixture.skills, "gamma", "gamma body");
    write_catalog(&fixture, &["alpha", "beta", "gamma"]);
    let drifted = fixture.connection.status(true).await.expect("status");
    assert_eq!(drifted["drift"]["added"], json!(["gamma"]));
    assert_eq!(drifted["drift"]["changed"], json!(true));
    assert_eq!(drifted["snapshot"]["skillCount"], json!(2), "snapshot unchanged");

    // A frozen source directory disappears: reported without rewriting the snapshot.
    std::fs::remove_dir_all(fixture.skills.join("beta")).expect("remove beta");
    let missing = fixture.connection.status(false).await.expect("status");
    assert_eq!(missing["snapshot"]["missingSources"], json!(["beta"]));
    assert_eq!(missing["snapshot"]["stale"], json!(true));
    assert!(missing["snapshot"]["staleReason"]
        .as_str()
        .expect("reason")
        .contains("missing"));
}

#[tokio::test]
async fn failed_sync_keeps_the_previous_complete_snapshot() {
    let fixture = setup("mc-sync-fail", &["alpha", "beta"]);
    fixture
        .connection
        .connect(connect_input())
        .await
        .expect("connect");
    let snapshot_path = fixture.home.path().join("skills/manager-catalog.json");
    let before = common::read_json(&snapshot_path);

    let broken = connection_for(&fixture, Some(("FIXTURE_FAIL", "repo")));
    let result = broken
        .sync(ManagerSyncInput::default())
        .await
        .expect("sync result");
    assert_eq!(result["synced"], json!(false));
    assert_eq!(result["error"]["code"], json!("manager_query_failed"));
    assert_eq!(result["keptSnapshot"]["skillCount"], json!(2));
    assert_eq!(common::read_json(&snapshot_path), before, "snapshot untouched");

    // The healthy connection can still sync, and reports the delta.
    common::write_skill_package(&fixture.skills, "gamma", "gamma body");
    write_catalog(&fixture, &["alpha", "beta", "gamma"]);
    let synced = fixture
        .connection
        .sync(ManagerSyncInput::default())
        .await
        .expect("sync");
    assert_eq!(synced["synced"], json!(true));
    assert_eq!(synced["changed"], json!(true));
    assert_eq!(synced["added"], json!(["gamma"]));
    assert_eq!(synced["removed"], json!([]));

    // An identical catalog is a no-op unless forced.
    let again = fixture
        .connection
        .sync(ManagerSyncInput::default())
        .await
        .expect("sync again");
    assert_eq!(again["changed"], json!(false));
    let forced = fixture
        .connection
        .sync(ManagerSyncInput { force: true })
        .await
        .expect("forced sync");
    assert_eq!(forced["changed"], json!(true));
    assert_eq!(forced["added"], json!([]));
    assert_eq!(forced["removed"], json!([]));
}

#[tokio::test]
async fn resolve_sources_uses_canonical_library_paths_and_rejects_symlinks() {
    let fixture = setup("mc-sources", &["alpha", "beta"]);
    fixture
        .connection
        .connect(connect_input())
        .await
        .expect("connect");

    let all = fixture
        .connection
        .resolve_sources(None)
        .expect("all sources");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, "alpha");
    let canonical_root = std::fs::canonicalize(&fixture.skills).expect("canonical root");
    assert_eq!(all[0].source_path, canonical_root.join("alpha"));

    let only = fixture
        .connection
        .resolve_sources(Some(&SkillPolicy::Only {
            ids: vec!["beta".to_string()],
        }))
        .expect("only beta");
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].name, "beta");

    let error = fixture
        .connection
        .resolve_sources(Some(&SkillPolicy::Only {
            ids: vec!["missing".to_string()],
        }))
        .expect_err("unknown skill");
    assert_eq!(error.code, "skill_not_found");

    // A symlink inside the library is refused, even though it resolves outside.
    let outside = fixture.home.path().join("outside-evil");
    common::write_skill_package(&fixture.home.path().join("outside"), "evil", "evil body");
    let outside = outside.join("evil");
    common::mkdir(&outside);
    common::write_file(&outside.join("SKILL.md"), "---\nname: evil\n---\n");
    symlink(&outside, fixture.skills.join("evil")).expect("symlink");
    write_catalog(&fixture, &["alpha", "beta", "evil"]);

    let synced = fixture
        .connection
        .sync(ManagerSyncInput { force: true })
        .await
        .expect("sync");
    let invalid = synced["snapshot"]["invalidSources"]
        .as_array()
        .expect("invalid list");
    assert!(invalid
        .iter()
        .any(|entry| entry["id"] == json!("evil")
            && entry["code"] == json!("unsafe_manager_skill_path")));

    let error = fixture
        .connection
        .resolve_sources(Some(&SkillPolicy::Only {
            ids: vec!["evil".to_string()],
        }))
        .expect_err("symlinked skill");
    assert_eq!(error.code, "unsafe_manager_skill_path");
}

#[tokio::test]
async fn nested_entries_use_the_cli_skill_file_authoritatively() {
    let fixture = setup("mc-nested", &["alpha"]);
    // A manager-recognised nested layout: the entry directory has no SKILL.md.
    let nested_dir = fixture.skills.join("nested/real");
    common::mkdir(&nested_dir);
    common::write_file(
        &nested_dir.join("SKILL.md"),
        "---\nname: nested\n---\nnested body\n",
    );
    common::write_file(
        &fixture
            .home
            .path()
            .join("show-nested.json"),
        &json!({"name": "nested", "skill_file": nested_dir.join("SKILL.md"), "files": ["SKILL.md"]})
            .to_string(),
    );
    let entry_dir = fixture.skills.join("nested-entry");
    common::mkdir(&entry_dir);
    common::write_file(
        &fixture.home.path().join("skills.json"),
        &json!([{
            "id": "id-nested",
            "name": "nested",
            "description": "nested",
            "path": entry_dir,
            "enabled": true,
            "tags": [],
            "presets": [],
            "preset_ids": [],
            "deployed_to": [],
            "source_type": "local",
        }])
        .to_string(),
    );

    fixture
        .connection
        .connect(connect_input())
        .await
        .expect("connect");
    let sources = fixture.connection.resolve_sources(None).expect("sources");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id, "nested");
    assert_eq!(
        sources[0].source_path,
        std::fs::canonicalize(&nested_dir).expect("canonical nested")
    );

    let calls = std::fs::read_to_string(fixture.home.path().join("calls.log")).expect("calls");
    assert!(calls.contains("skills show nested"));
}

#[tokio::test]
async fn agent_key_and_selection_are_validated_before_any_write() {
    let fixture = setup("mc-validate", &["alpha", "beta"]);

    let error = fixture
        .connection
        .connect(ManagerConnectInput {
            agent_key: Some("bad key!".to_string()),
            ..ManagerConnectInput::default()
        })
        .await
        .expect_err("bad agent key");
    assert_eq!(error.code, "invalid_manager_agent");

    let error = fixture
        .connection
        .connect(ManagerConnectInput {
            agent_key: Some("unknown".to_string()),
            ..ManagerConnectInput::default()
        })
        .await
        .expect_err("unknown agent key");
    assert_eq!(error.code, "invalid_manager_agent");

    let error = fixture
        .connection
        .connect(ManagerConnectInput {
            selection: ManagerSelection {
                preset_ids: vec!["nope".to_string()],
                ..ManagerSelection::default()
            },
            ..ManagerConnectInput::default()
        })
        .await
        .expect_err("unknown preset");
    assert_eq!(error.code, "invalid_manager_selection");

    assert!(
        !fixture
            .home
            .path()
            .join("skills/manager-connection.json")
            .exists(),
        "a rejected connect must not write anything"
    );

    let ok = fixture
        .connection
        .connect(ManagerConnectInput {
            agent_key: Some("pihub".to_string()),
            selection: ManagerSelection {
                preset_ids: vec!["p1".to_string()],
                tags: vec!["common".to_string()],
                ids: Vec::new(),
            },
            source: Some("  manual test  ".to_string()),
            ..ManagerConnectInput::default()
        })
        .await
        .expect("connect");
    assert_eq!(ok["config"]["agentKey"], json!("pihub"));
    assert_eq!(ok["config"]["source"], json!("manual test"));
    assert_eq!(ok["snapshot"]["skillCount"], json!(2));
}

#[tokio::test]
async fn disconnect_keeps_the_snapshot_and_reports_repeat_calls() {
    let fixture = setup("mc-disconnect", &["alpha"]);
    fixture
        .connection
        .connect(connect_input())
        .await
        .expect("connect");

    let first = fixture
        .connection
        .disconnect(ManagerDisconnectInput::default())
        .await
        .expect("disconnect");
    assert_eq!(first["connected"], json!(false));
    assert_eq!(first["alreadyDisconnected"], json!(false));
    assert_eq!(first["snapshotKept"], json!(true));
    assert_eq!(first["snapshot"]["skillCount"], json!(1));

    let second = fixture
        .connection
        .disconnect(ManagerDisconnectInput::default())
        .await
        .expect("disconnect again");
    assert_eq!(second["alreadyDisconnected"], json!(true));
    assert_eq!(second["snapshotKept"], json!(true));

    let purged = fixture
        .connection
        .disconnect(ManagerDisconnectInput {
            purge_snapshot: true,
        })
        .await
        .expect("purge");
    assert_eq!(purged["snapshotKept"], json!(false));
    assert_eq!(purged["snapshot"], Value::Null);
    assert!(!fixture
        .home
        .path()
        .join("skills/manager-catalog.json")
        .exists());

    let status = fixture.connection.status(false).await.expect("status");
    assert_eq!(status["connected"], json!(false));
    assert_eq!(status["config"], Value::Null);
}

#[tokio::test]
async fn missing_cli_is_reported_without_touching_manager_state() {
    let home = common::temp_home("mc-missing");
    let connection = FsManagerConnection::with_options(
        home.path(),
        None,
        Some(BTreeMap::new()),
        Some("linux".to_string()),
        Some(Duration::from_secs(5)),
    )
    .expect("connection");
    let status = connection.status(true).await.expect("status");
    assert_eq!(status["connected"], json!(false));
    assert_eq!(status["manager"]["available"], json!(false));
    assert!(status["manager"]["reason"]
        .as_str()
        .expect("reason")
        .contains("not found"));

    let error = connection
        .connect(ManagerConnectInput::default())
        .await
        .expect_err("connect without CLI");
    assert_eq!(error.code, "manager_unavailable");
    let sync_error = connection
        .sync(ManagerSyncInput::default())
        .await
        .expect_err("sync without connection");
    assert_eq!(sync_error.code, "manager_not_connected");
    let sources_error = connection
        .resolve_sources(None)
        .expect_err("sources without connection");
    assert_eq!(sources_error.code, "manager_not_connected");
    let _ = Path::new("/");
}
