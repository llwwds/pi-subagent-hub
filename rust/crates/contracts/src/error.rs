use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type HubResult<T> = Result<T, HubError>;

/// Internal error. `code` is part of the wire contract.
#[derive(Debug, Clone, Serialize)]
pub struct HubError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Option<Value>,
    /// CLI-only exit code; never serialized into the JSON envelope.
    #[serde(skip)]
    pub exit_code: Option<i32>,
}

impl HubError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            details: None,
            exit_code: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("request_failed", message)
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code.clone(),
            message: self.message.clone(),
            retryable: self.retryable,
            details: self.details.clone(),
        }
    }
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HubError {}

impl From<std::io::Error> for HubError {
    fn from(value: std::io::Error) -> Self {
        Self::new("io_error", value.to_string())
    }
}

impl From<serde_json::Error> for HubError {
    fn from(value: serde_json::Error) -> Self {
        Self::new("invalid_json", value.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default)]
    pub details: Option<Value>,
}

impl From<&HubError> for ErrorBody {
    fn from(value: &HubError) -> Self {
        value.body()
    }
}

impl From<HubError> for ErrorBody {
    fn from(value: HubError) -> Self {
        value.body()
    }
}

/// The single CLI/HTTP envelope: `{ok,data,error}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub ok: bool,
    pub data: Option<T>,
    pub error: Option<ErrorBody>,
}

impl<T> Envelope<T> {
    pub fn success(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn failure(error: ErrorBody) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error),
        }
    }

    pub fn from_result(result: HubResult<T>) -> Self {
        match result {
            Ok(data) => Self::success(data),
            Err(error) => Self::failure(error.body()),
        }
    }
}
