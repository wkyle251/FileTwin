use crate::{
    Error, ErrorCode, Result,
    api::*,
    local::{self, Revision},
    native, profile,
    scan::detect_format,
    vector_file, worker_protocol as protocol,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// An immutable configuration, with no catalog, owner lock or background job.
/// Each encode call owns its temporary worker pool and returns the vectors.
pub struct Encoder {
    config: EncoderConfig,
}
impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        if [&config.model_dir, &config.temp_dir]
            .iter()
            .any(|p| !p.is_absolute())
        {
            return Err(Error::invalid(
                "Model and temporary directories must be absolute",
            ));
        }
        if !config.temp_dir.is_dir() {
            return Err(Error::invalid(
                "Temporary base directory must already exist",
            ));
        }
        if !(1..=64).contains(&config.workers)
            || !(1..=64).contains(&config.runtime.inference_threads)
            || config.runtime.cuda_device_id < 0
            || config.memory_bytes == 0
            || config.staging_bytes == 0
        {
            return Err(Error::invalid(
                "Workers/threads must be 1..64, CUDA device nonnegative, and resource allowances positive",
            ));
        }
        if config
            .runtime
            .cuda_library_dirs
            .iter()
            .any(|p| !p.is_absolute() || !p.is_dir())
        {
            return Err(Error::invalid(
                "CUDA library directories must be absolute existing directories",
            ));
        }
        Ok(Self { config })
    }

    /// Blocking encoding with callback progress. The host may run this on its
    /// own thread and share the cancellation token. No host signals are changed.
    pub fn encode(
        &self,
        request: &EncodeRequest,
        cancel: &CancellationToken,
        mut progress: impl FnMut(&Progress),
    ) -> Result<VectorFile> {
        let root = local::resolve_root(&request.directory)?;
        if !local::open_secure(&root)?.metadata()?.is_dir() {
            return Err(Error::invalid("Input must be a directory"));
        }
        let mut excluded = Vec::new();
        let mut cache = HashMap::new();
        if let Some(path) = &request.vectors_file {
            let path = local::resolve_root(path)?;
            let previous = vector_file::read_vectors(&path)?;
            for file in previous
                .files
                .into_iter()
                .filter(|f| f.state == FileState::Ready)
            {
                cache.insert(
                    (
                        file.file_id.clone().unwrap(),
                        file.profile_id.clone().unwrap(),
                    ),
                    file,
                );
            }
            excluded.push(path);
        }
        if let Some(path) = &request.output_file {
            let path = local::resolve_root(path)?;
            vector_file::validate_output(&path)?;
            excluded.push(path);
        }
        // No persistent per-run state: source copies and CoreML compilation
        // artifacts belong to this private directory and are removed on exit.
        let started = Instant::now();
        let temporary = tempfile::Builder::new()
            .prefix("filetwin-")
            .tempdir_in(&self.config.temp_dir)?;
        let result = (|| -> Result<VectorFile> {
            let mut config = self.config.clone();
            config.temp_dir = std::fs::canonicalize(temporary.path())?;
            excluded.push(config.temp_dir.clone());
            if config.model_dir.exists() {
                excluded.push(std::fs::canonicalize(&config.model_dir)?);
            }
            if excluded.contains(&root) {
                return Err(Error::invalid(
                    "Input directory cannot be a model, vector-output or staging artifact",
                ));
            }
            let profiles = profile::experimental_profiles_for(config.backend)
                .into_iter()
                .map(|p| (p.family, p.profile_id))
                .collect();
            let pool = native::Pool::new(&config);
            let workers = pool.capacity();
            let mut run = Run {
                config,
                root: root.clone(),
                excluded,
                cache,
                profiles,
                counts: Counts::default(),
                records: Vec::new(),
                pending: (0..workers).map(|_| None).collect(),
                pool,
                cancel,
                progress: &mut progress,
                started,
                last_progress: Instant::now(),
                stage: Stage::Discovering,
                discovery_complete: true,
            };
            run.emit(true);
            let result = run.process();
            let cancelled = match result {
                Err(e) if e.code == ErrorCode::Cancelled => true,
                Err(e) => return Err(e),
                Ok(()) => false,
            };
            // Destroy native children before releasing the private staged sources.
            drop(run.pool);
            run.records
                .sort_by_key(|r| r.path.to_path_buf().expect("Generated path"));
            let summary = Summary {
                counts: run.counts.clone(),
                elapsed_seconds: run.started.elapsed().as_secs_f64(),
                backend: self.config.backend,
                workers,
                cancelled,
            };
            Ok(VectorFile {
                format: VECTOR_FILE_FORMAT.into(),
                schema_version: SCHEMA_VERSION,
                directory: FilePath::from_path(&root),
                complete: !cancelled && run.discovery_complete,
                files: run.records,
                summary,
            })
        })();
        // Drop all workers/files before removing the invocation directory. Check
        // deletion explicitly: TempDir's Drop would silently ignore failures.
        temporary.close().map_err(|e| {
            let mut error = Error::new(
                ErrorCode::IoError,
                "cleanup",
                format!("Cannot remove temporary processing data: {e}"),
            );
            if let Err(cause) = &result {
                error.details["operation_error"] =
                    serde_json::to_value(cause).expect("Serializable error");
            }
            error
        })?;
        let mut result = result?;
        result.summary.elapsed_seconds = started.elapsed().as_secs_f64();
        progress(&Progress {
            stage: if result.summary.cancelled {
                Stage::Cancelled
            } else {
                Stage::Completed
            },
            counts: result.summary.counts.clone(),
            elapsed_seconds: result.summary.elapsed_seconds,
        });
        Ok(result)
    }
}

struct Pending {
    file: File,
    path: PathBuf,
    revision: Revision,
    record: FileRecord,
}
struct Run<'a> {
    config: EncoderConfig,
    root: PathBuf,
    excluded: Vec<PathBuf>,
    cache: HashMap<(String, String), FileRecord>,
    profiles: BTreeMap<String, String>,
    counts: Counts,
    records: Vec<FileRecord>,
    pool: native::Pool,
    pending: Vec<Option<Pending>>,
    cancel: &'a CancellationToken,
    progress: &'a mut dyn FnMut(&Progress),
    started: Instant,
    last_progress: Instant,
    stage: Stage,
    discovery_complete: bool,
}

impl Run<'_> {
    fn emit(&mut self, force: bool) {
        if force || self.last_progress.elapsed() >= Duration::from_millis(250) {
            (self.progress)(&Progress {
                stage: self.stage,
                counts: self.counts.clone(),
                elapsed_seconds: self.started.elapsed().as_secs_f64(),
            });
            self.last_progress = Instant::now();
        }
    }
    fn check(&mut self) -> Result<()> {
        self.emit(false);
        self.cancel.check()
    }

    fn blank(&self, path: &Path) -> FileRecord {
        FileRecord {
            path: FilePath::from_path(path.strip_prefix(&self.root).expect("Child path")),
            file_id: None,
            bytes: None,
            state: FileState::Failed,
            family: None,
            profile_id: None,
            vector: None,
            vector_sha256: None,
            reused: false,
            error: None,
            extraction: None,
        }
    }
    fn failed(&mut self, path: &Path, error: Error, skipped: bool) {
        let mut record = self.blank(path);
        record.state = if skipped {
            FileState::Skipped
        } else {
            FileState::Failed
        };
        record.error = Some(error);
        self.record(record);
    }
    fn record(&mut self, record: FileRecord) {
        self.counts.files_processed += 1;
        match record.state {
            FileState::Ready => {
                self.counts.files_ready += 1;
                if record.reused {
                    self.counts.cache_hits += 1;
                } else {
                    self.counts.vectors_encoded += 1;
                }
                self.cache.insert(
                    (
                        record.file_id.clone().unwrap(),
                        record.profile_id.clone().unwrap(),
                    ),
                    record.clone(),
                );
            }
            FileState::Skipped => self.counts.files_skipped += 1,
            _ => self.counts.files_failed += 1,
        }
        self.records.push(record);
        self.emit(false);
    }

    fn process(&mut self) -> Result<()> {
        let mut directories = vec![self.root.clone()];
        let mut files = Vec::new();
        let mut visited = HashSet::new();
        while let Some(path) = directories.pop() {
            self.check()?;
            let listing = (|| -> Result<local::SecureDir> {
                let m = local::open_secure(&path)?.metadata()?;
                let r = Revision::of(&m);
                if !visited.insert((r.device, r.inode)) {
                    return Err(Error::invalid("Directory was already visited"));
                }
                local::SecureDir::open(&path)
            })();
            let listing = match listing {
                Ok(v) => v,
                Err(e) if path != self.root => {
                    self.discovery_complete = false;
                    self.counts.files_discovered += 1;
                    self.failed(&path, e, false);
                    continue;
                }
                Err(e) => return Err(e),
            };
            for entry in listing {
                self.check()?;
                let child = path.join(entry?);
                if self.excluded.iter().any(|p| child.starts_with(p)) {
                    continue;
                }
                let meta = std::fs::symlink_metadata(&child);
                match meta {
                    Ok(m) if m.is_dir() => directories.push(child),
                    Ok(m) if m.is_file() => {
                        self.counts.files_discovered += 1;
                        files.push(child);
                    }
                    Ok(_) => {
                        self.counts.files_discovered += 1;
                        self.failed(
                            &child,
                            Error::new(
                                ErrorCode::UnsupportedFormat,
                                "discovery",
                                "Symlinks and nonregular entries are not followed",
                            ),
                            true,
                        );
                    }
                    Err(e) => {
                        self.counts.files_discovered += 1;
                        self.failed(&child, e.into(), false);
                    }
                }
            }
        }
        files.sort();
        self.counts.files_total = Some(self.counts.files_discovered);
        self.stage = Stage::Encoding;
        self.emit(true);
        for path in files {
            self.check()?;
            self.drain()?;
            if let Err(e) = self.encode_file(&path) {
                if e.code == ErrorCode::Cancelled {
                    return Err(e);
                }
                self.failed(&path, e, false);
            }
        }
        while self.pool.active() > 0 {
            self.wait_one()?;
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        while let Some((slot, outcome)) = self.pool.poll() {
            self.finish(slot, outcome)?;
        }
        Ok(())
    }
    fn wait_one(&mut self) -> Result<()> {
        loop {
            self.check()?;
            if let Some((slot, outcome)) = self.pool.poll() {
                return self.finish(slot, outcome);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn finish(&mut self, slot: usize, outcome: native::Outcome) -> Result<()> {
        self.check()?;
        let context = self.pending[slot].take().expect("Pending source");
        self.finish_record(context, outcome);
        Ok(())
    }
    fn finish_record(&mut self, mut context: Pending, outcome: native::Outcome) {
        if !local::still_current(&context.file, &context.path, &context.revision) {
            context.record.file_id = None;
            context.record.error = Some(Error::new(
                ErrorCode::SourceChanged,
                "validation",
                "Source changed during encoding; vector discarded",
            ));
        } else {
            match outcome {
                Ok(encoded) => {
                    context.record.state = FileState::Ready;
                    context.record.profile_id = Some(self.profiles[&encoded.family].clone());
                    context.record.family = Some(encoded.family);
                    context.record.vector_sha256 =
                        Some(profile::digest_hex(&profile::vector_bytes(&encoded.vector)));
                    context.record.vector = Some(encoded.vector);
                    context.record.extraction = Some(encoded.extraction);
                }
                Err(e) => {
                    if e.code == ErrorCode::UnsupportedFormat {
                        context.record.state = FileState::Unsupported;
                    }
                    context.record.error = Some(e);
                }
            }
        }
        self.record(context.record);
    }

    fn encode_file(&mut self, path: &Path) -> Result<()> {
        let mut file = local::open_secure(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(Error::new(
                ErrorCode::SourceChanged,
                "discovery",
                "Entry is no longer a regular file",
            ));
        }
        let revision = Revision::of(&meta);
        let mut prefix = [0u8; 512];
        let n = file.read(&mut prefix)?;
        self.counts.bytes_read += n as u64;
        file.seek(SeekFrom::Start(0))?;
        let (family, format) = detect_format(&prefix[..n], path);
        let is_native = format != "utf8" && family != "unknown";
        let mut staged = if is_native {
            let reservation = revision
                .bytes
                .saturating_add(2 * protocol::MAX_RESPONSE as u64);
            while self.pool.active() == self.pool.capacity()
                || (self.pool.active() > 0
                    && self.pool.staged_bytes().saturating_add(reservation)
                        > self.config.staging_bytes)
            {
                self.wait_one()?;
            }
            Some(tempfile::NamedTempFile::new_in(&self.config.temp_dir)?)
        } else {
            None
        };
        let source_limit = self.config.staging_bytes.saturating_sub(
            self.pool
                .staged_bytes()
                .saturating_add(2 * protocol::MAX_RESPONSE as u64),
        );
        // Hash every readable original. An oversized native file still gets a
        // content identifier even though its source copy cannot be admitted.
        let mut stage_overflow = staged.is_some() && revision.bytes > source_limit;
        if stage_overflow {
            staged = None;
        }
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            self.check()?;
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            bytes += n as u64;
            self.counts.bytes_read += n as u64;
            self.counts.bytes_hashed += n as u64;
            if staged.is_some() && bytes > source_limit {
                staged = None;
                stage_overflow = true;
            }
            if let Some(copy) = &mut staged {
                copy.write_all(&buffer[..n])?;
            }
        }
        let mut record = self.blank(path);
        record.file_id = Some(format!("sha256:{:x}", hash.finalize()));
        record.bytes = Some(bytes);
        record.family = (family != "unknown").then(|| family.into());
        let mut context = Pending {
            file,
            path: path.to_owned(),
            revision,
            record,
        };
        if !local::still_current(&context.file, path, &context.revision) {
            self.finish_record(
                context,
                Err(Error::new(
                    ErrorCode::SourceChanged,
                    "validation",
                    "Source changed while hashing",
                )),
            );
            return Ok(());
        }
        // Wait for an identical input already being encoded in this call.
        while self
            .pending
            .iter()
            .flatten()
            .any(|p| p.record.file_id == context.record.file_id)
        {
            self.wait_one()?;
        }
        let candidates = if format == "media" {
            vec!["audio", "video"]
        } else {
            vec![family]
        };
        let hit = candidates.into_iter().find_map(|family| {
            self.profiles
                .get(family)
                .and_then(|id| {
                    self.cache
                        .get(&(context.record.file_id.clone().unwrap(), id.clone()))
                })
                .cloned()
        });
        if let Some(mut hit) = hit {
            if !local::still_current(&context.file, path, &context.revision) {
                self.finish_record(
                    context,
                    Err(Error::new(
                        ErrorCode::SourceChanged,
                        "validation",
                        "Source changed before vector reuse",
                    )),
                );
            } else {
                hit.path = context.record.path;
                hit.bytes = context.record.bytes;
                hit.reused = true;
                self.record(hit);
            }
            return Ok(());
        }
        if family == "unknown" || stage_overflow {
            let e = if stage_overflow {
                Error::new(
                    ErrorCode::ResourceBudgetTooSmall,
                    "staging",
                    "File exceeds the private staging allowance",
                )
            } else {
                Error::new(
                    ErrorCode::UnsupportedFormat,
                    "encoding",
                    "No content reader for this format",
                )
            };
            self.finish_record(context, Err(e));
            return Ok(());
        }
        if format == "utf8" {
            context.file.seek(SeekFrom::Start(0))?;
            let result = crate::text::encode(&mut context.file, false, &mut || self.check());
            let encoded = match result {
                Ok(e) => {
                    self.counts.bytes_read += e.bytes_read;
                    Ok(protocol::Encoded {
                        family: "text".into(),
                        format: "text/plain; charset=utf-8".into(),
                        vector: e.vector,
                        extraction: json!({"coverage":"complete_source_text","characters":e.characters}),
                    })
                }
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(e) => {
                    self.counts.bytes_read += e.details["bytes_read"].as_u64().unwrap_or(0);
                    Err(e)
                }
            };
            self.finish_record(context, encoded);
            return Ok(());
        }
        if let Err(e) = native::validate_runtime(&self.config, family, format) {
            self.finish_record(context, Err(e));
            return Ok(());
        }
        let id = if format == "media" {
            self.profiles["video"].clone()
        } else {
            self.profiles[family].clone()
        };
        let copy = staged.as_mut().expect("Admitted source copy");
        copy.flush()?;
        let request = protocol::Request {
            version: protocol::VERSION,
            path: copy.path().to_owned(),
            format: format.into(),
            profile_id: id,
            profiles: self.profiles.clone(),
            model_dir: self.config.model_dir.clone(),
            runtime: self.config.runtime.clone(),
            memory_bytes: self.config.memory_bytes,
            parent_pid: std::process::id(),
            inference_cache_dir: Some(self.config.temp_dir.join("inference-cache")),
        };
        let slot = self.pool.submit(native::Prepared {
            request,
            bytes,
            _staged: staged.unwrap(),
        });
        self.pending[slot] = Some(context);
        Ok(())
    }
}
