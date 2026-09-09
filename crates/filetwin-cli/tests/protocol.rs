use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_filetwin"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FILETWIN_") {
            command.env_remove(key);
        }
    }
    command.current_dir(root);
    command
}
fn cli(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().unwrap()
}

fn tree(
    root: &Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Option<(Vec<u8>, std::time::SystemTime)>> {
    let mut result = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = path.symlink_metadata().unwrap();
            let value = if metadata.is_dir() {
                pending.push(path.clone());
                None
            } else {
                Some((fs::read(&path).unwrap(), metadata.modified().unwrap()))
            };
            result.insert(path.strip_prefix(root).unwrap().to_owned(), value);
        }
    }
    result
}

#[test]
fn repeated_runs_only_persist_the_explicit_output_file() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    fs::create_dir(root.join("input")).unwrap();
    fs::create_dir(root.join("scratch")).unwrap();
    fs::write(root.join("input/a.txt"), "a complete document").unwrap();
    let before = tree(root);
    let run = |args: &[&str], code| {
        result(
            &command(root)
                .env("TMPDIR", root.join("scratch"))
                .args(args)
                .output()
                .unwrap(),
            code,
        )
    };
    for _ in 0..3 {
        assert_eq!(run(&["input"], 0)["summary"]["counts"]["files_ready"], 1);
        assert_eq!(tree(root), before);
    }
    let first = run(&["input", "--output", "vectors.json"], 0);
    let saved = fs::read(root.join("vectors.json")).unwrap();
    assert_eq!(first, serde_json::from_slice::<Value>(&saved).unwrap());
    let with_output = tree(root);
    let mut others = with_output.clone();
    others.remove(Path::new("vectors.json"));
    assert_eq!(others, before);
    for _ in 0..3 {
        assert_eq!(
            run(&["input", "vectors.json"], 0)["summary"]["counts"]["cache_hits"],
            1
        );
        assert_eq!(tree(root), with_output); // Reuse input is read-only.
    }
    for _ in 0..2 {
        let reused = run(&["input", "vectors.json", "-o", "vectors.json"], 0);
        assert_eq!(reused["files"].as_array().unwrap().len(), 1);
        let mut after = tree(root);
        after.remove(Path::new("vectors.json"));
        assert_eq!(after, before); // No backups, history or extra caches.
    }
    fs::write(root.join("input/broken.png"), b"\x89PNG\r\n\x1a\ninvalid").unwrap();
    let before_failure = tree(root);
    assert_eq!(run(&["input"], 3)["summary"]["counts"]["files_failed"], 1);
    assert_eq!(tree(root), before_failure);
}
fn result(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn directory_json_and_saved_vectors_are_the_complete_cli_workflow() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    fs::create_dir(root.join("input")).unwrap();
    fs::write(root.join("input/a.txt"), "complete source document").unwrap();
    let first = cli(root, &["input", "--output", "input/vectors.json"]);
    let a = result(&first, 0);
    assert_eq!(a["format"], "filetwin-vectors");
    assert_eq!(a["schema_version"], 1);
    assert_eq!(a["files"][0]["vector"].as_array().unwrap().len(), 4096);
    assert!(
        a["files"][0]["file_id"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let saved: Value =
        serde_json::from_slice(&fs::read(root.join("input/vectors.json")).unwrap()).unwrap();
    assert_eq!(a, saved);
    let events: Vec<Value> = String::from_utf8(first.stderr)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(events.iter().all(|e| e["type"] == "progress"));
    assert_eq!(events[0]["data"]["stage"], "discovering");
    assert_eq!(
        events.last().unwrap()["data"]["counts"]["files_processed"],
        1
    );
    fs::rename(root.join("input/a.txt"), root.join("input/moved.txt")).unwrap();
    let b = result(
        &cli(
            root,
            &["input", "input/vectors.json", "-o", "input/vectors.json"],
        ),
        0,
    );
    assert_eq!(b["files"].as_array().unwrap().len(), 1);
    assert_eq!(b["files"][0]["file_id"], a["files"][0]["file_id"]);
    assert_eq!(b["files"][0]["vector"], a["files"][0]["vector"]);
    assert_eq!(b["summary"]["counts"]["cache_hits"], 1);
    assert!(!root.join(".filetwin").exists());
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn removed_parameters_and_invalid_inputs_are_structured_errors() {
    let t = tempfile::tempdir().unwrap();
    for args in [
        vec![],
        vec!["missing"],
        vec![".", "--threshold", "0.95"],
        vec![".", "--experimental"],
        vec![".", "--data-dir", "data"],
        vec![".", "--workers", "0"],
    ] {
        let output = cli(t.path(), &args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let events: Vec<Value> = String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(events.last().unwrap()["type"], "error");
    }
    assert_eq!(fs::read_dir(t.path()).unwrap().count(), 0);
}

#[test]
fn unsupported_bytes_have_ids_errors_and_null_vectors() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("binary"), b"\0\xff\x80").unwrap();
    let value = result(&cli(t.path(), &["."]), 3);
    assert_eq!(value["complete"], true);
    assert!(value["files"][0]["file_id"].is_string());
    assert!(value["files"][0]["vector"].is_null());
    assert!(value["files"][0]["error"]["code"].is_string());
}

#[test]
fn backend_selection_does_not_require_a_gpu_for_text() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("a.txt"), "plain UTF-8 text").unwrap();
    let value = result(&cli(t.path(), &[".", "--backend", "cuda"]), 0);
    assert_eq!(value["summary"]["backend"], "cuda");
    assert_eq!(value["summary"]["workers"], 1);
    let help = cli(t.path(), &["--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("<DIRECTORY> [VECTORS_FILE]")
    );
}

#[test]
fn signals_return_partial_results_and_save_only_when_requested() {
    use std::{
        os::unix::fs::PermissionsExt,
        process::Stdio,
        time::{Duration, Instant},
    };
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let input = root.join("input");
        fs::create_dir(&input).unwrap();
        fs::create_dir(root.join("scratch")).unwrap();
        fs::write(input.join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let models = root.join("models");
        fs::create_dir_all(models.join("runtime")).unwrap();
        fs::write(models.join("sscd_disc_mixup.onnx"), []).unwrap();
        let suffix = if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        fs::write(models.join(format!("runtime/libonnxruntime.{suffix}")), []).unwrap();
        let worker = root.join("worker");
        let pid_file = root.join("started");
        fs::write(
            &worker,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 120\n",
                pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        let output = root.join("vectors.json");
        let mut command = Command::new(env!("CARGO_BIN_EXE_filetwin"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("FILETWIN_") {
                command.env_remove(key);
            }
        }
        command
            .arg(&input)
            .arg("--model-dir")
            .arg(&models)
            .env("FILETWIN_WORKER", &worker)
            .env("TMPDIR", root.join("scratch"));
        if signal == libc::SIGTERM {
            command.arg("--output").arg(&output);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !pid_file.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "CLI exited before worker start"
            );
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: send the chosen cancellation signal only to our child CLI.
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        let response = result(&child.wait_with_output().unwrap(), 130);
        assert_eq!(response["complete"], false);
        assert_eq!(response["summary"]["cancelled"], true);
        assert_eq!(response["summary"]["counts"]["files_processed"], 0);
        if signal == libc::SIGTERM {
            assert_eq!(
                response,
                serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap()
            );
        } else {
            assert!(!output.exists());
        }
        assert_eq!(fs::read_dir(root.join("scratch")).unwrap().count(), 0);
    }
}
