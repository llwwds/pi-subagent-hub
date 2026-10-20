//! App-level error helpers.
//!
//! `HubError` itself lives in `pihub-contracts`; this module only adds the two
//! things the app layer owns: the exact HTTP status mapping used by the 2.1
//! server, and small constructors for CLI-only failures.

use pihub_contracts::HubError;

/// Default CLI/HTTP error code for a handler that failed (`src/cli.js`,
/// `src/http-server.js`).
pub const CODE_COMMAND_FAILED: &str = "command_failed";
pub const CODE_REQUEST_FAILED: &str = "request_failed";
pub const CODE_UNAUTHORIZED: &str = "unauthorized";
pub const CODE_NOT_FOUND: &str = "not_found";
pub const CODE_AGENT_NOT_FOUND: &str = "agent_not_found";

/// 2.1 maps an unknown Agent to 404 and every other handler failure to 400.
pub fn status_for(code: &str) -> u16 {
    if code == CODE_AGENT_NOT_FOUND {
        404
    } else {
        400
    }
}

/// A command-level failure, reported on stdout as `command_failed`.
pub fn command_failed(message: impl Into<String>) -> HubError {
    HubError::new(CODE_COMMAND_FAILED, message)
}

/// A request-level failure (malformed body, bad flag combination, ...).
pub fn invalid(message: impl Into<String>) -> HubError {
    HubError::new(CODE_REQUEST_FAILED, message)
}

pub fn not_found(message: impl Into<String>) -> HubError {
    HubError::new(CODE_NOT_FOUND, message)
}

pub fn unauthorized() -> HubError {
    HubError::new(CODE_UNAUTHORIZED, "valid local control token required")
}
