//! Host/worker contracts with deterministic disposable workers; no model downloads.
use filetwin_core::{
    Encoder, ErrorCode,
    api::*,
    profile,
    worker_protocol::{Encoded, Response, VERSION},
    write_vectors,
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    input: PathBuf,
    config: EncoderConfig,
}
impl Fixture {
    fn new(body: impl FnOnce(&Path) -> String) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let input = root.join("input");
        fs::create_dir(&input).unwrap();
        let mut config = EncoderConfig::new(root.join("models"), root.join("staging"));
        fs::create_dir(&config.temp_dir).unwrap();
        fs::create_dir(&config.model_dir).unwrap();
        fs::write(config.model_dir.join(profile::SSCD_MODEL_FILE), []).unwrap();
        let worker = root.join("worker");
        fs::write(&worker, format!("#!/bin/sh\n{}\n", body(&root))).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        let library = root.join("library");
        fs::write(&library, []).unwrap();
        config.runtime = RuntimeConfig {
            worker_path: Some(worker.clone()),
            ffmpeg_path: Some(worker.clone()),
            ffprobe_path: Some(worker),
            onnxruntime_path: Some(library.clone()),
            pdfium_path: Some(library),
            ..RuntimeConfig::default()
        };
        response(&root.join("response.json"), "image", 512);
        Self {
            _temp: temp,
            root,
            input,
            config,
        }
    }
    fn images(&self, n: u8) {
        for i in 0..n {
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.push(i);
            fs::write(self.input.join(format!("{i}.png")), bytes).unwrap();
        }
    }
    fn run(&self) -> VectorFile {
        Encoder::new(self.config.clone())
            .unwrap()
            .encode(
                &EncodeRequest::new(&self.input),
                &CancellationToken::default(),
                |_| {},
            )
            .unwrap()
    }
    fn clean(&self) {
        assert_eq!(fs::read_dir(&self.config.temp_dir).unwrap().count(), 0);
    }
}
fn response(path: &Path, family: &str, dimensions: usize) {
    let mut vector = vec![0.0; dimensions];
    vector[0] = 1.0;
    fs::write(
        path,
        serde_json::to_vec(&Response {
            version: VERSION,
            result: Ok(Encoded {
                family: family.into(),
                format: "fixture".into(),
                vector,
                extraction: json!({"fixture":true}),
            }),
        })
        .unwrap(),
    )
    .unwrap();
}
fn serve(root: &Path) -> String {
    format!(
        "while IFS= read -r request; do /bin/cat '{}'; printf '\\n'; done",
        root.join("response.json").display()
    )
}

#[test]
fn native_cache_and_scratch_files_are_disposable_between_calls() {
    let f = Fixture::new(|root| {
        format!(
            r#"
while IFS= read -r request; do
    case "$TMPDIR" in '{root}/staging/'*) ;; *) exit 31 ;; esac
    case "$XDG_CACHE_HOME" in "$TMPDIR/"*) ;; *) exit 32 ;; esac
    case "$CUDA_CACHE_PATH" in "$TMPDIR/"*) ;; *) exit 33 ;; esac
    [ "$TMP" = "$TMPDIR" ] && [ "$TEMP" = "$TMPDIR" ] || exit 34
    printf 'scratch' > "$TMPDIR/decoder-tmp"
    printf 'cache' > "$XDG_CACHE_HOME/library-cache"
    printf 'compiled' > "$CUDA_CACHE_PATH/kernel-cache"
    /bin/cat '{root}/response.json'
    printf '\n'
done
"#,
            root = root.display()
        )
    });
    f.images(3);
    for _ in 0..3 {
        let result = f.run();
        assert_eq!(result.summary.counts.files_ready, 3);
        assert_eq!(result.summary.counts.vectors_encoded, 3);
        f.clean();
    }
}
fn wait_for(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "worker did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_workers_run_concurrently_reuse_processes_and_deduplicate_content() {
    let f = Fixture::new(|root| {
        fs::create_dir(root.join("starts")).unwrap();
        format!(
            r#"
while IFS= read -r request; do
    /usr/bin/touch '{root}/starts/'$$
    attempts=0
    while [ "$(/bin/ls '{root}/starts' | /usr/bin/wc -l)" -lt 2 ]; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 500 ] || exit 19
        /bin/sleep 0.01
    done
    /bin/cat '{root}/response.json'
    printf '\n'
done
"#,
            root = root.display()
        )
    });
    f.images(6);
    fs::hard_link(f.input.join("0.png"), f.input.join("alias.png")).unwrap();
    let result = f.run();
    assert_eq!(
        (
            result.summary.counts.files_ready,
            result.summary.counts.vectors_encoded,
            result.summary.counts.cache_hits
        ),
        (7, 6, 1),
        "{:?}",
        result.summary
    );
    assert_eq!(fs::read_dir(f.root.join("starts")).unwrap().count(), 2);
    assert!(
        result
            .files
            .iter()
            .all(|f| f.vector.as_ref().unwrap().len() == 512)
    );
    f.clean();
}

#[test]
fn a_crashed_worker_is_replaced_for_the_next_file() {
    let mut f = Fixture::new(|root| {
        format!(
            r#"
while IFS= read -r request; do
    if [ ! -e '{root}/crashed' ]; then /usr/bin/touch '{root}/crashed'; exit 17; fi
    /bin/cat '{root}/response.json'
    printf '\n'
done
"#,
            root = root.display()
        )
    });
    f.config.workers = 1;
    f.images(3);
    let result = f.run();
    assert_eq!(
        (
            result.summary.counts.files_ready,
            result.summary.counts.files_failed
        ),
        (2, 1)
    );
    assert_eq!(
        result.files[0].error.as_ref().unwrap().code,
        ErrorCode::WorkerFailed
    );
    assert!(result.files[0].file_id.is_some());
    f.clean();
}

#[test]
fn malformed_oversized_and_incompatible_worker_results_do_not_stop_text() {
    for kind in [
        "malformed",
        "oversized",
        "version",
        "dimensions",
        "family",
        "zero_vector",
    ] {
        let f = Fixture::new(|root| match kind {
            "malformed" => "printf 'not JSON\\n'".into(),
            "oversized" => "/usr/bin/head -c 1048577 /dev/zero".into(),
            _ => serve(root),
        });
        if matches!(kind, "dimensions" | "family") {
            response(
                &f.root.join("response.json"),
                if kind == "family" { "audio" } else { "image" },
                4096,
            );
        }
        if matches!(kind, "version" | "zero_vector") {
            let path = f.root.join("response.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if kind == "version" {
                value["version"] = json!(VERSION - 1);
            } else {
                value["result"]["Ok"]["vector"] = json!(vec![0.0; 512]);
            }
            fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        f.images(1);
        fs::write(f.input.join("text.txt"), "still encodes complete text").unwrap();
        let result = f.run();
        assert_eq!(
            (
                result.summary.counts.files_ready,
                result.summary.counts.files_failed
            ),
            (1, 1),
            "{kind}"
        );
        assert!(
            result.files[0].file_id.is_some() && result.files[0].vector.is_none(),
            "{kind}"
        );
        assert_eq!(
            result.files[0].error.as_ref().unwrap().code,
            ErrorCode::WorkerFailed,
            "{kind}"
        );
        f.clean();
    }
}

#[test]
fn a_source_changed_during_native_encoding_loses_id_and_vector() {
    let f = Fixture::new(|root| {
        format!(
            r#"
while IFS= read -r request; do
    /usr/bin/touch '{root}/started'
    while [ ! -e '{root}/release' ]; do /bin/sleep 0.01; done
    /bin/cat '{root}/response.json'
    printf '\n'
done
"#,
            root = root.display()
        )
    });
    f.images(1);
    let input = f.input.clone();
    let config = f.config.clone();
    let thread = std::thread::spawn(move || {
        Encoder::new(config)
            .unwrap()
            .encode(
                &EncodeRequest::new(input),
                &CancellationToken::default(),
                |_| {},
            )
            .unwrap()
    });
    wait_for(&f.root.join("started"));
    fs::write(f.input.join("0.png"), b"\x89PNG\r\n\x1a\nchanged").unwrap();
    fs::write(f.root.join("release"), []).unwrap();
    let result = thread.join().unwrap();
    assert_eq!(result.summary.counts.files_failed, 1);
    assert!(result.files[0].file_id.is_none() && result.files[0].vector.is_none());
    assert_eq!(
        result.files[0].error.as_ref().unwrap().code,
        ErrorCode::SourceChanged
    );
    f.clean();
}

#[test]
fn cache_reuses_native_vectors_without_runtime_and_separates_backends() {
    let f = Fixture::new(serve);
    f.images(2);
    let original = f.run();
    let saved = f.root.join("vectors.json");
    write_vectors(&saved, &original).unwrap();
    let mut config = f.config.clone();
    config.runtime = RuntimeConfig::default();
    let mut request = EncodeRequest::new(&f.input);
    request.vectors_file = Some(saved);
    let cached = Encoder::new(config.clone())
        .unwrap()
        .encode(&request, &CancellationToken::default(), |_| {})
        .unwrap();
    assert_eq!(
        (
            cached.summary.counts.cache_hits,
            cached.summary.counts.vectors_encoded
        ),
        (2, 0)
    );
    assert_eq!(cached.summary.counts.bytes_hashed, 18);
    config.backend = profile::Backend::Coreml;
    let different = Encoder::new(config)
        .unwrap()
        .encode(&request, &CancellationToken::default(), |_| {})
        .unwrap();
    assert_eq!(
        (
            different.summary.counts.cache_hits,
            different.summary.counts.files_failed
        ),
        (0, 2)
    );
    assert!(
        different
            .files
            .iter()
            .all(|f| f.error.as_ref().unwrap().code == ErrorCode::RuntimeUnavailable)
    );
}

#[test]
fn staging_allowance_is_shared_and_oversized_files_still_get_an_id() {
    let mut f = Fixture::new(serve);
    f.images(3);
    f.config.staging_bytes = 2 * filetwin_core::worker_protocol::MAX_RESPONSE as u64 + 9;
    let good = f.run();
    assert_eq!(good.summary.counts.files_ready, 3);
    f.config.staging_bytes = 1;
    let limited = f.run();
    assert_eq!(limited.summary.counts.files_failed, 3);
    assert_eq!(limited.summary.counts.bytes_hashed, 27);
    assert!(limited.files.iter().all(|f| f.file_id.is_some()
        && f.vector.is_none()
        && f.error.as_ref().unwrap().code == ErrorCode::ResourceBudgetTooSmall));
    f.clean();
}

#[test]
fn cancellation_kills_worker_descendants_and_cleans_staging() {
    let f = Fixture::new(|root| {
        format!(
            "/bin/sleep 120 &\nprintf '%s' \"$!\" > '{}/decoder.pid'\nwait",
            root.display()
        )
    });
    f.images(1);
    let token = CancellationToken::default();
    let other = token.clone();
    let input = f.input.clone();
    let config = f.config.clone();
    let thread = std::thread::spawn(move || {
        Encoder::new(config)
            .unwrap()
            .encode(&EncodeRequest::new(input), &other, |_| {})
            .unwrap()
    });
    wait_for(&f.root.join("decoder.pid"));
    let pid: i32 = fs::read_to_string(f.root.join("decoder.pid"))
        .unwrap()
        .parse()
        .unwrap();
    token.cancel();
    let result = thread.join().unwrap();
    assert_eq!(result.exit_code(), 130);
    assert_eq!(result.summary.counts.files_processed, 0);
    assert!(!result.complete);
    f.clean();
    for _ in 0..100 {
        // SAFETY: signal zero only checks whether this disposable process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        #[cfg(target_os = "linux")]
        if fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z ")) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Decoder still running after cancellation");
}
