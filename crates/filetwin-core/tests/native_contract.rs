//! Test the host/worker boundary without installing any native models.
use filetwin_core::{
    Catalog, Engine, ErrorCode, HostServices,
    api::*,
    profile,
    worker_protocol::{Encoded, Response, VERSION},
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn config(tmp: &Path, worker_body: &str) -> EngineConfig {
    let mut config = EngineConfig::new(tmp.join("data"));
    let worker = tmp.join("worker");
    script(&worker, worker_body);
    let dummy = tmp.join("library");
    fs::write(&dummy, []).unwrap();
    fs::create_dir_all(&config.model_dir).unwrap();
    fs::write(config.model_dir.join(profile::SSCD_MODEL_FILE), []).unwrap();
    config.runtime = RuntimeConfig {
        worker_path: Some(worker.clone()),
        ffmpeg_path: Some(worker.clone()),
        ffprobe_path: Some(worker),
        onnxruntime_path: Some(dummy.clone()),
        pdfium_path: Some(dummy),
    };
    config
}

fn response(path: &Path, family: &str, dimensions: usize) {
    let mut vector = vec![0.0; dimensions];
    vector[0] = 1.0;
    let result = Response {
        version: VERSION,
        result: Ok(Encoded {
            family: family.into(),
            format: "fixture".into(),
            vector,
            extraction: json!({"fixture":true}),
        }),
    };
    fs::write(path, serde_json::to_vec(&result).unwrap()).unwrap();
}

#[test]
fn mixed_dimensions_profiles_cache_and_frozen_comparison() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    response(&tmp.join("image.json"), "image", 512);
    response(&tmp.join("video.json"), "video", 512);
    let cfg = config(
        &tmp,
        &format!(
            "request=$(/bin/cat)\ncase \"$request\" in *'\"format\":\"media\"'*) /bin/cat '{}' ;; *) /bin/cat '{}' ;; esac",
            tmp.join("video.json").display(),
            tmp.join("image.json").display()
        ),
    );
    let root = tmp.join("input");
    fs::create_dir(&root).unwrap();
    for i in 0..2 {
        fs::write(root.join(format!("{i}.txt")), "identical text\n").unwrap();
        fs::write(root.join(format!("{i}.png")), b"\x89PNG\r\n\x1a\n").unwrap();
        fs::write(root.join(format!("{i}.mp4")), b"\0\0\0\x18ftypisom").unwrap();
    }
    let engine = Engine::open(cfg, HostServices::default()).unwrap();
    let request = JobRequest::experimental_scan([root.clone()], 0.95);
    let a = engine.submit(request.clone()).unwrap().wait().unwrap();
    assert_eq!(a.counts.files_ready, 6, "{a:?}");
    assert_eq!(a.counts.pairs_compared, 3);
    assert_eq!(a.counts.similar_pairs, 3);
    assert_eq!(a.counts.groups, 3);
    let b = engine.submit(request.clone()).unwrap().wait().unwrap();
    assert_eq!((b.counts.cache_hits, b.counts.bytes_read), (6, 0));
    fs::remove_dir_all(root).unwrap();
    let compare = JobRequest {
        operation: Operation::Compare,
        snapshot_id: a.snapshot_id.clone(),
        matching: request.matching,
        ..JobRequest::default()
    };
    let c = engine.submit(compare.clone()).unwrap().wait().unwrap();
    assert_eq!(c.counts.similar_pairs, 3);
    assert_eq!(c.counts.bytes_read, 0);
    let mut incomplete = compare;
    incomplete
        .matching
        .as_mut()
        .unwrap()
        .threshold_overrides
        .remove(&profile::image_profile().profile_id);
    assert!(matches!(engine.submit(incomplete), Err(e) if e.code == ErrorCode::ThresholdRequired));
    let groups = Catalog::open_read_only(tmp.join("data"))
        .unwrap()
        .results(ResultsQuery {
            run_id: c.run_id,
            kind: "groups".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    let families: std::collections::BTreeSet<_> = groups
        .items
        .iter()
        .map(|g| g["family"].as_str().unwrap())
        .collect();
    assert_eq!(
        families,
        std::collections::BTreeSet::from(["image", "text", "video"])
    );
}

#[test]
fn matrices_keep_incompatible_profiles_null_even_when_dimensions_match() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    response(&root.join("image.json"), "image", 512);
    response(&root.join("video.json"), "video", 512);
    let cfg = config(
        &root,
        &format!(
            "request=$(/bin/cat)\ncase \"$request\" in *'\"format\":\"media\"'*) /bin/cat '{}' ;; *) /bin/cat '{}' ;; esac",
            root.join("video.json").display(),
            root.join("image.json").display()
        ),
    );
    let input = root.join("input");
    fs::create_dir(&input).unwrap();
    fs::write(input.join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    fs::write(input.join("video.mp4"), b"\0\0\0\x18ftypisom").unwrap();
    let engine = Engine::open(cfg, HostServices::default()).unwrap();
    let summary = engine
        .submit(JobRequest::experimental_scores([input]))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(summary.counts.files_ready, 2, "{summary:?}");
    let matrix = Catalog::open_read_only(root.join("data"))
        .unwrap()
        .matrix(MatrixQuery::new(summary.run_id.unwrap()))
        .unwrap();
    assert_eq!(
        matrix.scores,
        vec![vec![Some(1.0), None], vec![None, Some(1.0)]]
    );
    assert_eq!(
        matrix.unavailable_reasons[0][1],
        Some(MatrixUnavailable::IncompatibleProfile)
    );
    assert_eq!(
        matrix.unavailable_reasons[1][0],
        Some(MatrixUnavailable::IncompatibleProfile)
    );
}

#[test]
fn malformed_worker_output_is_a_file_failure_and_does_not_stop_text() {
    let temp = tempfile::tempdir().unwrap();
    let tmp = temp.path().canonicalize().unwrap();
    let cfg = config(&tmp, "printf 'not a protocol response'");
    let image = tmp.join("bad.png");
    fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
    let text = tmp.join("good.txt");
    fs::write(&text, "still processes text").unwrap();
    let engine = Engine::open(cfg, HostServices::default()).unwrap();
    let summary = engine
        .submit(JobRequest::experimental_scan([image, text], 0.95))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(
        (summary.counts.files_ready, summary.counts.files_failed),
        (1, 1)
    );
    assert_eq!(summary.completeness.source_coverage, "partial");
    let errors = Catalog::open_read_only(tmp.join("data"))
        .unwrap()
        .results(ResultsQuery {
            run_id: summary.run_id,
            kind: "errors".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert_eq!(errors.items[0]["code"], "worker_failed");
}

#[test]
fn cancellation_kills_native_descendants_and_removes_staging() {
    let temp = tempfile::tempdir().unwrap();
    let tmp = temp.path().canonicalize().unwrap();
    let pid_file = tmp.join("decoder.pid");
    let cfg = config(
        &tmp,
        &format!(
            "/bin/sleep 120 &\nprintf '%s' \"$!\" > '{}'\nwait",
            pid_file.display()
        ),
    );
    let staging = cfg.temp_dir.clone();
    let image = tmp.join("image.png");
    fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
    let engine = Engine::open(cfg, HostServices::default()).unwrap();
    let job = engine
        .submit(JobRequest::experimental_scan([image], 0.95))
        .unwrap();
    let start = Instant::now();
    while !pid_file.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    job.cancel();
    let summary = job.wait().unwrap();
    assert_eq!(summary.status, "cancelled");
    assert_eq!(summary.counts.files_ready, 0);
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
    // A killed descendant can be briefly visible as a zombie until the system
    // reaper observes it. It must never continue running after cancellation.
    for _ in 0..100 {
        // SAFETY: signal zero only checks process existence, without signaling it.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        #[cfg(target_os = "linux")]
        if fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z ")) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Native decoder still running after cancellation");
}

#[test]
fn experimental_profiles_reject_unknown_families_and_mismatched_thresholds() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("a.txt");
    fs::write(&path, "hello").unwrap();
    let engine = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    let mut r = JobRequest::experimental_scan([path], 0.95);
    r.families = Some(vec!["image".into()]);
    assert!(matches!(engine.submit(r.clone()), Err(e) if e.code == ErrorCode::InvalidRequest));
    r.families = None;
    r.profiles
        .as_mut()
        .unwrap()
        .insert("text".into(), profile::image_profile().profile_id);
    assert!(matches!(engine.submit(r), Err(e) if e.code == ErrorCode::InvalidRequest));
}

#[test]
fn known_digests_survive_profile_changes_and_vector_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("input");
    fs::create_dir(&root).unwrap();
    for name in ["a.txt", "b.md"] {
        fs::write(root.join(name), "The same original bytes.").unwrap();
    }
    let engine = Engine::open(
        EngineConfig::new(temp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    let mut old = JobRequest::text_scan([root.clone()], 0.95);
    old.exact_duplicates = Some(ExactDuplicates::Compute);
    assert_eq!(
        engine
            .submit(old)
            .unwrap()
            .wait()
            .unwrap()
            .counts
            .exact_pairs,
        1
    );
    let mut modern = JobRequest::experimental_scan([root], 0.95);
    let changed = engine.submit(modern.clone()).unwrap().wait().unwrap();
    assert_eq!(changed.counts.vectors_encoded, 2);
    assert_eq!(changed.counts.bytes_hashed, 0);
    assert_eq!(changed.counts.exact_pairs, 1);
    modern.cache = Some(Cache {
        mode: CacheMode::Refresh,
        ..Cache::default()
    });
    let refreshed = engine.submit(modern).unwrap().wait().unwrap();
    assert_eq!(refreshed.counts.vectors_encoded, 2);
    assert_eq!(refreshed.counts.bytes_hashed, 0);
    assert_eq!(refreshed.counts.exact_pairs, 1);
}
