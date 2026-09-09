use crate::args::Format;
use filetwin_core::{Error, ErrorCode, Result, api::Envelope};
use serde_json::Value;
use std::{
    io::{self, IsTerminal},
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    time::{Duration, Instant},
};

/// The CLI owns signal handling. No signal registrations enter the core crate.
pub struct Signals {
    pub received: Arc<AtomicI32>,
    handle: signal_hook::iterator::Handle,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Signals {
    pub fn install() -> Result<Self> {
        let mut signals = signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM])?;
        let handle = signals.handle();
        let received = Arc::new(AtomicI32::new(0));
        let flag = received.clone();
        let thread = std::thread::Builder::new()
            .name("filetwin-signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    if flag
                        .compare_exchange(0, signal, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        std::process::exit(128 + signal);
                    }
                }
            })?;
        Ok(Self {
            received,
            handle,
            thread: Some(thread),
        })
    }
    pub fn cancelled(&self) -> bool {
        self.received.load(Ordering::Acquire) != 0
    }
    pub fn exit_code(&self) -> i32 {
        128 + self.received.load(Ordering::Acquire)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Nonblocking pipe writes keep memory bounded and let signals interrupt a
/// stalled consumer. Restore descriptor flags when returning to normal teardown.
struct Descriptor {
    fd: libc::c_int,
    flags: libc::c_int,
}
impl Descriptor {
    fn new(fd: libc::c_int) -> io::Result<Self> {
        // SAFETY: fcntl reads flags on the supplied standard descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: F_SETFL changes status flags without transferring ownership.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, flags })
    }
    fn write(
        &self,
        mut bytes: &[u8],
        signal: &AtomicI32,
        timeout: Option<Duration>,
    ) -> io::Result<()> {
        let started = Instant::now();
        let mut cancelled_at = None;
        while !bytes.is_empty() {
            // SAFETY: bytes is readable for its length and remains borrowed
            // throughout write; the descriptor remains owned by this process.
            let n = unsafe { libc::write(self.fd, bytes.as_ptr().cast(), bytes.len()) };
            if n > 0 {
                bytes = &bytes[n as usize..];
                continue;
            }
            let error = if n == 0 {
                io::Error::new(io::ErrorKind::WriteZero, "Output made no progress")
            } else {
                io::Error::last_os_error()
            };
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
            if signal.load(Ordering::Acquire) != 0 {
                cancelled_at.get_or_insert_with(Instant::now);
            }
            if timeout.is_some_and(|d| started.elapsed() >= d)
                || cancelled_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(1))
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Output consumer did not drain during cancellation",
                ));
            }
            let mut poll = libc::pollfd {
                fd: self.fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: poll points to one initialized pollfd for this call.
            let rc = unsafe { libc::poll(&mut poll, 1, 100) };
            if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: restoring the original status flags does not transfer ownership.
        unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.flags) };
    }
}

pub struct Output {
    pub format: Format,
    pub request_id: String,
    pub job_id: Option<String>,
    pub run_id: Option<String>,
    invocation_id: String,
    sequence: u64,
    stdout: Descriptor,
    stderr: Option<Descriptor>,
    signal: Arc<AtomicI32>,
}
impl Output {
    pub fn closed(&self) -> bool {
        let mut poll = libc::pollfd {
            fd: self.stdout.fd,
            events: 0,
            revents: 0,
        };
        // SAFETY: poll points to one initialized descriptor; a zero timeout
        // only inspects readiness/error state and consumes no output.
        (unsafe { libc::poll(&mut poll, 1, 0) }) > 0
            && poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
    }
    pub fn new(format: Format, request_id: String, signal: Arc<AtomicI32>) -> Result<Self> {
        Ok(Self {
            format,
            request_id,
            job_id: None,
            run_id: None,
            invocation_id: format!("invocation_{}", uuid::Uuid::new_v4()),
            sequence: 0,
            stdout: Descriptor::new(libc::STDOUT_FILENO)?,
            stderr: Descriptor::new(libc::STDERR_FILENO).ok(),
            signal,
        })
    }
    pub fn raw(&self, text: &str) -> Result<()> {
        self.stdout
            .write(text.as_bytes(), &self.signal, None)
            .map_err(output_error)
    }
    pub fn emit(&mut self, kind: &str, data: Value) -> Result<()> {
        if self.format == Format::Human {
            if kind == "error" {
                self.diagnostic(&format!(
                    "{}: {}",
                    data["code"].as_str().unwrap_or("error"),
                    data["message"].as_str().unwrap_or("Operation failed")
                ));
                return Ok(());
            }
            if kind == "summary" {
                return self.raw(&format!("{}: {}\nSnapshot: {}\nRun: {}\nFiles: {} ready, {} failed, {} excluded\nMatches: {} pairs, {} groups; {} byte-identical pairs\nCoverage: {}; {}\n",data["job_id"].as_str().unwrap_or("job"),data["status"].as_str().unwrap_or("unknown"),data["snapshot_id"].as_str().unwrap_or("none"),data["run_id"].as_str().unwrap_or("none"),data["counts"]["files_ready"],data["counts"]["files_failed"],data["counts"]["files_excluded"],data["counts"]["similar_pairs"],data["counts"]["groups"],data["counts"]["exact_pairs"],data["completeness"]["source_coverage"].as_str().unwrap_or("unknown"),data["completeness"]["comparison_coverage"].as_str().unwrap_or("unknown")));
            }
            return self.raw(&(serde_json::to_string_pretty(&data)? + "\n"));
        }
        self.sequence += 1;
        let envelope = Envelope {
            schema_version: 1,
            invocation_id: self.invocation_id.clone(),
            request_id: self.request_id.clone(),
            sequence: self.sequence,
            kind: kind.into(),
            job_id: self.job_id.clone(),
            run_id: self.run_id.clone(),
            data,
        };
        self.raw(&(serde_json::to_string(&envelope)? + "\n"))
    }
    pub fn diagnostic(&self, message: &str) {
        if let Some(stderr) = &self.stderr {
            let _ = stderr.write(
                format!("{message}\n").as_bytes(),
                &self.signal,
                Some(Duration::from_millis(100)),
            );
        }
    }
    pub fn interactive_progress(&self) -> bool {
        self.format == Format::Human && io::stderr().is_terminal()
    }
}
fn output_error(error: io::Error) -> Error {
    Error::new(ErrorCode::OutputClosed, "output", error.to_string())
}
