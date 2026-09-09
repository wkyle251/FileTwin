use filetwin_worker::{documents, media, raster};

use filetwin_core::{Error, ErrorCode, Result, profile, worker_protocol as protocol};
use std::io::{BufRead, Read, Write};

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
    let serve = std::env::args().any(|a| a == "--serve");
    let mut input = std::io::stdin().lock();
    let mut watched = false;
    loop {
        let mut bytes = Vec::new();
        let read = if serve {
            input
                .by_ref()
                .take(64 * 1024 + 2)
                .read_until(b'\n', &mut bytes)
        } else {
            input.by_ref().take(64 * 1024 + 1).read_to_end(&mut bytes)
        };
        if matches!(read, Ok(0)) {
            break;
        }
        let started = std::time::Instant::now();
        let result = (|| -> Result<protocol::Encoded> {
            read?;
            if bytes.len() > 64 * 1024 + usize::from(serve)
                || (serve && bytes.last() != Some(&b'\n'))
            {
                return Err(Error::invalid(
                    "Worker request exceeds framed protocol limit",
                ));
            }
            let request: protocol::Request = serde_json::from_slice(&bytes)?;
            if !watched {
                watch_parent(request.parent_pid);
                watched = true;
            }
            let mut encoded = encode(&request)?;
            encoded.extraction["worker_seconds"] =
                serde_json::json!(started.elapsed().as_secs_f64());
            Ok(encoded)
        })();
        let response = protocol::Response {
            version: protocol::VERSION,
            result,
        };
        let output = serde_json::to_vec(&response).expect("Serializable response");
        if output.len() > protocol::MAX_RESPONSE {
            break;
        }
        let mut stdout = std::io::stdout().lock();
        if stdout
            .write_all(&output)
            .and_then(|_| stdout.write_all(b"\n"))
            .and_then(|_| stdout.flush())
            .is_err()
        {
            break;
        }
        if !serve {
            break;
        }
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
