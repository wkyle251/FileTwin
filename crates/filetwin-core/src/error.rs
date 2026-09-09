use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    UnsupportedSchemaVersion,
    UnknownProfile,
    InvalidVectorFile,
    IoError,
    ResourceBudgetTooSmall,
    SourceChanged,
    UnsupportedFormat,
    UnselectedFamily,
    InvalidText,
    InsufficientContent,
    RuntimeUnavailable,
    ModelIntegrity,
    DecodeFailed,
    WorkerFailed,
    WorkerTimeout,
    OutputClosed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub code: ErrorCode,
    pub stage: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "BoxValue::is_null")]
    pub details: Box<Value>,
}
// A small helper keeps serde's predicate independent of Box's forwarding APIs.
struct BoxValue;
impl BoxValue {
    fn is_null(v: &Value) -> bool {
        v.is_null()
    }
}

impl Error {
    pub fn new(code: ErrorCode, stage: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            stage: stage.into(),
            message: message.into(),
            details: Box::new(Value::Null),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, "validation", message)
    }
    pub fn exit_code(&self) -> i32 {
        match self.code {
            ErrorCode::InvalidRequest
            | ErrorCode::UnsupportedSchemaVersion
            | ErrorCode::InvalidVectorFile
            | ErrorCode::UnknownProfile
            | ErrorCode::ResourceBudgetTooSmall => 2,
            ErrorCode::Cancelled => 130,
            _ => 1,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorCode::IoError, "io", e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        if e.is_io() {
            Self::new(ErrorCode::IoError, "json_io", e.to_string())
        } else {
            Self::invalid(e.to_string())
        }
    }
}
