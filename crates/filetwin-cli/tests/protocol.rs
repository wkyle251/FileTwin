use filetwin_core::{
    Catalog,
    api::{JobRequest, StatusQuery},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

fn command(data: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_filetwin"));
    for key in [
        "FILETWIN_CONFIG",
        "FILETWIN_DATA_DIR",
        "FILETWIN_MODEL_DIR",
        "FILETWIN_TEMP_DIR",
    ] {
        c.env_remove(key);
    }
    c.arg("--data-dir").arg(data);
    c
}
fn json_output(data: &Path, args: &[&str]) -> (i32, Value) {
    let o = command(data)
        .args(args)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(
        o.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    (
        o.status.code().unwrap(),
        serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&o.stdout))),
    )
}
fn wait_bounded(child: &mut Child) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            panic!("Child did not stop within 15 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn signal(child: &Child, signal: i32) {
    // SAFETY: child.id identifies this test's live subprocess, and the signal is
    // one of the Unix signal constants exercised by the CLI contract.
    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
}
fn large_input(tmp: &Path) -> std::path::PathBuf {
    let path = tmp.join("large.txt");
    fs::write(&path, "The boat crossed the lake.\n".repeat(160_000)).unwrap();
    path
}

#[test]
fn help_capabilities_and_errors_do_not_require_an_index() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("missing");
    let (code, doctor) = json_output(&data, &["doctor"]);
    assert_eq!(code, 0);
    assert_eq!(doctor["type"], "capabilities");
    assert_eq!(doctor["data"]["docker_required"], false);
    assert!(!data.exists());
    let (code, error) = json_output(&data, &["scan"]);
    assert_eq!(code, 2);
    assert_eq!(error["type"], "error");
    assert!(error["job_id"].is_null());
    assert!(!data.exists());
    let o = command(&data).output().unwrap();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("Usage:"));
}

#[test]
fn score_matrix_filter_and_group_commands_share_saved_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let input = tmp.path().join("input");
    fs::create_dir(&input).unwrap();
    for (name, text) in [("a", "aaa"), ("b", "aab"), ("c", "zzz")] {
        fs::write(input.join(format!("{name}.txt")), text).unwrap();
    }
    let config = tmp.path().join("defaults.toml");
    let profile = filetwin_core::profile::text_profile().profile_id;
    fs::write(
        &config,
        format!("[defaults.matching.threshold_overrides]\n{profile:?} = 1.0\n"),
    )
    .unwrap();
    let (code, scanned) = json_output(
        &data,
        &[
            "--config",
            config.to_str().unwrap(),
            "scan",
            input.to_str().unwrap(),
            "--experimental-text",
            "--all-scores",
        ],
    );
    assert_eq!(code, 0, "{scanned}");
    assert_eq!(scanned["data"]["counts"]["scores_retained"], 3);
    assert_eq!(scanned["data"]["counts"]["files_processed"], 3);
    assert_eq!(scanned["data"]["counts"]["files_total"], 3);
    assert_eq!(scanned["data"]["counts"]["pairs_processed"], 3);
    assert_eq!(scanned["data"]["counts"]["pairs_total"], 3);
    assert_eq!(scanned["data"]["counts"]["groups"], 0);
    let run = scanned["data"]["run_id"].as_str().unwrap();
    let (code, matrix) = json_output(&data, &["matrix", "--run", run]);
    assert_eq!(code, 0, "{matrix}");
    assert_eq!(matrix["type"], "matrix");
    assert_eq!(matrix["data"]["rows"].as_array().unwrap().len(), 3);
    let (code, filtered) = json_output(
        &data,
        &[
            "results",
            "--run",
            run,
            "--kind",
            "scores",
            "--min-score",
            "-1",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(filtered["data"]["items"].as_array().unwrap().len(), 3);
    fs::remove_dir_all(input).unwrap();
    let (code, grouped) = json_output(&data, &["group", "--run", run, "--threshold", "text=-1"]);
    assert_eq!(code, 0, "{grouped}");
    assert_eq!(grouped["data"]["counts"]["scores_reused"], 3);
    assert_eq!(grouped["data"]["counts"]["scores_total"], 3);
    assert_eq!(grouped["data"]["counts"]["files_processed"], 3);
    assert_eq!(grouped["data"]["counts"]["pairs_processed"], 0);
    assert!(grouped["data"]["counts"]["pairs_total"].is_null());
    assert_eq!(grouped["data"]["counts"]["pairs_compared"], 0);
    assert_eq!(grouped["data"]["counts"]["bytes_read"], 0);
    assert_eq!(grouped["data"]["counts"]["groups"], 1);
    let mut request =
        JobRequest::group_scores(run, std::collections::BTreeMap::from([(profile, 0.95)]));
    request.source_revision = Some(1);
    let path = tmp.path().join("group.json");
    fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
    let (code, from_json) = json_output(&data, &["run", "--request", path.to_str().unwrap()]);
    assert_eq!(code, 0, "{from_json}");
    assert_eq!(from_json["data"]["counts"]["scores_reused"], 3);
    for args in [
        vec!["matrix", "--run", run, "--row-limit", "257"],
        vec![
            "results",
            "--run",
            run,
            "--kind",
            "groups",
            "--min-score",
            "0.8",
        ],
    ] {
        let (code, error) = json_output(&data, &args);
        assert_eq!(code, 2, "{error}");
    }
}

#[test]
fn flags_and_serialized_requests_share_core_results() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("input");
    fs::create_dir(&input).unwrap();
    fs::write(input.join("a.txt"), "The boat crossed the quiet lake.").unwrap();
    fs::write(input.join("b.txt"), "The boat crossed the quiet lake!").unwrap();
    let data = tmp.path().join("data");
    let (code, flag) = json_output(
        &data,
        &[
            "scan",
            input.to_str().unwrap(),
            "--experimental-text",
            "--threshold",
            "0.7",
            "--request-id",
            "flags",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(flag["type"], "summary");
    assert_eq!(flag["sequence"], 1);
    assert_eq!(flag["request_id"], "flags");
    let mut child = command(&data)
        .args(["run", "--request", "-", "--format", "jsonl"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut request = JobRequest::text_scan([input], 0.7);
    request.request_id = Some("serialized".into());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events[0]["type"], "accepted");
    assert_eq!(events.last().unwrap()["type"], "summary");
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event["sequence"], i + 1);
        assert_eq!(event["request_id"], "serialized");
        assert_eq!(event.as_object().unwrap().len(), 8);
    }
    let terminal = &events.last().unwrap()["data"];
    assert_eq!(
        terminal["counts"]["similar_pairs"],
        flag["data"]["counts"]["similar_pairs"]
    );
    assert_eq!(terminal["counts"]["cache_hits"], 2);
    assert_eq!(terminal["counts"]["bytes_read"], 0);
    let (code, page) = json_output(
        &data,
        &[
            "results",
            "--run",
            terminal["run_id"].as_str().unwrap(),
            "--kind",
            "pairs",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 1);
}

#[test]
fn malformed_duplicate_and_incompatible_json_are_preacceptance_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    for bytes in [
        br#"{"schema_version":1,"schema_version":1}"#.as_slice(),
        br#"{"schema_version":1,"operation":"index","matching":null}"#,
        br#"{"schema_version":1,"operation":"compare","sources":null}"#,
        br#"{} trailing"#,
    ] {
        let path = tmp.path().join("request.json");
        fs::write(&path, bytes).unwrap();
        let (code, error) = json_output(&data, &["run", "--request", path.to_str().unwrap()]);
        assert_eq!(code, 2);
        assert_eq!(error["type"], "error");
        assert!(error["job_id"].is_null());
    }
}

#[test]
fn sigint_and_sigterm_checkpoint_before_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let input = large_input(tmp.path());
    for (sig, expected) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let data = tmp.path().join(format!("data-{sig}"));
        let mut child = command(&data)
            .arg("index")
            .arg(&input)
            .args(["--experimental-text", "--format", "jsonl"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let accepted: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(accepted["type"], "accepted");
        signal(&child, sig);
        assert_eq!(wait_bounded(&mut child).code(), Some(expected));
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        let terminal: Value = serde_json::from_str(rest.lines().last().unwrap()).unwrap();
        assert_eq!(terminal["type"], "summary");
        assert_eq!(terminal["data"]["status"], "cancelled");
        let status = Catalog::open_read_only(&data)
            .unwrap()
            .status(StatusQuery {
                schema_version: 1,
                job_id: accepted["job_id"].as_str().unwrap().into(),
                request_id: None,
            })
            .unwrap();
        assert_eq!(status["owner_active"], false);
        assert_eq!(status["resumable"], true);
    }
}

#[test]
fn closed_output_cancels_and_retains_durable_error() {
    let tmp = tempfile::tempdir().unwrap();
    let input = large_input(tmp.path());
    let data = tmp.path().join("data");
    let mut child = command(&data)
        .arg("index")
        .arg(input)
        .args(["--experimental-text", "--format", "jsonl"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let accepted: Value = serde_json::from_str(&line).unwrap();
    drop(reader);
    assert_eq!(wait_bounded(&mut child).code(), Some(1));
    let status = Catalog::open_read_only(&data)
        .unwrap()
        .status(StatusQuery {
            schema_version: 1,
            job_id: accepted["job_id"].as_str().unwrap().into(),
            request_id: None,
        })
        .unwrap();
    assert_eq!(status["status"], "cancelled");
    assert_eq!(status["summary"]["error"]["code"], "output_closed");
}

#[test]
fn a_stalled_output_consumer_does_not_block_cancellation() {
    let tmp = tempfile::tempdir().unwrap();
    let input = large_input(tmp.path());
    let data = tmp.path().join("data");
    let mut request = JobRequest::text_index(std::iter::repeat_n(input, 1500));
    request.request_id = Some("stalled".into());
    let path = tmp.path().join("request.json");
    fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
    let mut child = command(&data)
        .arg("run")
        .arg("--request")
        .arg(path)
        .args(["--format", "jsonl"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read a prefix to establish that acceptance began, then leave the pipe full.
    let mut stdout = child.stdout.take().unwrap();
    let mut prefix = [0; 128];
    stdout.read_exact(&mut prefix).unwrap();
    signal(&child, libc::SIGTERM);
    let status = wait_bounded(&mut child);
    assert_eq!(status.code(), Some(1));
    drop(stdout);
}

#[test]
fn configuration_merges_per_operation_and_flags_win() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("input.txt");
    fs::write(&input, "hello configuration").unwrap();
    let data = tmp.path().join("data");
    let profile = filetwin_core::profile::text_profile().profile_id;
    let path = tmp.path().join("config.toml");
    fs::write(&path,format!("[defaults.profiles]\ntext = {profile:?}\n[defaults.matching.threshold_overrides]\n{profile:?} = 0.7\n[defaults.limits]\nresult_bytes = 1\n")).unwrap();
    let output = command(&data)
        .arg("--config")
        .arg(path)
        .arg("index")
        .arg(input)
        .args(["--result-bytes", "100000", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["data"]["counts"]["files_ready"], json!(1));
}

#[test]
fn a_killed_owner_is_detected_and_the_job_can_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let input = large_input(tmp.path());
    let data = tmp.path().join("data");
    let mut child = command(&data)
        .arg("index")
        .arg(&input)
        .args(["--experimental-text", "--format", "jsonl"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let accepted: Value = serde_json::from_str(&line).unwrap();
    child.kill().unwrap();
    wait_bounded(&mut child);
    let job = accepted["job_id"].as_str().unwrap();
    let (code, status) = json_output(&data, &["status", "--job", job]);
    assert_eq!(code, 0);
    assert_eq!(status["data"]["status"], "interrupted");
    assert_eq!(status["data"]["resumable"], true);
    fs::write(input, "A small file after the interrupted attempt.").unwrap();
    let (code, resumed) = json_output(&data, &["resume", "--job", job]);
    assert_eq!(code, 0);
    assert_eq!(resumed["data"]["attempt_id"], 2);
    assert_eq!(resumed["data"]["status"], "completed");
}
