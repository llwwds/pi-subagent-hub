//! Store parity: schema, append-only migrations, cursor ordering, allowlist,
//! interrupted-Agent recovery and frozen prompt hydration.

mod common;

use pihub_contracts::{
    AgentPatch, AgentRecord, AgentStatus, Carrier, DesiredState, EventQuery, HubResult,
    ListAgentsOptions, NewEvent, ResourceRef, SkillRef, Store, ThinkingLevel,
};
use pihub_core::HubStore;
use serde_json::json;

fn agent(id: &str, status: AgentStatus, created_at: &str) -> AgentRecord {
    AgentRecord {
        id: id.to_string(),
        name: id.to_string(),
        cwd: "/tmp".into(),
        workspace_mode: "shared".to_string(),
        provider: None,
        model: None,
        thinking: None,
        status,
        desired_state: DesiredState::Running,
        pid: Some(4242),
        session_id: format!("session-{id}"),
        session_file: None,
        session_dir: format!("/tmp/{id}/sessions").into(),
        agent_dir: format!("/tmp/{id}/pi-agent").into(),
        logs_dir: format!("/tmp/{id}/logs").into(),
        profile_digest: "sha256:profile".to_string(),
        tools: vec!["read".to_string()],
        skills: vec![SkillRef::Profile {
            path: "/tmp/skill".into(),
            sha256: "0".repeat(64),
        }],
        extensions: Vec::<ResourceRef>::new(),
        carrier: Carrier::Pi,
        skill_authorization: None,
        prompt_config: None,
        last_error: None,
        created_at: created_at.to_string(),
        updated_at: created_at.to_string(),
    }
}

#[test]
fn store_persists_agents_and_orders_event_cursors() -> HubResult<()> {
    let home = common::temp_home("store-basic");
    let database = home.path().join("state/hub.sqlite");
    let store = HubStore::open(&database)?;
    store.create_agent(&agent("agent-one", AgentStatus::Idle, "2026-01-01T00:00:00.000Z"))?;

    let first = store.append_event(NewEvent::new(
        "event-one",
        Some("agent-one".to_string()),
        "agent_ready",
        json!({"n": 1}),
    ))?;
    let second = store.append_event(NewEvent::new(
        "event-two",
        Some("agent-one".to_string()),
        "agent_settled",
        json!({"n": 2}),
    ))?;
    assert_eq!((first, second), (1, 2));

    assert_eq!(store.list_agents(ListAgentsOptions::default())?.len(), 1);
    assert_eq!(
        store.get_agent("agent-one")?.expect("agent").tools,
        vec!["read".to_string()]
    );

    let after_first = store.list_events(EventQuery {
        agent_id: Some("agent-one".to_string()),
        after: 1,
        ..EventQuery::default()
    })?;
    assert_eq!(after_first.len(), 1);
    assert_eq!(after_first[0].payload["n"], json!(2));
    assert_eq!(store.latest_cursor(false)?, 2);

    // `EventQuery::clamped_limit` follows 2.1: `limit=0` means the 500 default,
    // then the value is clamped into 1..=2000.
    assert_eq!(
        EventQuery {
            limit: 0,
            ..EventQuery::default()
        }
        .clamped_limit(),
        500,
        "an explicit 0 means the default 500, not 1"
    );
    assert_eq!(
        EventQuery {
            limit: 1,
            ..EventQuery::default()
        }
        .clamped_limit(),
        1
    );
    assert_eq!(
        EventQuery {
            limit: 9_999,
            ..EventQuery::default()
        }
        .clamped_limit(),
        2_000
    );
    assert_eq!(
        EventQuery::default().clamped_limit(),
        500,
        "an unset limit stays 500, matching 2.1"
    );
    let clamped = store.list_events(EventQuery {
        agent_id: Some("agent-one".to_string()),
        after: 0,
        limit: 1,
        legacy_only: false,
    })?;
    assert_eq!(clamped.len(), 1);
    let defaulted = store.list_events(EventQuery {
        agent_id: Some("agent-one".to_string()),
        after: 0,
        limit: 0,
        legacy_only: false,
    })?;
    assert_eq!(defaulted.len(), 2, "limit 0 falls back to the 500 default");

    store.close()?;
    // Reopening keeps rows and the AUTOINCREMENT cursor.
    let reopened = HubStore::open(&database)?;
    assert_eq!(reopened.latest_cursor(false)?, 2);
    assert_eq!(reopened.get_agent("agent-one")?.expect("agent").name, "agent-one");
    reopened.close()?;
    Ok(())
}

#[test]
fn stored_events_keep_the_raw_payload_json_column() -> HubResult<()> {
    let home = common::temp_home("store-payload-json");
    let database = home.path().join("state/hub.sqlite");
    let store = HubStore::open(&database)?;
    store.create_agent(&agent("agent-raw", AgentStatus::Idle, "2026-01-01T00:00:00.000Z"))?;

    // A payload whose serialization differs from a naive re-encode: key order and
    // spacing are the caller's, and 2.1 hands the stored bytes back unchanged.
    let raw = "{ \"z\": 1, \"a\": [2,3] }";
    {
        let connection = rusqlite::Connection::open(&database).expect("open raw");
        connection
            .execute(
                "INSERT INTO events (event_id, agent_id, event_type, payload_json, created_at) \
                 VALUES ('raw-event','agent-raw','note',?1,'2026-01-01T00:00:00.000Z')",
                rusqlite::params![raw],
            )
            .expect("insert raw event");
    }

    let events = store.list_events(EventQuery {
        agent_id: Some("agent-raw".to_string()),
        ..EventQuery::default()
    })?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload_json, raw, "raw column text, not re-serialized");
    assert_eq!(events[0].payload["z"], json!(1));
    assert_eq!(events[0].payload["a"], json!([2, 3]));
    store.close()?;
    Ok(())
}

#[test]
fn store_migrates_a_2_0_database_in_place() -> HubResult<()> {
    let home = common::temp_home("store-migrate");
    let database = home.path().join("state/hub.sqlite");
    common::mkdir(database.parent().expect("state dir"));
    {
        // 2.0 shape: no carrier, no skill authorization, no prompt column.
        let connection = rusqlite::Connection::open(&database).expect("open legacy db");
        connection
            .execute_batch(
                "CREATE TABLE agents (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    workspace_mode TEXT NOT NULL,
                    provider TEXT, model TEXT, thinking TEXT,
                    status TEXT NOT NULL,
                    desired_state TEXT NOT NULL,
                    pid INTEGER,
                    session_id TEXT NOT NULL,
                    session_file TEXT,
                    session_dir TEXT NOT NULL,
                    agent_dir TEXT NOT NULL,
                    logs_dir TEXT NOT NULL,
                    profile_digest TEXT NOT NULL,
                    tools_json TEXT NOT NULL,
                    skills_json TEXT NOT NULL,
                    extensions_json TEXT NOT NULL,
                    last_error TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE events (
                    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id TEXT NOT NULL UNIQUE,
                    agent_id TEXT,
                    event_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                INSERT INTO agents VALUES (
                    'legacy-agent','legacy','/tmp','shared',NULL,NULL,NULL,'idle','stopped',NULL,
                    'session-legacy',NULL,'/tmp/sessions','/tmp/agent','/tmp/logs','sha256:legacy',
                    '[\"read\"]','[]','[]',NULL,'2026-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z'
                );
                INSERT INTO events (event_id, agent_id, event_type, payload_json, created_at)
                    VALUES ('legacy-event','legacy-agent','agent_ready','{\"pid\":1}','2026-01-01T00:00:00.000Z');",
            )
            .expect("legacy schema");
    }

    let store = HubStore::open(&database)?;
    let legacy = store.get_agent("legacy-agent")?.expect("legacy agent");
    assert_eq!(legacy.carrier, Carrier::Pi);
    assert!(legacy.skill_authorization.is_none());
    assert!(legacy.prompt_config.is_none());
    assert_eq!(legacy.status, AgentStatus::Idle);
    assert_eq!(store.latest_cursor(true)?, 1);

    let mut modern = agent("new-agent", AgentStatus::Idle, "2026-02-01T00:00:00.000Z");
    modern.prompt_config = Some(pihub_contracts::FrozenPromptConfig {
        schema_version: 1,
        mode: pihub_contracts::PromptMode::None,
        carrier: Carrier::Pi,
        channel: Some(pihub_contracts::PromptChannel::Append),
        support: Some(pihub_contracts::PromptChannel::Append),
        principle_ids: Vec::new(),
        custom_text: String::new(),
        prompt: String::new(),
        digest: format!("sha256:{}", "a".repeat(64)),
        resolved_at: Some("2026-02-01T00:00:00.000Z".to_string()),
    });
    // A platform row carries a frozen authorization; `legacy_only` must skip it.
    modern.skill_authorization = Some(pihub_contracts::SkillAuthorization {
        mode: pihub_contracts::SkillPolicyMode::Only,
        grants: Vec::new(),
        digest: format!("sha256:{}", "b".repeat(64)),
    });
    store.create_agent(&modern)?;
    let stored = store.get_agent("new-agent")?.expect("new agent");
    let frozen = stored.prompt_config.expect("frozen prompt hydrated");
    assert_eq!(frozen.digest, modern.prompt_config.as_ref().expect("frozen").digest);

    assert_eq!(
        store
            .list_agents(ListAgentsOptions {
                include_stopped: true,
                legacy_only: false
            })?
            .len(),
        2
    );
    let legacy_rows = store.list_agents(ListAgentsOptions {
        include_stopped: true,
        legacy_only: true,
    })?;
    assert_eq!(legacy_rows.len(), 1);
    assert_eq!(legacy_rows[0].id, "legacy-agent");
    store.close()?;
    Ok(())
}

#[test]
fn store_update_allowlist_and_interrupted_recovery() -> HubResult<()> {
    let home = common::temp_home("store-update");
    let store = HubStore::open(&home.path().join("state/hub.sqlite"))?;
    store.create_agent(&agent("agent-active", AgentStatus::Running, "2026-01-01T00:00:00.000Z"))?;
    store.create_agent(&agent("agent-crashed", AgentStatus::Crashed, "2026-01-02T00:00:00.000Z"))?;

    let updated = store.update_agent(
        "agent-active",
        AgentPatch {
            status: Some(AgentStatus::WaitingInput),
            pid: Some(Some(777)),
            thinking: Some(Some(ThinkingLevel::High)),
            last_error: Some(Some("note".to_string())),
            ..AgentPatch::default()
        },
    )?;
    assert_eq!(updated.status, AgentStatus::WaitingInput);
    assert_eq!(updated.pid, Some(777));
    assert_eq!(updated.thinking, Some(ThinkingLevel::High));
    assert_eq!(updated.last_error.as_deref(), Some("note"));
    // Cleared pid / last_error reach SQL NULL, not a stale value.
    let cleared = store.update_agent(
        "agent-active",
        AgentPatch {
            pid: Some(None),
            last_error: Some(None),
            ..AgentPatch::default()
        },
    )?;
    assert_eq!(cleared.pid, None);
    assert_eq!(cleared.last_error, None);

    store.mark_interrupted()?;
    let interrupted = store.get_agent("agent-active")?.expect("agent");
    assert_eq!(interrupted.status, AgentStatus::Crashed);
    assert_eq!(interrupted.pid, None);
    assert_eq!(
        interrupted.last_error.as_deref(),
        Some("daemon restarted; previous RPC stdio cannot be reattached")
    );
    let untouched = store.get_agent("agent-crashed")?.expect("agent");
    assert_eq!(untouched.status, AgentStatus::Crashed);
    assert_eq!(untouched.last_error, None);
    store.close()?;
    Ok(())
}

#[test]
fn store_events_are_ordered_by_cursor_and_scoped_per_agent() -> HubResult<()> {
    let home = common::temp_home("store-events");
    let store = HubStore::open(&home.path().join("state/hub.sqlite"))?;
    store.create_agent(&agent("agent-a", AgentStatus::Idle, "2026-01-01T00:00:00.000Z"))?;
    store.create_agent(&agent("agent-b", AgentStatus::Idle, "2026-01-02T00:00:00.000Z"))?;
    for index in 0..5 {
        store.append_event(NewEvent::new(
            format!("a-{index}"),
            Some("agent-a".to_string()),
            "note",
            json!(index),
        ))?;
        store.append_event(NewEvent::new(
            format!("b-{index}"),
            Some("agent-b".to_string()),
            "note",
            json!(index),
        ))?;
    }
    let scoped = store.list_events(EventQuery {
        agent_id: Some("agent-a".to_string()),
        after: 2,
        limit: 2,
        legacy_only: false,
    })?;
    assert_eq!(scoped.len(), 2);
    assert!(scoped.iter().all(|event| event.agent_id.as_deref() == Some("agent-a")));
    assert!(scoped[0].cursor < scoped[1].cursor);
    assert_eq!(store.latest_cursor(false)?, 10);
    store.close()?;
    Ok(())
}
