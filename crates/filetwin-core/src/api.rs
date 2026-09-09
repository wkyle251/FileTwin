//! Small typed interface shared by the CLI, Rust hosts and portable vector files.
use crate::{Error, ErrorCode, Result, profile::Backend};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const VECTOR_FILE_FORMAT: &str = "filetwin-vectors";
pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_VECTOR_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// The host provides native paths. The library never searches PATH or downloads assets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub worker_path: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
    pub ffprobe_path: Option<PathBuf>,
    pub onnxruntime_path: Option<PathBuf>,
    pub pdfium_path: Option<PathBuf>,
    pub inference_threads: u32,
    pub cuda_device_id: i32,
    pub cuda_library_dirs: Vec<PathBuf>,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_path: None,
            ffmpeg_path: None,
            ffprobe_path: None,
            onnxruntime_path: None,
            pdfium_path: None,
            inference_threads: 2,
            cuda_device_id: 0,
            cuda_library_dirs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub model_dir: PathBuf,
    /// Existing parent for disposable processing data; never created by encode.
    pub temp_dir: PathBuf,
    pub runtime: RuntimeConfig,
    pub backend: Backend,
    pub workers: usize,
    /// Shared native admission allowance, not a hard total RSS/GPU memory ceiling.
    pub memory_bytes: u64,
    /// Maximum simultaneous private source copies, including protocol reserves.
    pub staging_bytes: u64,
}
impl EncoderConfig {
    pub fn new(model_dir: impl Into<PathBuf>, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            model_dir: model_dir.into(),
            temp_dir: temp_dir.into(),
            runtime: RuntimeConfig::default(),
            backend: Backend::Cpu,
            workers: 2,
            memory_bytes: 2 << 30,
            staging_bytes: 10 << 30,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodeRequest {
    pub directory: PathBuf,
    pub vectors_file: Option<PathBuf>,
    /// Reserve an output pathname so it is not included as a source. Encoding
    /// itself never writes the vector file; call write_vectors afterwards.
    pub output_file: Option<PathBuf>,
}
impl EncodeRequest {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            vectors_file: None,
            output_file: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::new(
                ErrorCode::Cancelled,
                "encoding",
                "Encoding cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

/// Normal paths are strings. Non-UTF-8 POSIX filenames remain lossless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum FilePath {
    Utf8(String),
    Bytes { encoding: String, base64: String },
}
impl FilePath {
    pub fn from_path(path: &Path) -> Self {
        match path.to_str() {
            Some(s) => Self::Utf8(s.into()),
            None => Self::Bytes {
                encoding: "posix_bytes".into(),
                base64: STANDARD.encode(path.as_os_str().as_bytes()),
            },
        }
    }
    pub fn to_path_buf(&self) -> Result<PathBuf> {
        let bytes = match self {
            Self::Utf8(s) => s.as_bytes().to_vec(),
            Self::Bytes { encoding, base64 } if encoding == "posix_bytes" => STANDARD
                .decode(base64)
                .map_err(|_| Error::invalid("Invalid path Base64"))?,
            _ => return Err(Error::invalid("Unknown path encoding")),
        };
        if bytes.contains(&0) {
            return Err(Error::invalid("NUL in path"));
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    Ready,
    Failed,
    Unsupported,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    /// Relative to VectorFile.directory. Paths are labels, never cache lookup keys.
    pub path: FilePath,
    /// SHA-256 of complete original bytes; identical copies share this identifier.
    pub file_id: Option<String>,
    pub bytes: Option<u64>,
    pub state: FileState,
    pub family: Option<String>,
    pub profile_id: Option<String>,
    pub vector: Option<Vec<f32>>,
    pub vector_sha256: Option<String>,
    pub reused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extraction: Option<Value>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub files_discovered: u64,
    pub files_processed: u64,
    pub files_total: Option<u64>,
    pub files_ready: u64,
    pub files_failed: u64,
    pub files_skipped: u64,
    pub vectors_encoded: u64,
    pub cache_hits: u64,
    pub bytes_read: u64,
    pub bytes_hashed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Discovering,
    Encoding,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub stage: Stage,
    pub counts: Counts,
    pub elapsed_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub counts: Counts,
    pub elapsed_seconds: f64,
    pub backend: Backend,
    pub workers: usize,
    pub cancelled: bool,
}

/// This is both the returned result and the optional cache input for another run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VectorFile {
    pub format: String,
    pub schema_version: u32,
    pub directory: FilePath,
    /// False on cancellation or an incomplete directory listing. Individual
    /// decoding failures are represented in files even when traversal completes.
    pub complete: bool,
    pub files: Vec<FileRecord>,
    pub summary: Summary,
}

impl VectorFile {
    pub fn exit_code(&self) -> i32 {
        if self.summary.cancelled {
            130
        } else if !self.complete
            || self.summary.counts.files_failed > 0
            || self.summary.counts.files_skipped > 0
        {
            3
        } else {
            0
        }
    }
}
