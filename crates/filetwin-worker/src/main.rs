use filetwin_worker::{documents, media, raster};

use filetwin_core::{Error, ErrorCode, Result, profile, worker_protocol as protocol};
use std::io::{Read, Write};

fn encode(request: &protocol::Request) -> Result<protocol::Encoded> {
    if request.version != protocol::VERSION {
        return Err(Error::new(
            ErrorCode::WorkerFailed,
            "worker",
            "Worker protocol version mismatch",
        ));
    }
    if request.format == "media" {
        return media::encode(request);
    }
    let p = profile::find(&request.profile_id)?;
    match p.family.as_str() {
        "text" => documents::encode(request),
        "image" => raster::encode(request),
        "audio" => media::audio(request),
        "video" => media::video(request),
        _ => Err(Error::new(
            ErrorCode::UnsupportedFormat,
            "worker",
            "Unknown encoding family",
        )),
    }
}

fn main() {
    let result = (|| -> Result<protocol::Encoded> {
        let mut input = Vec::new();
        std::io::stdin()
            .take(64 * 1024 + 1)
            .read_to_end(&mut input)?;
        if input.len() > 64 * 1024 {
            return Err(Error::invalid("Worker request too large"));
        }
        let request: protocol::Request = serde_json::from_slice(&input)?;
        watch_parent(request.parent_pid);
        encode(&request)
    })();
    let response = protocol::Response {
        version: protocol::VERSION,
        result,
    };
    if let Ok(bytes) = serde_json::to_vec(&response) {
        let _ = std::io::stdout().write_all(&bytes);
    }
}

fn watch_parent(expected: u32) {
    // SAFETY: getpgrp/getpid only inspect the calling process. A standalone worker
    // must never terminate the shell's group; only the coordinator's private
    // group (whose ID equals this worker PID) is eligible for orphan cleanup.
    if expected == 0 || unsafe { libc::getpgrp() != libc::getpid() } {
        return;
    }
    std::thread::spawn(move || {
        loop {
            // SAFETY: the worker is the private process-group leader. If its original
            // host disappears, terminate this worker and its decoder descendants.
            unsafe {
                if libc::getppid() as u32 != expected {
                    libc::kill(-libc::getpid(), libc::SIGKILL);
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
}
