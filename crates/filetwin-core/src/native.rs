use crate::{
    Error, ErrorCode, Result, api::EngineConfig, engine::Work, profile, worker_protocol as protocol,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Input is copied from the already opened source into private bounded staging.
/// Native code never reopens the user's pathname. Each worker and its decoder
/// descendants share a private process group, terminated on every exit path.
pub(crate) fn encode(
    work: &mut Work<'_>,
    file: &mut File,
    format: &str,
    profile_id: &str,
    compute: bool,
) -> Result<(protocol::Encoded, Option<String>)> {
    let allowance = work.job.request.limits.as_ref().expect("Limits");
    let staging_bytes = allowance.staging_bytes.expect("Staging limit");
    let memory_bytes = allowance.memory_bytes.expect("Memory limit");
    let family = profile::find(profile_id)?.family;
    validate_runtime(&work.config, &family, format)?;
    let required_memory = if format != "media" && (family == "image" || family == "video") {
        512 * 1024 * 1024
    } else {
        128 * 1024 * 1024
    };
    if memory_bytes < required_memory {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "encoding",
            format!(
                "{family} encoding requires at least {} MiB",
                required_memory / 1024 / 1024
            ),
        ));
    }
    // Reserve two bounded worker protocol files in addition to the input copy.
    let source_limit = staging_bytes.saturating_sub(2 * protocol::MAX_RESPONSE as u64);
    if file.metadata()?.len() > source_limit {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "staging",
            "File exceeds staging allowance (includes a 2 MiB protocol reserve)",
        ));
    }
    std::fs::create_dir_all(&work.config.temp_dir)?;
    let private = tempfile::Builder::new()
        .prefix("filetwin-worker-")
        .tempdir_in(&work.config.temp_dir)?;
    let mut staged = tempfile::NamedTempFile::new_in(private.path())?;
    let mut hash = compute.then(Sha256::new);
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        work.check()?;
        let n = file.read(&mut buffer)?;
        work.job.counts.bytes_read += n as u64;
        bytes += n as u64;
        if bytes > source_limit {
            return Err(Error::new(
                ErrorCode::ResourceBudgetTooSmall,
                "staging",
                "Source grew beyond staging allowance",
            ));
        }
        if n == 0 {
            break;
        }
        staged.write_all(&buffer[..n])?;
        if let Some(h) = &mut hash {
            h.update(&buffer[..n]);
            work.job.counts.bytes_hashed += n as u64;
        }
    }
    staged.flush()?;
    let digest = hash.map(|h| format!("{:x}", h.finalize()));
    let request = protocol::Request {
        version: protocol::VERSION,
        path: staged.path().to_owned(),
        format: format.into(),
        profile_id: profile_id.into(),
        model_dir: work.config.model_dir.clone(),
        profiles: work.job.request.profiles.clone().expect("Profiles"),
        runtime: work.config.runtime.clone(),
        memory_bytes,
        parent_pid: std::process::id(),
    };
    let result = invoke(&work.config, &request, private.path(), &|| work.check());
    match result {
        Ok(encoded) => {
            let id = if format == "media" {
                request
                    .profiles
                    .get(&encoded.family)
                    .map(String::as_str)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorCode::WorkerFailed,
                            "worker",
                            "Unrequested media family",
                        )
                    })?
            } else {
                profile_id
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
            Ok((encoded, digest))
        }
        Err(mut e) => {
            if let Some(digest) = digest {
                e.details["digest"] = json!(digest);
            }
            Err(e)
        }
    }
}

pub(crate) fn validate_runtime(config: &EngineConfig, family: &str, format: &str) -> Result<()> {
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
                    "Configure an absolute existing {name} path in EngineConfig.runtime or [engine.runtime]"
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

fn invoke(
    config: &EngineConfig,
    request: &protocol::Request,
    directory: &Path,
    check: &dyn Fn() -> Result<()>,
) -> Result<protocol::Encoded> {
    let mut input = tempfile::tempfile_in(directory)?;
    let bytes = serde_json::to_vec(request)?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::invalid("Native request exceeds protocol limit"));
    }
    input.write_all(&bytes)?;
    input.seek(SeekFrom::Start(0))?;
    let mut stdout = tempfile::tempfile_in(directory)?;
    let mut stderr = tempfile::tempfile_in(directory)?;
    let mut command = Command::new(
        config
            .runtime
            .worker_path
            .as_ref()
            .expect("Validated worker"),
    );
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("OMP_NUM_THREADS", "1")
        .current_dir(directory)
        .process_group(0)
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    let memory = request.memory_bytes;
    // SAFETY: pre_exec only calls async-signal-safe setrlimit and does not allocate
    // or acquire locks. Limits affect the child and its descendants, not the host.
    unsafe {
        command.pre_exec(move || {
            let output = libc::rlimit {
                rlim_cur: protocol::MAX_RESPONSE as _,
                rlim_max: protocol::MAX_RESPONSE as _,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &output) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let no_core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &no_core) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                let cap = libc::rlimit {
                    rlim_cur: memory,
                    rlim_max: memory,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &cap) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            #[cfg(target_os = "macos")]
            let _ = memory; // macOS has no reliable per-process RSS rlimit.
            Ok(())
        });
    }
    let mut child = ProcessGroup(
        command
            .spawn()
            .map_err(|e| Error::new(ErrorCode::WorkerFailed, "worker", e.to_string()))?,
    );
    let start = Instant::now();
    let status = loop {
        check()?;
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        if start.elapsed() >= Duration::from_secs(protocol::MAX_FILE_SECONDS) {
            return Err(Error::new(
                ErrorCode::WorkerTimeout,
                "worker",
                "Native encoding exceeded the 300 second per-file deadline",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Kill descendants before reading output, including a decoder left behind by
    // an unexpected worker exit. The files eliminate pipe-full deadlocks.
    drop(child);
    stdout.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    stdout
        .take((protocol::MAX_RESPONSE + 1) as u64)
        .read_to_end(&mut bytes)?;
    if !status.success() || bytes.len() > protocol::MAX_RESPONSE {
        stderr.seek(SeekFrom::Start(0))?;
        let mut diagnostic = Vec::new();
        stderr.take(4096).read_to_end(&mut diagnostic)?;
        let mut e = Error::new(
            ErrorCode::WorkerFailed,
            "worker",
            format!("Native worker exited with {status}"),
        );
        e.details = Box::new(json!({"diagnostic":String::from_utf8_lossy(&diagnostic)}));
        return Err(e);
    }
    let response: protocol::Response = serde_json::from_slice(&bytes).map_err(|_| {
        Error::new(
            ErrorCode::WorkerFailed,
            "worker",
            "Invalid native worker response",
        )
    })?;
    if response.version != protocol::VERSION {
        return Err(Error::new(
            ErrorCode::WorkerFailed,
            "worker",
            "Native worker version mismatch",
        ));
    }
    response.result
}
