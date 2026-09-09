use clap::Parser;
use filetwin_core::{
    Encoder, Error, Result,
    api::{CancellationToken, EncodeRequest, EncoderConfig},
    profile::Backend,
    write_vectors,
};
use serde_json::json;
use std::{
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Parser)]
#[command(
    name = "filetwin",
    version,
    about = "Encode a directory and return file IDs with vectors",
    after_help = "JSON vectors go to stdout; progress counters go to stderr as JSONL.\nThe optional VECTORS_FILE reuses vectors after verifying current file contents.\nNo database, thresholds or experimental flags are required."
)]
struct Args {
    /// Directory to read recursively, including hidden files.
    directory: PathBuf,
    /// A vector JSON file from a previous run (optional).
    vectors_file: Option<PathBuf>,
    /// Also save the returned JSON atomically; existing FileTwin vector files may be replaced.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Native model/runtime assets (default: FILETWIN_MODEL_DIR or .filetwin/models).
    #[arg(long, value_name = "DIR")]
    model_dir: Option<PathBuf>,
    /// Image/video inference backend; CUDA requires Linux x86-64 and NVIDIA dependencies.
    #[arg(long, default_value = "cpu", value_parser = ["cpu", "coreml", "cuda"])]
    backend: String,
    /// Concurrent native files (default: 2; CUDA: 1).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=64))]
    workers: Option<u32>,
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => (),
            Component::ParentDir => {
                result.pop();
            }
            value => result.push(value.as_os_str()),
        }
    }
    result
}
fn executable(name: &str, variable: &str, cwd: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(variable) {
        return Some(absolute(Path::new(&path), cwd));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| absolute(&dir.join(name), cwd))
            .find(|path| path.is_file())
    })
}
fn event(value: &serde_json::Value) -> std::io::Result<()> {
    let mut stderr = std::io::stderr().lock();
    serde_json::to_writer(&mut stderr, value)?;
    stderr.write_all(b"\n")
}
fn run(args: Args) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let models = args
        .model_dir
        .or_else(|| std::env::var_os("FILETWIN_MODEL_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".filetwin/models"));
    let mut config = EncoderConfig::new(absolute(&models, &cwd), std::env::temp_dir());
    config.backend = serde_json::from_value(json!(args.backend))?;
    config.workers =
        args.workers
            .map(|n| n as usize)
            .unwrap_or(if config.backend == Backend::Cuda {
                1
            } else {
                2
            });
    config.runtime.worker_path = std::env::var_os("FILETWIN_WORKER")
        .map(|p| absolute(Path::new(&p), &cwd))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.with_file_name("filetwin-worker"))
        });
    config.runtime.ffmpeg_path = executable("ffmpeg", "FILETWIN_FFMPEG", &cwd);
    config.runtime.ffprobe_path = executable("ffprobe", "FILETWIN_FFPROBE", &cwd);
    let suffix = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    config.runtime.onnxruntime_path = Some(
        config
            .model_dir
            .join(format!("runtime/libonnxruntime.{suffix}")),
    );
    config.runtime.pdfium_path = Some(config.model_dir.join(format!("runtime/libpdfium.{suffix}")));
    if let Ok(value) = std::env::var("FILETWIN_INFERENCE_THREADS") {
        config.runtime.inference_threads = value
            .parse()
            .map_err(|_| Error::invalid("FILETWIN_INFERENCE_THREADS must be an integer"))?;
    }
    if let Ok(value) = std::env::var("FILETWIN_CUDA_DEVICE") {
        config.runtime.cuda_device_id = value
            .parse()
            .map_err(|_| Error::invalid("FILETWIN_CUDA_DEVICE must be a nonnegative integer"))?;
    }
    if let Some(paths) = std::env::var_os("FILETWIN_CUDA_LIBRARY_DIRS") {
        config.runtime.cuda_library_dirs = std::env::split_paths(&paths)
            .map(|p| absolute(&p, &cwd))
            .collect();
    }
    let request = EncodeRequest {
        directory: absolute(&args.directory, &cwd),
        vectors_file: args.vectors_file.map(|p| absolute(&p, &cwd)),
        output_file: args.output.map(|p| absolute(&p, &cwd)),
    };
    let encoder = Encoder::new(config)?;
    let cancel = CancellationToken::default();
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])?;
    let signal_handle = signals.handle();
    let interrupted = cancel.clone();
    let listener = std::thread::spawn(move || {
        for _ in signals.forever() {
            interrupted.cancel();
        }
    });
    let result = encoder.encode(&request, &cancel, |p| {
        if event(&json!({"type":"progress","data":p})).is_err() {
            cancel.cancel();
        }
    });
    signal_handle.close();
    let _ = listener.join();
    let result = result?;
    if let Some(path) = &request.output_file {
        write_vectors(path, &result)?;
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &result)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(result.exit_code())
}
fn main() {
    let outcome = match Args::try_parse() {
        Ok(args) => run(args),
        Err(e)
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = e.print();
            return;
        }
        Err(e) => Err(Error::invalid(e.to_string())),
    };
    let code = match outcome {
        Ok(code) => code,
        Err(error) => {
            let _ = event(&json!({"type":"error","error":error}));
            error.exit_code()
        }
    };
    std::process::exit(code);
}
