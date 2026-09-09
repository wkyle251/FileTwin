use crate::{Error, ErrorCode, Result, api::EncoderConfig, profile, worker_protocol as protocol};
use serde_json::json;
use std::{
    io::{Read, Write},
    os::unix::{io::AsRawFd, process::CommandExt},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) struct Prepared {
    pub request: protocol::Request,
    pub bytes: u64,
    pub _staged: tempfile::NamedTempFile,
}

fn validate_encoded(
    request: &protocol::Request,
    encoded: protocol::Encoded,
) -> Result<protocol::Encoded> {
    let id = if request.format == "media" {
        if !matches!(encoded.family.as_str(), "audio" | "video") {
            return Err(Error::new(
                ErrorCode::WorkerFailed,
                "worker",
                "Unexpected media family",
            ));
        }
        request.profiles.get(&encoded.family).ok_or_else(|| {
            Error::new(
                ErrorCode::WorkerFailed,
                "worker",
                "Unrequested media family",
            )
        })?
    } else {
        &request.profile_id
    };
    let p = profile::find(id)?;
    if encoded.family != p.family || encoded.vector.len() != p.dimensions {
        return Err(Error::new(
            ErrorCode::WorkerFailed,
            "worker",
            "Worker returned an incompatible representation",
        ));
    }
    profile::decode_vector(&profile::vector_bytes(&encoded.vector), p.dimensions).map_err(
        |_| {
            Error::new(
                ErrorCode::WorkerFailed,
                "worker",
                "Worker returned an invalid vector",
            )
        },
    )?;
    Ok(encoded)
}

pub(crate) fn validate_runtime(config: &EncoderConfig, family: &str, format: &str) -> Result<()> {
    let minimum = if matches!(family, "image" | "video") {
        512 << 20
    } else {
        128 << 20
    };
    if config.memory_bytes / (worker_count(config) as u64) < minimum {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "runtime",
            "Native memory allowance is too small for this reader",
        ));
    }
    let mut paths = vec![("filetwin-worker", &config.runtime.worker_path)];
    if format != "media" && (family == "image" || family == "video") {
        paths.push(("ONNX Runtime", &config.runtime.onnxruntime_path));
    }
    if matches!(format, "media" | "heif_avif") || family == "audio" || family == "video" {
        paths.push(("FFmpeg", &config.runtime.ffmpeg_path));
        paths.push(("ffprobe", &config.runtime.ffprobe_path));
    }
    if format == "pdf" {
        paths.push(("PDFium", &config.runtime.pdfium_path));
    }
    for (name, path) in paths {
        if path
            .as_ref()
            .is_none_or(|p| !p.is_absolute() || !p.is_file())
        {
            return Err(Error::new(
                ErrorCode::RuntimeUnavailable,
                "runtime",
                format!(
                    "Configure an absolute existing {name} path in EncoderConfig.runtime; see README runtime setup"
                ),
            ));
        }
    }
    if format != "media"
        && (family == "image" || family == "video")
        && !config.model_dir.join(profile::SSCD_MODEL_FILE).is_file()
    {
        return Err(Error::new(
            ErrorCode::RuntimeUnavailable,
            "runtime",
            "SSCD model missing; run scripts/setup-native.py explicitly (processing never downloads models)",
        ));
    }
    Ok(())
}

struct ProcessGroup(Child);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // SAFETY: a positive child PID is converted to its private process-group
        // ID. The child was spawned with process_group(0); no host group is used.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

/// Bound concurrency by the requested count and native memory allowance.
pub(crate) fn worker_count(config: &EncoderConfig) -> usize {
    config
        .workers
        .min((config.memory_bytes / (512 << 20)).max(1) as usize)
        .clamp(1, 64)
}

pub(crate) struct Pool {
    config: EncoderConfig,
    slots: Vec<Slot>,
    memory_per_worker: u64,
}

struct Slot {
    // Drop the process group before deleting a still-active staged source.
    process: Option<Process>,
    prepared: Option<Prepared>,
    error: Option<Error>,
}

pub(crate) type Outcome = Result<protocol::Encoded>;

impl Pool {
    pub fn new(config: &EncoderConfig) -> Self {
        let n = worker_count(config);
        Self {
            config: config.clone(),
            slots: (0..n)
                .map(|_| Slot {
                    process: None,
                    prepared: None,
                    error: None,
                })
                .collect(),
            memory_per_worker: config.memory_bytes / n as u64,
        }
    }
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
    pub fn active(&self) -> usize {
        self.slots.iter().filter(|s| s.prepared.is_some()).count()
    }
    pub fn staged_bytes(&self) -> u64 {
        self.slots
            .iter()
            .filter_map(|s| s.prepared.as_ref())
            .map(|p| p.bytes + 2 * protocol::MAX_RESPONSE as u64)
            .sum()
    }
    pub fn submit(&mut self, mut prepared: Prepared) -> usize {
        let i = self
            .slots
            .iter()
            .position(|s| s.prepared.is_none())
            .expect("Admitted native slot");
        prepared.request.memory_bytes = self.memory_per_worker;
        let slot = &mut self.slots[i];
        let result = (|| {
            if slot.process.is_none() {
                slot.process = Some(Process::spawn(&self.config, &prepared.request)?);
            }
            slot.process.as_mut().unwrap().begin(&prepared.request)
        })();
        slot.error = result.err();
        slot.prepared = Some(prepared);
        i
    }
    pub fn poll(&mut self) -> Option<(usize, Outcome)> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.prepared.is_none() {
                continue;
            }
            let result = if let Some(error) = slot.error.take() {
                Some(Err(error))
            } else {
                slot.process.as_mut().unwrap().poll()
            };
            if let Some(result) = result {
                let prepared = slot.prepared.take().unwrap();
                let result = result.and_then(|e| validate_encoded(&prepared.request, e));
                if result.is_err() {
                    slot.process = None;
                }
                return Some((i, result));
            }
        }
        None
    }
}

struct Process {
    child: ProcessGroup,
    input: std::process::ChildStdin,
    output: std::process::ChildStdout,
    errors: std::process::ChildStderr,
    outgoing: Vec<u8>,
    written: usize,
    response: Vec<u8>,
    diagnostics: Vec<u8>,
    started: Instant,
    _directory: tempfile::TempDir,
}

fn worker_error(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::WorkerFailed, "worker", message)
}

fn nonblocking(fd: std::os::fd::RawFd) -> Result<()> {
    // SAFETY: fcntl modifies flags on our owned pipe, without transferring ownership.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

impl Process {
    fn spawn(config: &EncoderConfig, request: &protocol::Request) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("filetwin-session-")
            .tempdir_in(&config.temp_dir)?;
        let mut command = Command::new(
            config
                .runtime
                .worker_path
                .as_ref()
                .expect("Validated worker"),
        );
        command
            .arg("--serve")
            .env_clear()
            .env("LC_ALL", "C")
            .env("OMP_NUM_THREADS", "1")
            .current_dir(directory.path())
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "linux")]
        if !request.runtime.cuda_library_dirs.is_empty() {
            let search = std::env::join_paths(&request.runtime.cuda_library_dirs)
                .map_err(|_| Error::invalid("Invalid CUDA library search directories"))?;
            command.env("LD_LIBRARY_PATH", search);
        }
        let memory = request.memory_bytes;
        let uses_cuda = request
            .profiles
            .values()
            .any(|id| profile::inference_backend(id).ok() == Some(profile::Backend::Cuda));
        // SAFETY: this hook only calls async-signal-safe setrlimit; no allocation
        // or host state changes occur after fork. GPU drivers reserve virtual
        // address space independent of physical memory, so cannot use RLIMIT_AS.
        unsafe {
            command.pre_exec(move || {
                let no_core = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::setrlimit(libc::RLIMIT_CORE, &no_core) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                if !uses_cuda {
                    let cap = libc::rlimit {
                        rlim_cur: memory,
                        rlim_max: memory,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &cap) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                #[cfg(not(target_os = "linux"))]
                let _ = (memory, uses_cuda);
                Ok(())
            });
        }
        let mut child = ProcessGroup(command.spawn().map_err(|e| worker_error(e.to_string()))?);
        let input = child.0.stdin.take().unwrap();
        let output = child.0.stdout.take().unwrap();
        let errors = child.0.stderr.take().unwrap();
        for fd in [input.as_raw_fd(), output.as_raw_fd(), errors.as_raw_fd()] {
            nonblocking(fd)?;
        }
        Ok(Self {
            child,
            input,
            output,
            errors,
            outgoing: Vec::new(),
            written: 0,
            response: Vec::new(),
            diagnostics: Vec::new(),
            started: Instant::now(),
            _directory: directory,
        })
    }

    fn begin(&mut self, request: &protocol::Request) -> Result<()> {
        self.outgoing = serde_json::to_vec(request)?;
        if self.outgoing.len() > 64 * 1024 {
            return Err(Error::invalid("Native request exceeds protocol limit"));
        }
        self.outgoing.push(b'\n');
        self.written = 0;
        self.response.clear();
        self.diagnostics.clear();
        self.started = Instant::now();
        Ok(())
    }

    fn poll(&mut self) -> Option<Result<protocol::Encoded>> {
        match self.poll_io() {
            Ok(None) => None,
            Ok(Some(bytes)) => Some(
                serde_json::from_slice::<protocol::Response>(&bytes)
                    .map_err(|_| worker_error("Invalid native worker response"))
                    .and_then(|r| {
                        if r.version == protocol::VERSION {
                            r.result
                        } else {
                            Err(worker_error(
                                "Native worker version mismatch; rebuild both executables",
                            ))
                        }
                    }),
            ),
            Err(mut error) => {
                if !self.diagnostics.is_empty() {
                    error.details["diagnostic"] = json!(String::from_utf8_lossy(
                        &self.diagnostics[..self.diagnostics.len().min(4096)]
                    ));
                }
                Some(Err(error))
            }
        }
    }

    fn poll_io(&mut self) -> Result<Option<Vec<u8>>> {
        if self.started.elapsed() >= Duration::from_secs(protocol::MAX_FILE_SECONDS) {
            return Err(Error::new(
                ErrorCode::WorkerTimeout,
                "worker",
                "Native encoding exceeded the 300 second per-file deadline",
            ));
        }
        while self.written < self.outgoing.len() {
            match self.input.write(&self.outgoing[self.written..]) {
                Ok(0) => return Err(worker_error("Worker closed its request pipe")),
                Ok(n) => self.written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(worker_error(e.to_string())),
            }
        }
        fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>) -> Result<bool> {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return Ok(true),
                    Ok(n) => {
                        if bytes.len() + n > protocol::MAX_RESPONSE + 1 {
                            return Err(worker_error("Native output exceeded protocol limit"));
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(worker_error(e.to_string())),
                }
            }
        }
        drain(&mut self.errors, &mut self.diagnostics)?;
        let eof = drain(&mut self.output, &mut self.response)?;
        if let Some(end) = self.response.iter().position(|b| *b == b'\n') {
            if end > protocol::MAX_RESPONSE || end + 1 != self.response.len() {
                return Err(worker_error("Unexpected data after native response"));
            }
            return Ok(Some(self.response[..end].to_vec()));
        }
        if eof || self.child.0.try_wait()?.is_some() {
            return Err(worker_error(
                "Worker exited without a complete protocol response",
            ));
        }
        Ok(None)
    }
}
