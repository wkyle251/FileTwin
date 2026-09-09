mod args;
mod config;
mod output;

use args::{Cli, Command, Format};
use clap::{CommandFactory, Parser};
use config::Configuration;
use filetwin_core::{
    Catalog, Engine, Error, ErrorCode, HostServices, JobHandle, Result, api::*, capabilities,
    profile,
};
use output::{Output, Signals};
use serde_json::json;
use std::{
    io::IsTerminal,
    process::ExitCode,
    time::{Duration, Instant},
};

fn main() -> ExitCode {
    ExitCode::from(entry() as u8)
}

fn entry() -> i32 {
    let raw: Vec<_> = std::env::args_os().collect();
    let parsed = Cli::try_parse_from(&raw);
    let default_format = if std::io::stdout().is_terminal() {
        Format::Human
    } else {
        Format::Jsonl
    };
    let format = parsed
        .as_ref()
        .ok()
        .and_then(|c| c.format)
        .unwrap_or_else(|| {
            raw.iter()
                .enumerate()
                .find_map(|(i, s)| {
                    s.to_str().and_then(|s| {
                        if let Some(v) = s.strip_prefix("--format=") {
                            format_name(v)
                        } else if s == "--format" {
                            raw.get(i + 1)
                                .and_then(|v| v.to_str())
                                .and_then(format_name)
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or(default_format)
        });
    let signals = match Signals::install() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", e.message);
            return 1;
        }
    };
    let request_id = parsed
        .as_ref()
        .ok()
        .and_then(|c| c.request_id.clone())
        .unwrap_or_else(|| format!("request_{}", uuid::Uuid::new_v4()));
    let mut output = match Output::new(format, request_id, signals.received.clone()) {
        Ok(o) => o,
        Err(_) => return 1,
    };
    let cli = match parsed {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                return if output.raw(&error.to_string()).is_ok() {
                    0
                } else {
                    1
                };
            }
            let _ = output.emit("error", json!(Error::invalid(error.to_string())));
            return 2;
        }
    };
    if cli.command.is_none() {
        return if output
            .raw(&format!("{}\n", Cli::command().render_long_help()))
            .is_ok()
        {
            0
        } else {
            1
        };
    }
    match execute(&cli, &signals, &mut output) {
        Ok(code) => code,
        Err(error) => {
            let code = if signals.cancelled() && error.code == ErrorCode::Cancelled {
                signals.exit_code()
            } else {
                error.exit_code()
            };
            if error.code == ErrorCode::OutputClosed {
                return 1;
            }
            if output.emit("error", json!(error)).is_err() {
                1
            } else {
                code
            }
        }
    }
}
fn format_name(value: &str) -> Option<Format> {
    match value {
        "human" => Some(Format::Human),
        "json" => Some(Format::Json),
        "jsonl" => Some(Format::Jsonl),
        _ => None,
    }
}

fn execute(cli: &Cli, signals: &Signals, output: &mut Output) -> Result<i32> {
    let config = Configuration::load(cli)?;
    let command = cli.command.as_ref().expect("Checked command");
    let request_id = output.request_id.clone();
    let request = match command {
        Command::Scan(args) => Some(config.flag_request(
            Some(&args.input),
            Some(&args.matching),
            None,
            "scan",
            None,
            &request_id,
        )?),
        Command::Index(args) => {
            Some(config.flag_request(Some(args), None, None, "index", None, &request_id)?)
        }
        Command::Compare(args) => Some(config.flag_request(
            None,
            Some(&args.matching),
            Some(&args.limits),
            "compare",
            Some(&args.snapshot),
            &request_id,
        )?),
        Command::Group(args) => {
            let matching = args::MatchingArgs {
                all_scores: false,
                threshold: args.threshold.clone(),
                pair_scope: None,
                retrieval: None,
            };
            let mut request = config.flag_request(
                None,
                Some(&matching),
                Some(&args.limits),
                "group",
                Some(&args.run),
                &request_id,
            )?;
            request.source_revision = args.revision;
            Some(request)
        }
        Command::Run { request } => {
            if cli.request_id.is_some() {
                return Err(Error::invalid(
                    "run takes request_id from the JSON request; omit --request-id",
                ));
            }
            Some(config.json_request(request, &request_id)?)
        }
        _ => None,
    };
    if let Some(request) = request {
        if let Some(id) = &request.request_id {
            output.request_id = id.clone();
        }
        let engine = Engine::open(config.engine, HostServices::default())?;
        let handle = engine.submit(request)?;
        let result = drive(&handle, cli, signals, output);
        let shutdown = engine.shutdown();
        return result.and_then(|code| {
            shutdown?;
            Ok(code)
        });
    }
    match command {
        Command::Resume { job } => {
            let engine = Engine::open(config.engine, HostServices::default())?;
            let handle = engine.resume(job)?;
            let result = drive(&handle, cli, signals, output);
            let shutdown = engine.shutdown();
            result.and_then(|code| {
                shutdown?;
                Ok(code)
            })
        }
        Command::Status { job } => {
            let value = Catalog::open_read_only(config.engine.data_dir)?.status(StatusQuery {
                schema_version: 1,
                job_id: job.clone(),
                request_id: Some(request_id),
            })?;
            output.job_id = Some(job.clone());
            output.run_id = value["run_id"].as_str().map(str::to_owned);
            output.emit("status", value)?;
            Ok(0)
        }
        Command::Results(args) => {
            let page = Catalog::open_read_only(config.engine.data_dir)?.results(ResultsQuery {
                schema_version: 1,
                request_id: Some(request_id),
                run_id: args.target.run.clone(),
                snapshot_id: args.target.snapshot.clone(),
                kind: args.kind.clone(),
                group_id: args.group.clone(),
                file_id: args.file.clone(),
                result_revision: args.target.revision,
                cursor: args.cursor.clone(),
                page_size: args.page_size,
                min_score: args.min_score,
            })?;
            output.run_id = page.run_id.clone();
            output.emit("page", json!(page))?;
            Ok(0)
        }
        Command::Matrix(args) => {
            let page = Catalog::open_read_only(config.engine.data_dir)?.matrix_with_cancel(
                MatrixQuery {
                    schema_version: 1,
                    run_id: args.run.clone(),
                    result_revision: args.revision,
                    row_offset: args.row_offset,
                    column_offset: args.column_offset,
                    row_limit: args.row_limit,
                    column_limit: args.column_limit,
                },
                &|| signals.cancelled(),
            )?;
            output.run_id = Some(page.run_id.clone());
            output.emit("matrix", json!(page))?;
            Ok(0)
        }
        Command::Export(args) => {
            let manifest = Catalog::open_read_only(config.engine.data_dir)?.export_with_cancel(
                ExportRequest {
                    schema_version: 1,
                    request_id: Some(request_id),
                    run_id: args.target.run.clone(),
                    snapshot_id: args.target.snapshot.clone(),
                    result_revision: args.target.revision,
                    format: args.report_format.clone(),
                    directory: config::absolute(&args.report_dir, &config.cwd),
                },
                &|| signals.cancelled(),
            )?;
            output.run_id = args.target.run.clone();
            output.emit("export", manifest)?;
            Ok(0)
        }
        Command::Profiles { .. } => {
            output.emit("profiles", json!({"profiles":profile::profiles()}))?;
            Ok(0)
        }
        Command::Doctor => {
            let mut value = capabilities();
            value["paths"] = json!({"data_dir":config.engine.data_dir,"model_dir":config.engine.model_dir,"temp_dir":config.engine.temp_dir});
            value["runtime"] = json!(config.engine.runtime);
            value["runtime_files_present"] = json!({
                "worker":config.engine.runtime.worker_path.as_ref().is_some_and(|p| p.is_file()),
                "ffmpeg":config.engine.runtime.ffmpeg_path.as_ref().is_some_and(|p| p.is_file()),
                "ffprobe":config.engine.runtime.ffprobe_path.as_ref().is_some_and(|p| p.is_file()),
                "onnxruntime":config.engine.runtime.onnxruntime_path.as_ref().is_some_and(|p| p.is_file()),
                "pdfium":config.engine.runtime.pdfium_path.as_ref().is_some_and(|p| p.is_file()),
                "sscd_model":config.engine.model_dir.join(profile::SSCD_MODEL_FILE).is_file()
            });
            output.emit("capabilities", value)?;
            Ok(0)
        }
        _ => unreachable!("Processing commands handled above"),
    }
}

fn drive(handle: &JobHandle, cli: &Cli, signals: &Signals, output: &mut Output) -> Result<i32> {
    output.job_id = Some(handle.id().into());
    let events = handle.events();
    let interval = Duration::from_millis(cli.progress_interval_ms);
    let mut last_progress = Instant::now();
    let mut failure = None;
    loop {
        if failure.is_none() && output.closed() {
            handle.cancel_for_output_failure();
            failure = Some(Error::new(
                ErrorCode::OutputClosed,
                "output",
                "The output pipe closed",
            ));
        }
        if signals.cancelled() {
            handle.cancel();
        }
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => {
                if failure.is_none() {
                    let result = match event {
                        JobEvent::Accepted { run_id, data, .. } => {
                            output.run_id = run_id;
                            if output.format == Format::Jsonl {
                                output.emit("accepted", data)
                            } else {
                                Ok(())
                            }
                        }
                        JobEvent::Progress { run_id, data, .. } => {
                            output.run_id = run_id;
                            if last_progress.elapsed() >= interval {
                                last_progress = Instant::now();
                                if output.format == Format::Jsonl {
                                    output.emit("progress", data)
                                } else {
                                    if output.interactive_progress() {
                                        output.diagnostic(&format!(
                                            "{}: {} files ready",
                                            data["stage"].as_str().unwrap_or("processing"),
                                            data["counts"]["files_ready"]
                                        ));
                                    }
                                    Ok(())
                                }
                            } else {
                                Ok(())
                            }
                        }
                        JobEvent::Error { run_id, error, .. } => {
                            output.run_id = run_id;
                            if output.format == Format::Jsonl {
                                output.emit("error", json!(error))
                            } else {
                                if output.format == Format::Human && cli.log_level != "error" {
                                    output.diagnostic(&error.message);
                                }
                                Ok(())
                            }
                        }
                        JobEvent::Summary(_) => Ok(()), // wait() is authoritative even if events coalesce.
                    };
                    if let Err(error) = result {
                        handle.cancel_for_output_failure();
                        failure = Some(error);
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => (),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if handle.is_finished() && events.is_empty() {
            break;
        }
    }
    let summary = handle.wait()?;
    if let Some(error) = failure {
        return Err(error);
    }
    output.run_id = summary.run_id.clone();
    output.emit("summary", serde_json::to_value(&summary)?)?;
    if signals.cancelled() && summary.status == "cancelled" {
        Ok(signals.exit_code())
    } else {
        Ok(summary.exit_code())
    }
}
