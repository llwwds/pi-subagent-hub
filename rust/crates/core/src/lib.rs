//! Core implementation crate: store, agent lifecycle, prompts, skills,
//! Skills Manager connection, bounded Hub logging and offline configuration.
//!
//! All cross-crate types and traits come from `pihub-contracts`; nothing here
//! depends on `pihub-runtime` or `pihub-app`.

mod config;
pub mod fsutil;
pub mod logging;
pub mod manager;
pub mod profiles;
pub mod prompts;
pub mod skills;
pub mod store;

pub use logging::{HubLog, HubLogLimits, resolve_hub_log_dir};
pub use manager::{AgentManager, AgentManagerOptions};
pub use profiles::{ResolvedProfile, hash_path, load_profile, resolve_profile};
pub use prompts::DefaultPromptService;
pub use skills::manager_connection::FsManagerConnection;
pub use skills::registry::FsSkillRegistry;
pub use store::HubStore;
