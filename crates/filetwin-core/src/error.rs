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
    UnsupportedProvider,
    UnsupportedCapability,
    ExperimentalProfileRequired,
    UnknownProfile,
    ThresholdRequired,
    CacheBusy,
    EngineBusy,
    NotFound,
    InvalidCursor,
    ResultsNotReady,
    DatabaseVersionUnsupported,
    DatabaseCorrupt,
    StorageError,
    IoError,
    ResourceBudgetTooSmall,
    BudgetExhausted,
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
    StrictConsistencyUnavailable,
    ResultExpired,
    RecordTooLarge,
    OutputClosed,
    Cancelled,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Error {
    pub code: ErrorCode,
    pub stage: String,
    pub source_id: Option<String>,
    pub file_id: Option<String>,
    pub retryable: bool,
    pub fatal: bool,
    pub message: String,
    pub details: Box<Value>,
}

impl Error {
    pub fn new(code: ErrorCode, stage: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            stage: stage.into(),
            source_id: None,
            file_id: None,
            retryable: false,
            fatal: true,
            message: message.into(),
            details: Box::new(Value::Null),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, "validation", message)
    }

    pub fn for_file(mut self, source: &str, file: Option<&str>) -> Self {
        self.source_id = Some(source.into());
        self.file_id = file.map(str::to_owned);
        self.fatal = false;
        self
    }

    pub fn exit_code(&self) -> i32 {
        use ErrorCode::*;
        match self.code {
            InvalidRequest
            | UnsupportedSchemaVersion
            | UnsupportedProvider
            | UnsupportedCapability
            | ExperimentalProfileRequired
            | UnknownProfile
            | ThresholdRequired
            | InvalidCursor
            | ResourceBudgetTooSmall => 2,
            CacheBusy | EngineBusy => 4,
            BudgetExhausted => 3,
            Cancelled => 130,
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

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::new(ErrorCode::StorageError, "storage", e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::invalid(e.to_string())
    }
}
