//! Versioned public input/output types. Optional request fields preserve whether
//! callers supplied them, so operation validation runs before defaults resolve.

use crate::{Error, ErrorCode, Result, profile};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, os::unix::ffi::OsStringExt, path::PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub model_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub runtime: RuntimeConfig,
}

/// Explicit paths for native decoding. The library never searches PATH, reads
/// environment configuration, downloads models, or installs signal handlers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub worker_path: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
    pub ffprobe_path: Option<PathBuf>,
    pub onnxruntime_path: Option<PathBuf>,
    pub pdfium_path: Option<PathBuf>,
}

impl EngineConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            model_dir: data_dir.join("models"),
            temp_dir: data_dir.join("tmp"),
            runtime: RuntimeConfig::default(),
            data_dir,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    #[default]
    Scan,
    Index,
    Compare,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PairScope {
    #[default]
    AllSelected,
    WithinEachSource,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    #[default]
    Reuse,
    Refresh,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Validation {
    #[default]
    Fast,
    Strict,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExactDuplicates {
    #[default]
    ReuseKnown,
    Compute,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawPath {
    pub encoding: String,
    pub base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<RawPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}

impl Source {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        use std::os::unix::ffi::OsStrExt;
        Self {
            provider: "local".into(),
            source_id: None,
            connection_id: None,
            root: path.to_str().map(str::to_owned),
            local_path: path.to_str().is_none().then(|| RawPath {
                encoding: "posix_bytes".into(),
                base64: STANDARD.encode(path.as_os_str().as_bytes()),
            }),
        }
    }

    pub fn path(&self) -> Result<PathBuf> {
        let path = match (&self.root, &self.local_path) {
            (Some(root), None) => PathBuf::from(root),
            (None, Some(raw)) if raw.encoding == "posix_bytes" => {
                let bytes = STANDARD
                    .decode(&raw.base64)
                    .map_err(|_| Error::invalid("Invalid path Base64"))?;
                PathBuf::from(std::ffi::OsString::from_vec(bytes))
            }
            _ => {
                return Err(Error::invalid(
                    "Local sources need exactly one root or posix_bytes local_path",
                ));
            }
        };
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(Error::invalid(
                "Source paths must be absolute and contain no NUL bytes",
            ));
        }
        Ok(path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    #[serde(default)]
    pub include_globs: Vec<String>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default = "yes")]
    pub include_hidden: bool,
    pub min_bytes: Option<u64>,
    pub max_bytes: Option<u64>,
}

fn yes() -> bool {
    true
}

impl Filters {
    pub fn unrestricted() -> Self {
        Self {
            include_hidden: true,
            ..Self::default()
        }
    }
}
impl Default for Filters {
    fn default() -> Self {
        Self {
            include_globs: vec![],
            exclude_globs: vec![],
            extensions: vec![],
            include_hidden: true,
            min_bytes: None,
            max_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Matching {
    #[serde(default = "exact")]
    pub retrieval: String,
    #[serde(default = "all_pairs")]
    pub grouping: String,
    #[serde(default)]
    pub threshold_overrides: BTreeMap<String, f64>,
}
fn exact() -> String {
    "exact".into()
}
fn all_pairs() -> String {
    "all_pairs".into()
}
impl Default for Matching {
    fn default() -> Self {
        Self {
            retrieval: exact(),
            grouping: all_pairs(),
            threshold_overrides: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    #[serde(default)]
    pub mode: CacheMode,
    #[serde(default)]
    pub validation: Validation,
    #[serde(default)]
    pub import_sidecars: bool,
    #[serde(default)]
    pub export_sidecars: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staging_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_workers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_workers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_bytes_per_second: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_time_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default)]
    pub operation: Operation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<Source>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub families: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Filters>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair_scope: Option<PairScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matching: Option<Matching>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<Cache>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_duplicates: Option<ExactDuplicates>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
}

impl Default for JobRequest {
    fn default() -> Self {
        Self {
            schema_version: 1,
            request_id: None,
            operation: Operation::Scan,
            sources: None,
            snapshot_id: None,
            recursive: None,
            families: None,
            profiles: None,
            filters: None,
            pair_scope: None,
            matching: None,
            cache: None,
            exact_duplicates: None,
            limits: None,
        }
    }
}

impl JobRequest {
    /// Parse bounded versioned JSON, rejecting duplicate/unknown keys and
    /// explicitly present fields that do not belong to the selected operation.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let value: Value = parse_json(bytes)?;
        validate_request_shape(&value)?;
        Ok(serde_json::from_value(value)?)
    }
    /// Explicitly opt into the experimental text profile and an uncalibrated cutoff.
    pub fn text_scan(paths: impl IntoIterator<Item = PathBuf>, threshold: f64) -> Self {
        let p = profile::text_profile();
        Self {
            sources: Some(paths.into_iter().map(Source::local).collect()),
            profiles: Some(BTreeMap::from([("text".into(), p.profile_id.clone())])),
            matching: Some(Matching {
                threshold_overrides: BTreeMap::from([(p.profile_id, threshold)]),
                ..Matching::default()
            }),
            ..Self::default()
        }
    }

    pub fn text_index(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut r = Self::text_scan(paths, 0.0);
        r.operation = Operation::Index;
        r.matching = None;
        r
    }

    /// Opt into the current experimental profile for each of the four families.
    /// The caller chooses the cutoff; this is not a calibrated default.
    pub fn experimental_scan(paths: impl IntoIterator<Item = PathBuf>, threshold: f64) -> Self {
        let profiles = profile::experimental_profiles();
        Self {
            sources: Some(paths.into_iter().map(Source::local).collect()),
            profiles: Some(
                profiles
                    .iter()
                    .map(|p| (p.family.clone(), p.profile_id.clone()))
                    .collect(),
            ),
            matching: Some(Matching {
                threshold_overrides: profiles
                    .into_iter()
                    .map(|p| (p.profile_id, threshold))
                    .collect(),
                ..Matching::default()
            }),
            ..Self::default()
        }
    }

    pub fn experimental_index(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut request = Self::experimental_scan(paths, 0.0);
        request.operation = Operation::Index;
        request.matching = None;
        request
    }

    pub(crate) fn resolve(mut self) -> Result<Self> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::new(
                ErrorCode::UnsupportedSchemaVersion,
                "validation",
                "Only schema_version 1 is supported",
            ));
        }
        if self.operation == Operation::Compare {
            if self.sources.is_some()
                || self.profiles.is_some()
                || self.filters.is_some()
                || self.families.is_some()
                || self.cache.is_some()
                || self.exact_duplicates.is_some()
                || self.recursive.is_some()
                || self.snapshot_id.as_ref().is_none_or(String::is_empty)
            {
                return Err(Error::invalid(
                    "compare requires snapshot_id and rejects discovery/encoding settings",
                ));
            }
        } else {
            if self.snapshot_id.is_some() {
                return Err(Error::invalid("snapshot_id is only valid for compare"));
            }
            let sources = self
                .sources
                .as_mut()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::invalid("At least one source is required"))?;
            let mut ids = std::collections::BTreeSet::new();
            for (i, s) in sources.iter_mut().enumerate() {
                if s.provider != "local" {
                    return Err(Error::new(
                        ErrorCode::UnsupportedProvider,
                        "validation",
                        "Only local sources are implemented",
                    ));
                }
                if s.connection_id.is_some() {
                    return Err(Error::invalid("Local sources do not use connection_id"));
                }
                s.path()?;
                let id = s
                    .source_id
                    .get_or_insert_with(|| format!("source_{}", i + 1));
                if id.is_empty() || id.len() > 256 || !ids.insert(id.clone()) {
                    return Err(Error::invalid(
                        "Source IDs must be distinct and contain 1..256 UTF-8 bytes",
                    ));
                }
            }
            let selected = self
                .profiles
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::ExperimentalProfileRequired,
                        "validation",
                        "Explicitly select experimental profiles; the CLI provides --experimental",
                    )
                })?;
            profile::validate_selection(selected)?;
            let families: Vec<_> = selected.keys().cloned().collect();
            if let Some(requested) = &self.families {
                let mut requested = requested.clone();
                requested.sort();
                if requested != families {
                    return Err(Error::invalid(
                        "families must contain exactly the selected profile families, without duplicates",
                    ));
                }
            }
            self.families = Some(families);
            self.recursive.get_or_insert(true);
            let f = self.filters.get_or_insert_with(Filters::unrestricted);
            if matches!((f.min_bytes,f.max_bytes),(Some(a),Some(b)) if a>b) {
                return Err(Error::invalid("min_bytes must not exceed max_bytes"));
            }
            if f.extensions
                .iter()
                .any(|s| s.is_empty() || s.starts_with('.') || s.contains('/'))
            {
                return Err(Error::invalid(
                    "Extensions must be nonempty names without a leading dot or slash",
                ));
            }
            for glob in f.include_globs.iter().chain(&f.exclude_globs) {
                crate::local::compile_glob(glob)?;
            }
            let cache = self.cache.get_or_insert_with(Cache::default);
            if cache.import_sidecars || cache.export_sidecars {
                return Err(Error::new(
                    ErrorCode::UnsupportedCapability,
                    "validation",
                    "Sidecar import/export is not implemented in this preview",
                ));
            }
            self.exact_duplicates
                .get_or_insert(ExactDuplicates::ReuseKnown);
        }
        if self.operation == Operation::Index {
            if self.matching.is_some() || self.pair_scope.is_some() {
                return Err(Error::invalid("index rejects matching and pair_scope"));
            }
        } else {
            self.pair_scope.get_or_insert(PairScope::AllSelected);
            let m = self.matching.get_or_insert_with(Matching::default);
            if m.retrieval != "exact" || m.grouping != "all_pairs" {
                return Err(Error::new(
                    ErrorCode::UnsupportedCapability,
                    "validation",
                    "Only exact retrieval and all_pairs grouping are implemented",
                ));
            }
            if m.threshold_overrides
                .values()
                .any(|v| !v.is_finite() || !(-1.0..=1.0).contains(v))
            {
                return Err(Error::invalid(
                    "Thresholds must be finite numbers in [-1, 1]",
                ));
            }
            if self.operation != Operation::Compare {
                self.validate_thresholds(self.profiles.as_ref().expect("Selected profiles"))?;
            }
        }
        let limits = self.limits.get_or_insert_with(Limits::default);
        if self.operation == Operation::Compare
            && (limits.staging_bytes.is_some()
                || limits.io_workers.is_some()
                || limits.inference_workers.is_some()
                || limits.download_bytes.is_some()
                || limits.download_bytes_per_second.is_some())
        {
            return Err(Error::invalid(
                "compare accepts only memory, result, and wall-time limits",
            ));
        }
        limits.memory_bytes.get_or_insert(2 * 1024 * 1024 * 1024);
        limits.result_bytes.get_or_insert(1024 * 1024 * 1024);
        if self.operation != Operation::Compare {
            limits.staging_bytes.get_or_insert(10 * 1024 * 1024 * 1024);
            limits.io_workers.get_or_insert(2);
            limits.inference_workers.get_or_insert(1);
        }
        if [
            limits.memory_bytes,
            limits.staging_bytes,
            limits.result_bytes,
            limits.download_bytes_per_second,
            limits.wall_time_seconds,
        ]
        .into_iter()
        .flatten()
        .any(|v| v == 0 || v > i64::MAX as u64)
            || limits.io_workers == Some(0)
            || limits.inference_workers == Some(0)
        {
            return Err(Error::invalid(
                "Budgets and worker counts must be positive; byte budgets must fit signed 64-bit storage",
            ));
        }
        if limits.memory_bytes.unwrap() < 64 * 1024 * 1024 {
            return Err(Error::new(
                ErrorCode::ResourceBudgetTooSmall,
                "validation",
                "The reference engine requires a memory budget of at least 64 MiB",
            ));
        }
        let id = self
            .request_id
            .get_or_insert_with(|| format!("request_{}", uuid::Uuid::new_v4()));
        if id.is_empty() || id.len() > 1024 {
            return Err(Error::invalid(
                "request_id must contain 1..1024 UTF-8 bytes",
            ));
        }
        Ok(self)
    }

    pub(crate) fn validate_thresholds(&self, selected: &BTreeMap<String, String>) -> Result<()> {
        profile::validate_selection(selected)?;
        let Some(m) = &self.matching else {
            return Ok(());
        };
        if m.threshold_overrides.len() != selected.len()
            || selected
                .values()
                .any(|id| !m.threshold_overrides.contains_key(id))
        {
            return Err(Error::new(
                ErrorCode::ThresholdRequired,
                "validation",
                "Provide exactly one threshold for every selected profile (including profiles with no ready files)",
            ));
        }
        Ok(())
    }

    pub(crate) fn threshold(&self, profile_id: &str) -> Result<f64> {
        self.matching
            .as_ref()
            .and_then(|m| m.threshold_overrides.get(profile_id))
            .copied()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::ThresholdRequired,
                    "matching",
                    "Missing profile threshold",
                )
            })
    }
}

/// Presence validation precedes merging defaults and typed deserialization.
pub fn validate_request_shape(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::invalid("Processing request must be one JSON object"))?;
    if !object.contains_key("schema_version") {
        return Err(Error::invalid("Serialized requests require schema_version"));
    }
    let operation = object
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("scan");
    if object
        .get("limits")
        .and_then(Value::as_object)
        .is_some_and(|l| {
            [
                "memory_bytes",
                "staging_bytes",
                "result_bytes",
                "io_workers",
                "inference_workers",
            ]
            .iter()
            .any(|k| l.get(*k).is_some_and(Value::is_null))
        })
    {
        return Err(Error::invalid(
            "Finite budgets and worker counts cannot be null",
        ));
    }
    if operation == "compare" {
        if [
            "sources",
            "families",
            "profiles",
            "filters",
            "cache",
            "recursive",
            "exact_duplicates",
        ]
        .iter()
        .any(|k| object.contains_key(*k))
        {
            return Err(Error::invalid(
                "compare rejects discovery/encoding fields, including explicit nulls",
            ));
        }
        if object
            .get("limits")
            .and_then(Value::as_object)
            .is_some_and(|l| {
                [
                    "staging_bytes",
                    "io_workers",
                    "inference_workers",
                    "download_bytes",
                    "download_bytes_per_second",
                ]
                .iter()
                .any(|k| l.contains_key(*k))
            })
        {
            return Err(Error::invalid(
                "compare accepts only memory, result, and wall-time limits",
            ));
        }
    }
    if operation == "index"
        && ["matching", "pair_scope"]
            .iter()
            .any(|k| object.contains_key(*k))
    {
        return Err(Error::invalid("index rejects matching and pair_scope"));
    }
    if operation != "compare" && object.contains_key("snapshot_id") {
        return Err(Error::invalid("snapshot_id is only valid for compare"));
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Counts {
    pub files_discovered: u64,
    pub files_ready: u64,
    pub files_failed: u64,
    pub files_excluded: u64,
    pub locations: u64,
    pub vectors_encoded: u64,
    pub cache_hits: u64,
    pub bytes_read: u64,
    pub bytes_hashed: u64,
    pub pairs_compared: u64,
    pub similar_pairs: u64,
    pub groups: u64,
    pub exact_pairs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Completeness {
    pub source_coverage: String,
    pub comparison_coverage: String,
    pub retrieval_mode: Option<String>,
    pub limits_reached: Vec<String>,
    pub freshness: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobSummary {
    pub job_id: String,
    pub run_id: Option<String>,
    pub attempt_id: u64,
    pub status: String,
    pub resumable: bool,
    pub snapshot_id: Option<String>,
    pub result_revision: Option<u64>,
    pub scope_summary: Value,
    pub started_at: String,
    pub finished_at: String,
    pub elapsed_seconds: f64,
    pub result_bytes_used: u64,
    pub counts: Counts,
    pub completeness: Completeness,
    pub error: Option<Error>,
}

impl JobSummary {
    pub fn exit_code(&self) -> i32 {
        if self.status == "cancelled" {
            return self.error.as_ref().map_or(130, Error::exit_code);
        }
        if self.status == "failed" {
            return self.error.as_ref().map_or(1, Error::exit_code);
        }
        if self.status == "checkpointed"
            || self.completeness.source_coverage == "partial"
            || self.completeness.comparison_coverage == "partial"
        {
            3
        } else {
            0
        }
    }
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    Accepted {
        job_id: String,
        run_id: Option<String>,
        data: Value,
    },
    Progress {
        job_id: String,
        run_id: Option<String>,
        data: Value,
    },
    Error {
        job_id: String,
        run_id: Option<String>,
        error: Error,
    },
    Summary(Box<JobSummary>),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    pub schema_version: u32,
    pub invocation_id: String,
    pub request_id: String,
    pub sequence: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub job_id: Option<String>,
    pub run_id: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusQuery {
    pub schema_version: u32,
    pub job_id: String,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultsQuery {
    pub schema_version: u32,
    pub request_id: Option<String>,
    pub run_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub kind: String,
    pub group_id: Option<String>,
    pub file_id: Option<String>,
    pub result_revision: Option<u64>,
    pub cursor: Option<String>,
    pub page_size: Option<u32>,
}

impl Default for ResultsQuery {
    fn default() -> Self {
        Self {
            schema_version: 1,
            request_id: None,
            run_id: None,
            snapshot_id: None,
            kind: "files".into(),
            group_id: None,
            file_id: None,
            result_revision: None,
            cursor: None,
            page_size: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResultPage {
    pub snapshot_id: String,
    pub run_id: Option<String>,
    pub result_revision: u64,
    pub kind: String,
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub schema_version: u32,
    pub request_id: Option<String>,
    pub run_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub result_revision: Option<u64>,
    pub format: String,
    pub directory: PathBuf,
}

/// Parse bounded input without allowing duplicate object keys at any depth.
pub fn parse_json<T: de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(Error::invalid("JSON input exceeds 8 MiB"));
    }
    let value: UniqueValue = serde_json::from_slice(bytes)?;
    Ok(serde_json::from_value(value.0)?)
}

struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: de::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON with unique object keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|v| UniqueValue(Value::Number(v)))
                    .ok_or_else(|| E::custom("Non-finite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut a = Vec::new();
                while let Some(v) = seq.next_element::<UniqueValue>()? {
                    a.push(v.0);
                }
                Ok(UniqueValue(Value::Array(a)))
            }
            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut obj = serde_json::Map::new();
                while let Some((k, v)) = map.next_entry::<String, UniqueValue>()? {
                    if obj.insert(k.clone(), v.0).is_some() {
                        return Err(de::Error::custom(format!("Duplicate JSON key: {k}")));
                    }
                }
                Ok(UniqueValue(Value::Object(obj)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
