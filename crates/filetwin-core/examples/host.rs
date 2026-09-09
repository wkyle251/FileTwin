//! Run: cargo run -p filetwin-core --example host -- /absolute/directory [/absolute/vectors.json]
use filetwin_core::{
    Encoder,
    api::{CancellationToken, EncodeRequest, EncoderConfig},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let directory = std::fs::canonicalize(args.next().ok_or("Supply a directory")?)?;
    let cwd = std::env::current_dir()?;
    let mut request = EncodeRequest::new(directory);
    request.vectors_file = args.next().map(std::fs::canonicalize).transpose()?;
    let mut config = EncoderConfig::new(cwd.join(".filetwin/models"), std::env::temp_dir());
    config.runtime.worker_path = Some(cwd.join("target/release/filetwin-worker"));
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
    // The host chooses how to find native executables; the library does not
    // inspect the environment. This example supports these explicit overrides.
    config.runtime.ffmpeg_path = std::env::var_os("FILETWIN_FFMPEG")
        .map(std::fs::canonicalize)
        .transpose()?;
    config.runtime.ffprobe_path = std::env::var_os("FILETWIN_FFPROBE")
        .map(std::fs::canonicalize)
        .transpose()?;
    // Supply both for HEIC/AVIF, audio and video.
    // Select profile::Backend::Coreml or ::Cuda explicitly for GPU inference.
    let encoder = Encoder::new(config)?;
    let result = encoder.encode(&request, &CancellationToken::default(), |progress| {
        eprintln!(
            "{} processed, {} encoded, {} reused",
            progress.counts.files_processed,
            progress.counts.vectors_encoded,
            progress.counts.cache_hits
        );
    })?;
    for file in &result.files {
        println!(
            "{:?}: {:?}, {} vector components",
            file.path,
            file.file_id,
            file.vector.as_ref().map_or(0, Vec::len)
        );
    }
    Ok(())
}
