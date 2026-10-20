//! `pihub` executable assembly.
//!
//! This is the only place that knows all three implementation crates. It keeps
//! the dependency direction one-way and routes:
//! - `pihub __daemon --home <path>` -> native daemon (core + runtime + app HTTP)
//! - every other argv -> app CLI over the native LocalOps boundary
//!
//! The CLI talks to the daemon only through the loopback HTTP client, exactly
//! like 2.1. No old Hub Node implementation is invoked.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pihub_app::{daemon_invocation, run_cli, run_daemon, LocalOperations};
use pihub_contracts::{HubApi, HubError, HubPaths, Store, APP_NAME};
use pihub_core::{
    resolve_hub_log_dir, AgentManager, AgentManagerOptions, FsManagerConnection, FsSkillRegistry,
    HubLog, HubStore, ResolvedProfile,
};
use pihub_runtime::{CarrierFactory, CarrierFactoryConfig, CcSwitchService, RuntimeEnv};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let app_root = resolve_app_root();
    let code = match daemon_invocation(&args) {
        Some(home) => {
            let paths = match home {
                Some(home) => HubPaths::resolve(home, &app_root),
                None => HubPaths::from_env(&app_root),
            };
            run_daemon_process(paths).await
        }
        None => {
            let paths = HubPaths::from_env(&app_root);
            let local = LocalOperations::new(paths.clone());
            run_cli(args, &local, &paths).await
        }
    };
    std::process::exit(code);
}

/// App root used for `vendor/pi`, dashboard assets and lock files.
///
/// Deployment layout is `<app_root>/bin/pihub`; source layout is
/// `rust/target/debug/pihub`. An explicit `PIHUB_APP_ROOT` always wins. Without
/// it, walk up to the directory owning `vendor/pi.lock.json`, then fall back to
/// the executable's `bin/..`.
fn resolve_app_root() -> PathBuf {
    if let Some(value) = std::env::var_os("PIHUB_APP_ROOT") {
        return PathBuf::from(value);
    }
    let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let mut cursor = executable.parent();
    for _ in 0..8 {
        let Some(directory) = cursor else {
            break;
        };
        if directory.join("vendor").join("pi.lock.json").is_file() {
            return directory.to_path_buf();
        }
        cursor = directory.parent();
    }
    let parent = executable.parent().unwrap_or_else(|| Path::new("."));
    if parent.file_name().is_some_and(|name| name == "bin") {
        return parent.parent().unwrap_or(parent).to_path_buf();
    }
    parent.to_path_buf()
}

async fn run_daemon_process(paths: HubPaths) -> i32 {
    match assemble_daemon(&paths) {
        Ok((manager, store)) => {
            let api: Arc<dyn HubApi> = manager.clone();
            let result = run_daemon(api, paths.clone()).await;

            // Single shutdown path: stop every child, flush the single Hub log,
            // then close the one SQLite connection.
            manager.stop_all().await;
            manager.close_logs().await;
            let close_error = store.close().err();
            match (result, close_error) {
                (Ok(()), None) => 0,
                (Ok(()), Some(error)) => {
                    eprintln!("{APP_NAME} daemon cleanup failed: {error}");
                    1
                }
                (Err(error), _) => {
                    eprintln!("{APP_NAME} daemon failed: {error}");
                    error.exit_code.unwrap_or(1)
                }
            }
        }
        Err(error) => {
            eprintln!("{APP_NAME} daemon failed: {error}");
            error.exit_code.unwrap_or(1)
        }
    }
}

fn assemble_daemon(paths: &HubPaths) -> Result<(Arc<AgentManager>, Arc<dyn Store>), HubError> {
    let store: Arc<dyn Store> = Arc::new(HubStore::open(&paths.database_path)?);
    let profile: ResolvedProfile = pihub_core::load_profile(&paths.profile_path)?;
    let skill_registry = Arc::new(FsSkillRegistry::new(&paths.home)?);
    let manager_connection = Arc::new(FsManagerConnection::new(&paths.home)?);
    let provider_service = Arc::new(CcSwitchService::new(paths)?);

    let mut runtime_config =
        CarrierFactoryConfig::new(paths.clone(), RuntimeEnv::from_process());
    // 3.0 default: skills/agents that shell out to `pihub` get this Rust binary,
    // never the 2.1 Node entry. The runtime crate keeps an explicit fallback
    // field for compatibility tests.
    runtime_config.hub_cli_path = std::env::current_exe().ok();
    let runtime_factory = Arc::new(CarrierFactory::new(runtime_config));

    let log = Arc::new(HubLog::open(&resolve_hub_log_dir(paths))?);
    let manager = Arc::new(AgentManager::new(AgentManagerOptions {
        paths: paths.clone(),
        store: store.clone(),
        profile,
        runtime_factory,
        skill_registry: Some(skill_registry),
        manager_connection: Some(manager_connection),
        provider_service: Some(provider_service),
        log: Some(log),
        log_debug: std::env::var("PIHUB_LOG_DEBUG").ok().as_deref() == Some("1"),
    })?);
    manager.recover_after_daemon_restart()?;
    Ok((manager, store))
}
