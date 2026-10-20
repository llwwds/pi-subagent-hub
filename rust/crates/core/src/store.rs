//! SQLite store: the 2.1 schema, append-only migrations, the update allowlist
//! and cursor-ordered event queries.
//!
//! The connection lives behind a `Mutex` because `rusqlite::Connection` is not
//! `Sync`. Every method is synchronous: the contracts forbid holding a
//! statement, transaction or this lock across an `.await`.

use std::path::Path;
use std::sync::Mutex;

use pihub_contracts::{
    now_iso, AgentPatch, AgentRecord, AgentStatus, DesiredState, EventQuery, HubError, HubResult,
    ListAgentsOptions, NewEvent, ResourceRef, SkillAuthorization, SkillRef, Store, StoredEvent,
    ThinkingLevel,
};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::fsutil;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS agents (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  cwd TEXT NOT NULL,
  workspace_mode TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  thinking TEXT,
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
  carrier TEXT NOT NULL DEFAULT 'pi',
  skill_authorization_json TEXT,
  prompt_config_json TEXT NOT NULL DEFAULT '{}',
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  cursor INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT NOT NULL UNIQUE,
  agent_id TEXT,
  event_type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  FOREIGN KEY(agent_id) REFERENCES agents(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_events_agent_cursor ON events(agent_id, cursor);
CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);
"#;

const INTERRUPTED_MESSAGE: &str =
    "daemon restarted; previous RPC stdio cannot be reattached";

/// One row's worth of raw text columns, before JSON hydration.
struct RawAgent {
    id: String,
    name: String,
    cwd: String,
    workspace_mode: String,
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    status: String,
    desired_state: String,
    pid: Option<i64>,
    session_id: String,
    session_file: Option<String>,
    session_dir: String,
    agent_dir: String,
    logs_dir: String,
    profile_digest: String,
    tools_json: String,
    skills_json: String,
    extensions_json: String,
    carrier: Option<String>,
    skill_authorization_json: Option<String>,
    prompt_config_json: Option<String>,
    last_error: Option<String>,
    created_at: String,
    updated_at: String,
}

pub struct HubStore {
    connection: Mutex<Option<Connection>>,
}

impl std::fmt::Debug for HubStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubStore").finish_non_exhaustive()
    }
}

impl HubStore {
    pub fn open(path: &Path) -> HubResult<Self> {
        if let Some(parent) = path.parent() {
            fsutil::ensure_private_dir(parent)?;
        }
        let connection = Connection::open(path).map_err(sql_error("cannot open store"))?;
        fsutil::chmod(path, fsutil::PRIVATE_FILE_MODE).map_err(HubError::from)?;
        connection
            .execute_batch(SCHEMA)
            .map_err(sql_error("cannot create schema"))?;
        migrate(&connection)?;
        Ok(Self {
            connection: Mutex::new(Some(connection)),
        })
    }

    fn with_connection<T>(&self, operation: impl FnOnce(&Connection) -> HubResult<T>) -> HubResult<T> {
        let guard = self.connection.lock().map_err(|_| {
            HubError::new("store_unavailable", "store lock poisoned by an earlier panic")
        })?;
        let connection = guard.as_ref().ok_or_else(|| {
            HubError::new("store_closed", "store connection is already closed")
        })?;
        operation(connection)
    }
}

fn sql_error(context: &'static str) -> impl Fn(rusqlite::Error) -> HubError {
    move |error| HubError::new("store_failed", format!("{context}: {error}"))
}

fn migrate(connection: &Connection) -> HubResult<()> {
    let mut statement = connection
        .prepare("PRAGMA table_info(agents)")
        .map_err(sql_error("cannot inspect agents table"))?;
    let columns: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sql_error("cannot inspect agents table"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error("cannot inspect agents table"))?;
    drop(statement);

    // Append-only: existing rows are never rewritten, columns are only added.
    if !columns.iter().any(|column| column == "carrier") {
        connection
            .execute_batch("ALTER TABLE agents ADD COLUMN carrier TEXT NOT NULL DEFAULT 'pi'")
            .map_err(sql_error("cannot add carrier column"))?;
    }
    if !columns.iter().any(|column| column == "skill_authorization_json") {
        connection
            .execute_batch("ALTER TABLE agents ADD COLUMN skill_authorization_json TEXT")
            .map_err(sql_error("cannot add skill_authorization_json column"))?;
    }
    if !columns.iter().any(|column| column == "prompt_config_json") {
        connection
            .execute_batch("ALTER TABLE agents ADD COLUMN prompt_config_json TEXT NOT NULL DEFAULT '{}'")
            .map_err(sql_error("cannot add prompt_config_json column"))?;
    }
    Ok(())
}

fn read_raw(row: &Row<'_>) -> rusqlite::Result<RawAgent> {
    Ok(RawAgent {
        id: row.get("id")?,
        name: row.get("name")?,
        cwd: row.get("cwd")?,
        workspace_mode: row.get("workspace_mode")?,
        provider: row.get("provider")?,
        model: row.get("model")?,
        thinking: row.get("thinking")?,
        status: row.get("status")?,
        desired_state: row.get("desired_state")?,
        pid: row.get("pid")?,
        session_id: row.get("session_id")?,
        session_file: row.get("session_file")?,
        session_dir: row.get("session_dir")?,
        agent_dir: row.get("agent_dir")?,
        logs_dir: row.get("logs_dir")?,
        profile_digest: row.get("profile_digest")?,
        tools_json: row.get("tools_json")?,
        skills_json: row.get("skills_json")?,
        extensions_json: row.get("extensions_json")?,
        carrier: row.get("carrier")?,
        skill_authorization_json: row.get("skill_authorization_json")?,
        prompt_config_json: row.get("prompt_config_json")?,
        last_error: row.get("last_error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn json_error(column: &str, error: serde_json::Error) -> HubError {
    HubError::new(
        "store_corrupt",
        format!("column {column} does not hold valid JSON: {error}"),
    )
}

fn hydrate(raw: RawAgent) -> HubResult<AgentRecord> {
    let tools: Vec<String> =
        serde_json::from_str(&raw.tools_json).map_err(|error| json_error("tools_json", error))?;
    let skills: Vec<SkillRef> =
        serde_json::from_str(&raw.skills_json).map_err(|error| json_error("skills_json", error))?;
    let extensions: Vec<ResourceRef> = serde_json::from_str(&raw.extensions_json)
        .map_err(|error| json_error("extensions_json", error))?;
    let authorization: Option<SkillAuthorization> = match raw.skill_authorization_json.as_deref() {
        Some(text) if !text.trim().is_empty() && text.trim() != "null" => {
            Some(serde_json::from_str(text).map_err(|error| {
                json_error("skill_authorization_json", error)
            })?)
        }
        _ => None,
    };
    let carrier = match raw.carrier.as_deref() {
        Some(value) => value.parse().map_err(|error: HubError| error)?,
        None => pihub_contracts::Carrier::Pi,
    };
    Ok(AgentRecord {
        id: raw.id,
        name: raw.name,
        cwd: raw.cwd.into(),
        workspace_mode: raw.workspace_mode,
        provider: raw.provider,
        model: raw.model,
        thinking: match raw.thinking.as_deref() {
            Some(value) => Some(value.parse().map_err(|error: HubError| error)?),
            None => None,
        },
        status: raw.status.parse()?,
        desired_state: raw.desired_state.parse()?,
        pid: raw.pid.and_then(|value| u32::try_from(value).ok()),
        session_id: raw.session_id,
        session_file: raw.session_file,
        session_dir: raw.session_dir.into(),
        agent_dir: raw.agent_dir.into(),
        logs_dir: raw.logs_dir.into(),
        profile_digest: raw.profile_digest,
        tools,
        skills,
        extensions,
        carrier,
        skill_authorization: authorization,
        prompt_config: crate::prompts::read_frozen_prompt_config(
            raw.prompt_config_json.as_deref(),
        ),
        last_error: raw.last_error,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
    })
}

fn select_agent(connection: &Connection, id: &str) -> HubResult<Option<AgentRecord>> {
    let raw = connection
        .prepare("SELECT * FROM agents WHERE id = ?1")
        .map_err(sql_error("cannot prepare agent lookup"))?
        .query_row(params![id], read_raw)
        .optional()
        .map_err(sql_error("cannot read agent"))?;
    match raw {
        Some(raw) => Ok(Some(hydrate(raw)?)),
        None => Ok(None),
    }
}

impl Store for HubStore {
    fn create_agent(&self, agent: &AgentRecord) -> HubResult<AgentRecord> {
        let tools_json = serde_json::to_string(&agent.tools)
            .map_err(|error| HubError::new("invalid_agent", error.to_string()))?;
        let skills_json = serde_json::to_string(&agent.skills)
            .map_err(|error| HubError::new("invalid_agent", error.to_string()))?;
        let extensions_json = serde_json::to_string(&agent.extensions)
            .map_err(|error| HubError::new("invalid_agent", error.to_string()))?;
        let authorization_json = match &agent.skill_authorization {
            Some(value) => Some(
                serde_json::to_string(value)
                    .map_err(|error| HubError::new("invalid_agent", error.to_string()))?,
            ),
            None => None,
        };
        let prompt_config_json = match &agent.prompt_config {
            Some(value) => serde_json::to_string(value)
                .map_err(|error| HubError::new("invalid_agent", error.to_string()))?,
            None => "{}".to_string(),
        };
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO agents (
                        id, name, cwd, workspace_mode, provider, model, thinking, status, desired_state, pid,
                        session_id, session_file, session_dir, agent_dir, logs_dir, profile_digest,
                        tools_json, skills_json, extensions_json, carrier, skill_authorization_json,
                        prompt_config_json, last_error, created_at, updated_at
                    ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                        ?11, ?12, ?13, ?14, ?15, ?16,
                        ?17, ?18, ?19, ?20, ?21,
                        ?22, ?23, ?24, ?25
                    )",
                    params![
                        agent.id,
                        agent.name,
                        agent.cwd.to_string_lossy(),
                        agent.workspace_mode,
                        agent.provider,
                        agent.model,
                        agent.thinking.map(|value| value.as_str()),
                        agent.status.as_str(),
                        agent.desired_state.as_str(),
                        agent.pid.map(i64::from),
                        agent.session_id,
                        agent.session_file,
                        agent.session_dir.to_string_lossy(),
                        agent.agent_dir.to_string_lossy(),
                        agent.logs_dir.to_string_lossy(),
                        agent.profile_digest,
                        tools_json,
                        skills_json,
                        extensions_json,
                        agent.carrier.as_str(),
                        authorization_json,
                        prompt_config_json,
                        agent.last_error,
                        agent.created_at,
                        agent.updated_at,
                    ],
                )
                .map_err(sql_error("cannot insert agent"))?;
            select_agent(connection, &agent.id)?
                .ok_or_else(|| HubError::new("store_failed", "agent vanished after insert"))
        })
    }

    fn get_agent(&self, id: &str) -> HubResult<Option<AgentRecord>> {
        self.with_connection(|connection| select_agent(connection, id))
    }

    fn list_agents(&self, options: ListAgentsOptions) -> HubResult<Vec<AgentRecord>> {
        self.with_connection(|connection| {
            let mut filters: Vec<&str> = Vec::new();
            if !options.include_stopped {
                filters.push("status NOT IN ('stopped', 'failed')");
            }
            if options.legacy_only {
                filters.push("skill_authorization_json IS NULL");
            }
            let where_clause = if filters.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", filters.join(" AND "))
            };
            let sql = format!("SELECT * FROM agents{where_clause} ORDER BY created_at DESC");
            let mut statement = connection
                .prepare(&sql)
                .map_err(sql_error("cannot prepare agent list"))?;
            let raw_rows: Vec<RawAgent> = statement
                .query_map([], read_raw)
                .map_err(sql_error("cannot list agents"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error("cannot list agents"))?;
            raw_rows.into_iter().map(hydrate).collect()
        })
    }

    fn update_agent(&self, id: &str, patch: AgentPatch) -> HubResult<AgentRecord> {
        let mut assignments: Vec<&str> = Vec::new();
        let mut sql_params: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(value) = patch.name {
            assignments.push("name = ?");
            sql_params.push(rusqlite::types::Value::Text(value));
        }
        if let Some(value) = patch.status {
            assignments.push("status = ?");
            sql_params.push(rusqlite::types::Value::Text(value.as_str().to_string()));
        }
        if let Some(value) = patch.desired_state {
            assignments.push("desired_state = ?");
            sql_params.push(rusqlite::types::Value::Text(value.as_str().to_string()));
        }
        if let Some(value) = patch.pid {
            assignments.push("pid = ?");
            sql_params.push(match value {
                Some(pid) => rusqlite::types::Value::Integer(i64::from(pid)),
                None => rusqlite::types::Value::Null,
            });
        }
        if let Some(value) = patch.session_file {
            assignments.push("session_file = ?");
            sql_params.push(match value {
                Some(value) => rusqlite::types::Value::Text(value),
                None => rusqlite::types::Value::Null,
            });
        }
        if let Some(value) = patch.last_error {
            assignments.push("last_error = ?");
            sql_params.push(match value {
                Some(value) => rusqlite::types::Value::Text(value),
                None => rusqlite::types::Value::Null,
            });
        }
        if let Some(value) = patch.provider {
            assignments.push("provider = ?");
            sql_params.push(match value {
                Some(value) => rusqlite::types::Value::Text(value),
                None => rusqlite::types::Value::Null,
            });
        }
        if let Some(value) = patch.model {
            assignments.push("model = ?");
            sql_params.push(match value {
                Some(value) => rusqlite::types::Value::Text(value),
                None => rusqlite::types::Value::Null,
            });
        }
        if let Some(value) = patch.thinking {
            assignments.push("thinking = ?");
            sql_params.push(match value {
                Some(value) => rusqlite::types::Value::Text(value.as_str().to_string()),
                None => rusqlite::types::Value::Null,
            });
        }
        let updated_at = patch.updated_at.unwrap_or_else(now_iso);
        assignments.push("updated_at = ?");
        sql_params.push(rusqlite::types::Value::Text(updated_at));

        self.with_connection(|connection| {
            if assignments.is_empty() {
                return select_agent(connection, id)?
                    .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")));
            }
            let sql = format!("UPDATE agents SET {} WHERE id = ?", assignments.join(", "));
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(sql_params.len() + 1);
            for value in &sql_params {
                params.push(value);
            }
            let id_value = rusqlite::types::Value::Text(id.to_string());
            params.push(&id_value);
            connection
                .execute(&sql, params.as_slice())
                .map_err(sql_error("cannot update agent"))?;
            select_agent(connection, id)?
                .ok_or_else(|| HubError::new("agent_not_found", format!("unknown agent: {id}")))
        })
    }

    fn append_event(&self, event: NewEvent) -> HubResult<i64> {
        let payload_json = serde_json::to_string(&event.payload)
            .map_err(|error| HubError::new("invalid_event", error.to_string()))?;
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO events (event_id, agent_id, event_type, payload_json, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event.event_id,
                        event.agent_id,
                        event.event_type,
                        payload_json,
                        event.created_at,
                    ],
                )
                .map_err(sql_error("cannot append event"))?;
            Ok(connection.last_insert_rowid())
        })
    }

    fn list_events(&self, query: EventQuery) -> HubResult<Vec<StoredEvent>> {
        let limit = query.clamped_limit() as i64;
        let after = query.after.max(0);
        self.with_connection(|connection| {
            let (sql, mut bindings): (&str, Vec<rusqlite::types::Value>) = match (
                query.legacy_only,
                query.agent_id.clone(),
            ) {
                (true, Some(agent_id)) => (
                    "SELECT e.* FROM events e JOIN agents a ON a.id = e.agent_id \
                     WHERE a.skill_authorization_json IS NULL AND e.agent_id = ?1 AND e.cursor > ?2 \
                     ORDER BY e.cursor ASC LIMIT ?3",
                    vec![
                        rusqlite::types::Value::Text(agent_id),
                        rusqlite::types::Value::Integer(after),
                    ],
                ),
                (true, None) => (
                    "SELECT e.* FROM events e JOIN agents a ON a.id = e.agent_id \
                     WHERE a.skill_authorization_json IS NULL AND e.cursor > ?1 \
                     ORDER BY e.cursor ASC LIMIT ?2",
                    vec![rusqlite::types::Value::Integer(after)],
                ),
                (false, Some(agent_id)) => (
                    "SELECT * FROM events WHERE agent_id = ?1 AND cursor > ?2 \
                     ORDER BY cursor ASC LIMIT ?3",
                    vec![
                        rusqlite::types::Value::Text(agent_id),
                        rusqlite::types::Value::Integer(after),
                    ],
                ),
                (false, None) => (
                    "SELECT * FROM events WHERE cursor > ?1 ORDER BY cursor ASC LIMIT ?2",
                    vec![rusqlite::types::Value::Integer(after)],
                ),
            };
            bindings.push(rusqlite::types::Value::Integer(limit));
            let mut statement = connection
                .prepare(sql)
                .map_err(sql_error("cannot prepare event query"))?;
            let params: Vec<&dyn rusqlite::ToSql> =
                bindings.iter().map(|value| value as &dyn rusqlite::ToSql).collect();
            let rows: Vec<(i64, String, Option<String>, String, String, String)> = statement
                .query_map(params.as_slice(), |row| {
                    Ok((
                        row.get("cursor")?,
                        row.get("event_id")?,
                        row.get("agent_id")?,
                        row.get("event_type")?,
                        row.get("payload_json")?,
                        row.get("created_at")?,
                    ))
                })
                .map_err(sql_error("cannot query events"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error("cannot query events"))?;
            rows.into_iter()
                .map(
                    |(cursor, event_id, agent_id, event_type, payload_json, created_at)| {
                        let payload: Value = serde_json::from_str(&payload_json)
                            .map_err(|error| json_error("payload_json", error))?;
                        Ok(StoredEvent {
                            cursor,
                            event_id,
                            agent_id,
                            event_type,
                            payload,
                            // 2.1 returns the stored row unchanged, so clients get
                            // the exact persisted bytes rather than a re-encoding.
                            payload_json,
                            created_at,
                        })
                    },
                )
                .collect()
        })
    }

    fn latest_cursor(&self, legacy_only: bool) -> HubResult<i64> {
        self.with_connection(|connection| {
            let sql = if legacy_only {
                "SELECT COALESCE(MAX(e.cursor), 0) FROM events e JOIN agents a ON a.id = e.agent_id \
                 WHERE a.skill_authorization_json IS NULL"
            } else {
                "SELECT COALESCE(MAX(cursor), 0) FROM events"
            };
            connection
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .map_err(sql_error("cannot read event cursor"))
        })
    }

    fn mark_interrupted(&self) -> HubResult<()> {
        let now = now_iso();
        self.with_connection(|connection| {
            let placeholders = AgentStatus::ACTIVE
                .iter()
                .map(|status| format!("'{}'", status.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "UPDATE agents SET status = 'crashed', pid = NULL, last_error = ?1, updated_at = ?2 \
                 WHERE status IN ({placeholders})"
            );
            connection
                .execute(&sql, params![INTERRUPTED_MESSAGE, now])
                .map_err(sql_error("cannot mark interrupted agents"))?;
            Ok(())
        })
    }

    fn close(&self) -> HubResult<()> {
        let mut guard = self.connection.lock().map_err(|_| {
            HubError::new("store_unavailable", "store lock poisoned by an earlier panic")
        })?;
        if let Some(connection) = guard.take() {
            connection
                .close()
                .map_err(|(_, error)| sql_error("cannot close store")(error))?;
        }
        Ok(())
    }
}

/// Convenience for tests and for callers that only have a status string.
pub fn parse_status(value: &str) -> HubResult<AgentStatus> {
    value.parse()
}

pub fn parse_desired_state(value: &str) -> HubResult<DesiredState> {
    value.parse()
}

pub fn parse_thinking(value: Option<&str>) -> HubResult<Option<ThinkingLevel>> {
    match value {
        Some(value) => Ok(Some(value.parse()?)),
        None => Ok(None),
    }
}
