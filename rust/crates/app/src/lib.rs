//! `pihub-app`: CLI, loopback HTTP API, Rust SSR panel and local operations.
//!
//! The crate depends only on `pihub-contracts`: `HubApi` for everything that
//! reads or mutates Agent state, `LocalOps` for offline config/doctor/daemon
//! lifecycle. It never opens the SQLite store and never imports
//! `pihub-core`/`pihub-runtime`.
//!
//! Boundary: the CLI is a control-plane *client*. [`run_cli`] takes
//! [`LocalOperations`] plus [`HubPaths`] and reaches Agent state over loopback
//! HTTP, so an ordinary command can never construct or own a manager and can
//! never touch the running daemon's database. Only the hidden daemon entry
//! ([`run_daemon`]) is handed a `HubApi`, and the bin assembly builds that one.
//!
//! [`HubPaths`]: pihub_contracts::HubPaths

pub mod cli;
pub mod client;
pub mod daemon;
pub mod dashboard;
pub mod error;
pub mod forms;
pub mod http;
pub mod local;

pub use cli::{run_cli, success_envelope, failure_envelope};
pub use client::HubClient;
pub use daemon::{daemon_invocation, run_daemon, serve_listener, DAEMON_SUBCOMMAND};
pub use http::{router, ServerState, BODY_LIMIT, CONTENT_SECURITY_POLICY};
pub use local::LocalOperations;
