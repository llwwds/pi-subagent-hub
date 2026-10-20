//! Bootstrap of the shared Pi configuration files.
//!
//! `config init|validate|paths` and `doctor` belong to the app layer
//! (`pihub-app::LocalOperations`); core only guarantees that an Agent whose
//! carrier needs the shared Pi files finds them, private and well-formed enough
//! to start.

use pihub_contracts::{HubError, HubPaths};
use serde_json::json;

use crate::fsutil;

/// Create `models.json`/`auth.json` when missing and force mode 0600.
pub fn ensure_pi_config_files(paths: &HubPaths) -> Result<(), HubError> {
    if !fsutil::path_exists(&paths.models_path) {
        fsutil::atomic_write_json(
            &paths.models_path,
            &json!({ "providers": {} }),
            fsutil::PRIVATE_FILE_MODE,
        )?;
    }
    if !fsutil::path_exists(&paths.auth_path) {
        fsutil::atomic_write_json(&paths.auth_path, &json!({}), fsutil::PRIVATE_FILE_MODE)?;
    }
    fsutil::chmod(&paths.models_path, fsutil::PRIVATE_FILE_MODE).map_err(HubError::from)?;
    fsutil::chmod(&paths.auth_path, fsutil::PRIVATE_FILE_MODE).map_err(HubError::from)
}
