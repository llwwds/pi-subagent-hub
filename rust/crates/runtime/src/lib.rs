//! Carrier runtimes (Pi RPC, Codex/Claude CLI) and the CC Switch provider
//! service.
//!
//! Owned by `pihub-rust-runtime-20261021`. This crate depends only on
//! `pihub-contracts`; it never depends on core or app, never opens the Hub
//! database, and never writes Manager state.
//!
//! Invariants kept from 2.1:
//! - One bounded `RuntimeEvent` channel per Agent; carrier payloads stay
//!   `serde_json::Value` and are not reshaped by Rust.
//! - Requests are correlated by id; a timeout or process exit rejects every
//!   pending request with the 2.1 message wording.
//! - Every child process runs in an isolated argv/env/HOME profile and is
//!   terminated with SIGTERM -> 5s grace -> SIGKILL (process group when the
//!   CC Switch route is used).

pub mod carriers;
pub mod cc_switch;
pub mod cli_runtime;
pub mod env;
pub mod fsx;
pub mod jsonl;
pub mod mount;
pub mod pi_rpc;
pub mod proc;
pub mod protocol_log;
pub mod prompt_args;

pub use carriers::{CarrierFactory, CarrierFactoryConfig};
pub use cli_runtime::CcSwitchRoute;
pub use cc_switch::CcSwitchService;
pub use cli_runtime::{CliRuntime, CliSpec};
pub use env::RuntimeEnv;
pub use jsonl::{JsonlReader, JsonlRecord};
pub use pi_rpc::{PiRpcProcess, PiSpec};
