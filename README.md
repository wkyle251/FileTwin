# FileTwin

FileTwin encodes a local directory and returns **each file's content ID and vector**.
Use its Rust CLI from any application, or embed the `filetwin-core` Rust library.
An optional JSON vector file reuses previous encodings. There is no database,
threshold configuration, automatic grouping, or file-metadata modification.

This is the **0.2 developer preview**, with a simplified interface that replaces
the 0.1 scan/index/compare workflow. Encoding profiles still need accuracy
qualification on representative collections. Their existing `experimental-*`
names identify algorithms; no experimental opt-in parameter is required.

## Quick start

Build with the pinned Rust toolchain (1.98.1). Plain UTF-8 text needs only the CLI:

```sh
cargo build --locked --release --workspace --bins
./target/release/filetwin examples/text
```

The complete workflow is:

```sh
# Encode all files recursively; return one JSON result on stdout.
filetwin ./files

# Also save that result as a reusable vector file.
filetwin ./files --output vectors.json

# Reuse previous vectors and atomically update the saved file.
filetwin ./files vectors.json --output vectors.json
```

Use `./target/release/filetwin` in place of `filetwin` until the binaries are on
PATH. Install `filetwin-worker` beside the CLI for native formats. Native models
and libraries require the one-time setup below; processing never downloads them.

## Input and parameters

```text
filetwin <DIRECTORY> [VECTORS_FILE] [OPTIONS]
```

| Input / option | Meaning | Default |
| --- | --- | --- |
| `DIRECTORY` | One directory, scanned recursively, including hidden files | Required |
| `VECTORS_FILE` | JSON returned/saved by an earlier run | No saved vectors |
| `-o, --output FILE` | Also save the returned JSON; replace only a valid FileTwin vector file | stdout only |
| `--model-dir DIR` | Provisioned native model and runtime directory | `FILETWIN_MODEL_DIR`, otherwise `.filetwin/models` |
| `--backend cpu\|coreml\|cuda` | Image/video inference backend | `cpu` |
| `--workers N` | Concurrent native files, 1–64, subject to memory admission | 2; CUDA: 1 |

Paths may be relative in the CLI. Output parents must already exist. A vector
input is read-only unless also named by `--output`. The supplied input/output
vector paths, model directory, and current private staging directory are excluded
from discovery. Symlinks and other nonregular entries are reported as skipped.
Unspecified files, including hidden metadata files, are attempted normally.

Do not redirect stdout onto a cache file that is also the input: shells truncate
redirected files before FileTwin starts. Use `--output` for safe replacement.
For example, `filetwin ./files vectors.json -o vectors.json > /dev/null` updates
the cache without displaying the large vector arrays.

## Output and saved vector file

**stdout contains one JSON object.** `--output` saves the same object, using
format `filetwin-vectors`, schema version `1`. There is one record per discovered
path, sorted by relative path. The following is an abbreviated example; real
vectors contain every component and IDs contain full SHA-256 digests:

```json
{
  "format": "filetwin-vectors",
  "schema_version": 1,
  "directory": "/absolute/files",
  "complete": true,
  "files": [
    {
      "path": "photo.jpg",
      "file_id": "sha256:<64 lowercase hex digits>",
      "bytes": 123456,
      "state": "ready",
      "family": "image",
      "profile_id": "sha256:<encoding-profile digest>",
      "vector": [0.012, -0.034],
      "vector_sha256": "<64 lowercase hex digits>",
      "reused": false,
      "extraction": {"coverage": "example; actual fields depend on reader"}
    }
  ],
  "summary": {
    "counts": {
      "files_discovered": 1,
      "files_processed": 1,
      "files_total": 1,
      "files_ready": 1,
      "files_failed": 0,
      "files_skipped": 0,
      "vectors_encoded": 1,
      "cache_hits": 0,
      "bytes_read": 123968,
      "bytes_hashed": 123456
    },
    "elapsed_seconds": 0.8,
    "backend": "cpu",
    "workers": 2,
    "cancelled": false
  }
}
```

The [vector-file schema](schemas/vector-file.schema.json) defines the complete
shape. A failed/unsupported record has `vector: null`, `vector_sha256: null`, and
an `error` with `code`, `stage`, `message`, and optional `details`. It retains
`file_id` when the entire original was readable and unchanged. Unreadable or
changing files may have a null ID. `profile_id` is present for ready vectors.
`complete` describes directory traversal; check states/counts for decoding failures.

Normal paths are UTF-8 strings. Non-UTF-8 POSIX names use
`{"encoding":"posix_bytes","base64":"..."}` to preserve their exact bytes.
The saved root and relative paths are labels; cache loading never opens old paths.
Saved vectors can therefore be reused with a renamed or relocated directory.

### Identity and reuse

- `file_id` is `sha256:` plus the SHA-256 of the **complete original bytes**.
  Moving/renaming preserves it. Changing bytes changes it. Identical copies and
  hardlinks share the ID but each path gets a record.
- A vector is reused only for the same `file_id` **and** `profile_id`. Switching
  image/video backend changes the profile and triggers encoding for those files.
- Every run reads and hashes current files, including cache hits. Saved vectors
  avoid decoding/model inference; reuse is not a zero-read operation. Identical
  content within one run is encoded once and also counted in `cache_hits`.
  Reused extraction provenance/timings describe the original encoding.
- Newly added or changed files are encoded. Deleted files disappear from the
  returned list. Failed encodings are attempted again on the next run.
- Cache input/output files are limited to 512 MiB. Malformed schemas, unknown
  profiles, invalid dimensions, nonfinite/non-unit vectors, checksum errors, and
  conflicting entries are rejected. `vector_sha256` hashes little-endian float32
  component bytes. Use vector files from a trusted producer: these integrity
  checks do not authenticate who computed a vector.

Vectors remain in memory or in the JSON file you explicitly save. Source files
and their metadata are never modified. Saved files contain paths and content
representations; treat them as collection data when sharing or publishing.

## Progress and application integration

**stderr contains JSONL progress events**, including discovered/processed/ready/
failed/skipped counts, vectors encoded, cache hits, and source bytes read/hashed.
`files_total` is null during discovery and known after discovery finishes.
Stages are `discovering`, `encoding`, `completed`, and `cancelled`.

```text
{"type":"progress","data":{"stage":"encoding","counts":{...},"elapsed_seconds":1.2}}
```

This line is abbreviated; actual events contain every counter. Consume stderr
while the child runs and parse stdout when it exits. No percentage is calculated;
your app can derive one from `files_processed / files_total` when total is known.
Counters update at stage boundaries and about every 250 ms while work progresses.
`bytes_read` includes sniffing, hashing, and text re-reading; private-copy/model
reads are excluded. `bytes_hashed` counts original bytes fed to SHA-256.

SIGINT/SIGTERM request cancellation. FileTwin stops workers, returns the completed
records with `complete: false` and `summary.cancelled: true`, and saves that partial
result if requested. A later invocation can reuse those successful vectors.

| Exit code | Meaning |
| --- | --- |
| `0` | Traversal completed; all discovered entries encoded |
| `3` | Result returned with failed, unsupported, skipped, or incompletely discovered entries |
| `130` | Cancelled; a partial result may be returned |
| `2` | Invalid arguments/configuration or invalid vector file |
| `1` | Fatal I/O/runtime-output error; inspect stderr |

Fatal errors use `{"type":"error","error":{...}}` on stderr. They do not promise
a stdout result. If saving/output fails after encoding, stderr can contain a
completed progress event followed by that fatal error.

### Rust library

```rust
use filetwin_core::{Encoder, api::*, write_vectors};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let config = EncoderConfig::new(cwd.join(".filetwin/models"), std::env::temp_dir());
    let encoder = Encoder::new(config)?;
    let mut request = EncodeRequest::new(cwd.join("examples/text"));
    request.output_file = Some(cwd.join("vectors.json"));
    // Set request.vectors_file = Some(cwd.join("vectors.json")) to reuse a prior file.
    let result = encoder.encode(&request, &CancellationToken::default(), |p| {
        eprintln!("{} files processed", p.counts.files_processed);
    })?;
    write_vectors(request.output_file.as_deref().unwrap(), &result)?;
    // result.files contains file_id, profile_id, vector and per-file outcomes.
    Ok(())
}
```

The library requires absolute paths and explicit native paths in
`EncoderConfig.runtime`; it does not search PATH, install signal handlers, or
write to stdout/stderr. `encode` blocks; a host can put it on its own thread and
share a cloned `CancellationToken`. Each invocation owns its temporary resources.
See the runnable [host example](crates/filetwin-core/examples/host.rs).

### Similarity belongs to the caller

Compare ready vectors with **equal `profile_id`** using cosine:
`sum(a[i]*b[i]) / (norm(a)*norm(b))`. Rust callers can use
`filetwin_core::profile::cosine`. It returns a value in `[-1, 1]`; a displayed
95% cutoff commonly means `cosine >= 0.95`, not a 95% confidence estimate.
Equal dimensions alone do not make image/video or text/audio vectors compatible.
Your app can build a matrix, filter pairs, or choose a grouping policy without
encoding again. FileTwin itself returns no matrix, groups, or similarity scores.
Equal non-null content IDs identify byte-identical files even without vectors.

## Native setup and acceleration

```sh
# macOS: install FFmpeg 9 / ffprobe 9. On Linux provide a compatible FFmpeg 9 build.
brew install ffmpeg

# Build-time Python environment for the initial SSCD conversion.
uv venv --python 3.12 target/model-tools
uv pip install --python target/model-tools/bin/python --torch-backend cpu \
  -r scripts/model-requirements.txt
python3 scripts/setup-native.py --model-dir .filetwin/models \
  --conversion-python target/model-tools/bin/python

# Apple Silicon acceleration, after provisioning the same native assets:
filetwin ./files --backend coreml --output vectors.json
```

Python scripts only provision assets or run development checks. The deployed
application, its worker, all extraction, and inference run in Rust/native code;
Python and Docker are not runtime requirements. `setup-native.py --onnx PATH`
accepts an already converted, checksum-verified model without PyTorch. Keep the
installed third-party notices with redistributed assets.

On **Linux x86-64 with NVIDIA**, provision the CUDA provider, then select it:

```sh
python3 scripts/setup-native.py --model-dir .filetwin/models \
  --onnx /absolute/sscd_disc_mixup.onnx --onnxruntime-variant cuda12
filetwin ./files --backend cuda --output vectors.json
```

The target needs a compatible NVIDIA driver, CUDA 12 runtime, and cuDNN 9 supplied
separately. Setup verifies pinned ONNX Runtime 1.28.2, provider, PDFium, and model
artifacts. A requested accelerator that cannot initialize produces per-file
errors; it does not relabel CPU inference as GPU inference. An initialized provider
may execute unsupported graph operators on CPU. NVIDIA hardware performance and
numerical qualification remain pending; CPU and CoreML are exercised locally.

Native processes and model sessions are reused within each invocation. CoreML
compilation artifacts live in its private temporary directory and are removed on
return. Default shared native admission is 2 GiB memory and 10 GiB staging; these
are not hard whole-process/GPU memory limits. Rust hosts may adjust them.

Advanced deployment settings use environment variables, keeping the CLI small:

| Variable | Meaning |
| --- | --- |
| `FILETWIN_MODEL_DIR` | Default native asset directory |
| `FILETWIN_WORKER` | Companion executable; default is the CLI's sibling |
| `FILETWIN_FFMPEG`, `FILETWIN_FFPROBE` | Executable paths; CLI otherwise searches PATH |
| `FILETWIN_INFERENCE_THREADS` | ONNX intra-op CPU threads, 1–64; default 2 |
| `FILETWIN_CUDA_DEVICE` | Nonnegative CUDA device ID; default 0 |
| `FILETWIN_CUDA_LIBRARY_DIRS` | Colon-separated existing library directories passed to Linux workers |

## Supported file formats

All four content families are attempted automatically. Extensions are hints;
decoders validate the actual content. Support does not cover every variant of a
container or every codec in every FFmpeg build.

| Family | Supported inputs | Scope |
| --- | --- | --- |
| Text | Strict UTF-8 TXT, Markdown, CSV, JSON, XML, HTML, source code, logs/configuration | Source text including markup; UTF-8 BOM, line-ending and NFC normalization; 4,096 components |
| Documents (text) | DOCX; text-based PDF | DOCX main body; text from every PDF page; no OCR; 4,096 components |
| Raster images | JPEG/JPG, PNG, WebP, TIFF, GIF, BMP, ICO, PNM/PBM/PGM/PPM/PAM, TGA, DDS, QOI, farbfeld, Radiance HDR, OpenEXR | First image/frame, supported orientation metadata; 512 SSCD components |
| HEIF/AVIF images | **SDR HEIC, HEIF, HIF, AVIF** | Primary image or assembled tile grid with crop/rotation; FFmpeg 9; 512 components |
| Audio | WAV, MP3, FLAC, AAC, M4A, AIFF/AIF, Opus, OGG/OGA, MP2, WMA, WV, CAF, AU, AC3/EAC3, MKA, audio-only MP4 | First audio stream; mono 16 kHz; up to 32 eight-second windows across the recording; 4,096 components |
| Video | MP4, MOV, MKV, AVI, WebM, MPG/MPEG, TS/MTS/M2TS, FLV, WMV, 3GP | Up to 32 distinct frames across the timeline; visual content only; 512 components |

Images/video need SSCD and ONNX Runtime. PDFs need PDFium. HEIF/AVIF, audio, and
video need FFmpeg 9 and ffprobe 9. Every ready vector is normalized. The
[format coverage contract](format-coverage.md) lists exercised codecs and fixtures.

**HEIC/AVIF is supported for SDR images.** PQ/HLG **HDR** HEIF/AVIF and video remain
unsupported until a consistent tone-mapping profile is implemented and tested.
SDR and HDR may share the same filename extension. Separate HEIF auxiliary
alpha/depth composition is also unavailable. Radiance HDR/OpenEXR raster readers
are available under their own decoding policy.

Other content-similarity exclusions: scanned or mixed/blank-page PDFs needing
OCR; legacy Word DOC; XLS/XLSX/XLSB spreadsheets; PPT/PPTX presentations;
ODT/ODS/ODP; SVG rendering and dedicated camera-RAW decoding; UTF-16/UTF-32;
generic archives, executables and disk images; protected, corrupt, empty,
silent, unknown-duration or over-limit content. SVG/XML source may still encode
as UTF-8 text; that is not image rendering. These files get explicit outcomes and
null vectors. **Any fully readable unchanged regular file still gets a SHA-256
ID**, so callers can recognize exact copies without inventing semantic vectors.

## Platforms, migration and development

Target platforms are macOS 14+ Apple Silicon, and glibc Linux x86-64/ARM64
(Ubuntu 24.04/26.04 CI targets). CPU is portable across these targets. CoreML
requires Apple Silicon macOS; CUDA is provisioned for Linux x86-64. Windows,
Intel macOS, BSD and musl/Alpine are not qualified native-runtime targets. Docker
is optional packaging; it is not needed for local use or library embedding.

Version 0.2 removes the old database, snapshots, jobs, TOML request configuration,
query/export/matrix commands, and threshold/experimental flags. Existing 0.1
databases are neither opened nor deleted. Encode the directory once with
`--output` to create the new portable file. Source-based file IDs replace the old
filesystem IDs. Encoding profile IDs are preserved when their algorithms match.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -p filetwin-core --example schemas -- schemas --check
python3 scripts/smoke.py target/release/filetwin
python3 scripts/native-smoke.py target/release/filetwin --model-dir .filetwin/models
```

See [architecture](file-similarity-architecture.md), [implementation plan](implementation-plan.md),
and [native encoder details](native-encoding.md). Generated schemas cover vector
files, progress, errors and profiles. Samples and private reports belong under
ignored local directories, not source control.

## License

FileTwin is available under the [PolyForm Noncommercial License 1.0.0](LICENSE)
for permitted noncommercial use, including personal and educational use. Commercial
use requires separate permission. This is a source-available noncommercial
license, not an OSI-approved open-source license. Third-party code/models retain
their own terms; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
