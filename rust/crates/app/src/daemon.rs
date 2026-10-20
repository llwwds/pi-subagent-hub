//! Daemon host: binds a random loopback port, publishes `state/daemon.json`
//! and `state/control.json`, serves the HTTP API, and shuts down on
//! `POST /v1/daemon/stop` or a termination signal.
//!
//! The daemon is started by the CLI as `pihub __daemon --home <path>`; the bin
//! assembly routes that hidden command here so the app never needs a second
//! executable.

use std::path::PathBuf;
use std::sync::Arc;

use pihub_contracts::{
    ControlRecord, DaemonLockRecord, HubApi, HubError, HubPaths, HubResult,
};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::http::{router, ServerState};
use crate::local::{
    atomic_write_json, ensure_config_files, ensure_hub_layout, is_process_alive, read_json,
};

/// Hidden entry point used by `daemon start`.
pub const DAEMON_SUBCOMMAND: &str = "__daemon";

/// Parse `pihub __daemon --home <path>`.
///
/// Returns `Some(home)` when the first argument is the daemon subcommand. When
/// `--home` is absent the caller should fall back to `HubPaths::from_env`.
pub fn daemon_invocation(args: &[String]) -> Option<Option<PathBuf>> {
    if args.first().map(String::as_str) != Some(DAEMON_SUBCOMMAND) {
        return None;
    }
    let home = args
        .iter()
        .position(|value| value == "--home")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from);
    Some(home)
}

/// Run the daemon until it is asked to stop.
pub async fn run_daemon(api: Arc<dyn HubApi>, paths: HubPaths) -> HubResult<()> {
    if !paths.is_state_home_outside_app() {
        return Err(HubError::new(
            "unsafe_home",
            format!(
                "PIHUB_HOME must be outside the source tree: {}",
                paths.app_root.display()
            ),
        ));
    }
    ensure_hub_layout(&paths)?;
    ensure_config_files(&paths)?;

    // Single-instance guard: an existing live daemon makes this a losing start.
    if let Some(lock) = read_json::<DaemonLockRecord>(&paths.lock_path)?
        && is_process_alive(lock.pid)
        && lock.pid != std::process::id()
    {
        return Err(
            HubError::new(
                "daemon_already_running",
                format!("pi-subagent-hub daemon already running with pid {}", lock.pid),
            )
            .with_exit_code(3),
        );
    }
    atomic_write_json(
        &paths.lock_path,
        &serde_json::to_value(DaemonLockRecord {
            pid: std::process::id(),
            started_at: pihub_contracts::now_iso(),
        })?,
    )?;

    let control = match read_json::<ControlRecord>(&paths.control_path)? {
        Some(control) if !control.token.is_empty() => control,
        _ => {
            let control = ControlRecord {
                token: new_token(),
                created_at: pihub_contracts::now_iso(),
            };
            atomic_write_json(&paths.control_path, &serde_json::to_value(&control)?)?;
            control
        }
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|error| {
        HubError::new("daemon_bind_failed", format!("cannot bind loopback: {error}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|error| HubError::new("daemon_bind_failed", error.to_string()))?
        .port();

    let stop = Arc::new(Notify::new());
    let state = Arc::new(ServerState::with_stop(
        api,
        paths.clone(),
        control.token.clone(),
        stop.clone(),
    ));

    let metadata = crate::local::daemon_metadata("127.0.0.1", port, &paths.app_root);
    atomic_write_json(&paths.daemon_path, &serde_json::to_value(&metadata)?)?;
    println!(
        "pi-subagent-hub daemon {} listening on 127.0.0.1:{port}",
        pihub_contracts::APP_VERSION
    );

    let serve_result = serve_listener(router(state), listener, stop).await;
    cleanup(&paths);
    serve_result.map_err(|error| HubError::new("daemon_serve_failed", error.to_string()))
}

/// Serve a prepared router on an already-bound listener until `stop` fires.
pub async fn serve_listener(
    router: axum::Router,
    listener: TcpListener,
    stop: Arc<Notify>,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal(stop))
        .await
}

async fn shutdown_signal(stop: Arc<Notify>) {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(_) => {
                    stop.notified().await;
                    return;
                }
            };
        tokio::select! {
            _ = stop.notified() => {},
            _ = terminate.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = stop.notified() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    }
}

fn cleanup(paths: &HubPaths) {
    let _ = std::fs::remove_file(&paths.daemon_path);
    let _ = std::fs::remove_file(&paths.lock_path);
}

/// 32 random bytes, hex encoded: two v4 UUIDs plus pid/time.
pub fn new_token() -> String {
    let material = format!(
        "{}:{}:{}:{}",
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        std::process::id(),
        pihub_contracts::now_iso()
    );
    pihub_contracts::sha256_hex(material.as_bytes())
}
