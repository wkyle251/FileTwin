# FileTwin

FileTwin finds similar local text, documents, images, audio and video. It provides a native `filetwin`
CLI, JSON/JSONL output for other applications, and the `filetwin-core` Rust
library. It indexes content, saves immutable snapshots, and retains every
qualifying pair alongside conservative groups.

**Status: all four encoding families implemented, developer preview 0.1.0-dev.**
Profiles remain experimental; thresholds are required and are not accuracy
percentages. Unsupported formats and failed extraction produce explicit outcomes.
OCR, cloud sources, calibration and production-scale qualification remain in the
[implementation plan](implementation-plan.md).
The [architecture](file-similarity-architecture.md) describes the broader target.
See [Supported file formats](#supported-file-formats) for the current format list,
runtime requirements and exclusions.

**HEIC/AVIF are supported for standard dynamic range (SDR) images.** The remaining
restriction concerns HDR variants using PQ/HLG. See [HEIC/AVIF support](#heicavif-support).

## Build and try it

Install Rust through [rustup](https://rustup.rs/) and a native C compiler/linker
(Xcode Command Line Tools on macOS; a development toolchain on Ubuntu). The
workspace pins Rust 1.98.1. The CLI bundles SQLite; no models, GPU, Docker, or
runtime service are needed for text processing.

```sh
cargo build --locked --release -p filetwin-cli
./target/release/filetwin doctor --format json
./target/release/filetwin scan examples/text --data-dir .filetwin \
  --experimental-text --threshold 0.7 --format human
```

For all four encoding families, build the companion worker and explicitly
provision the selected SSCD model, ONNX Runtime and PDFium. Python/PyTorch are
used only to convert the model during setup. Production processing uses Rust,
the native libraries and FFmpeg; it never downloads anything.

```sh
cargo build --locked --release --workspace
# macOS: install FFmpeg 9 with Homebrew. Ubuntu: provide an FFmpeg 9/ffprobe 9 build.
brew install ffmpeg
# Build-only conversion environment (uv can install Python 3.12 if needed).
uv venv --python 3.12 target/model-tools
uv pip install --python target/model-tools/bin/python --torch-backend cpu \
  -r scripts/model-requirements.txt
python3 scripts/setup-native.py --model-dir .filetwin/models \
  --conversion-python target/model-tools/bin/python

./target/release/filetwin --data-dir .filetwin doctor --format json
./target/release/filetwin --data-dir .filetwin scan /absolute/path/to/files \
  --experimental --threshold 0.95 --format jsonl
```

The native assets are installed under `.filetwin/models` by this command.
`setup-native.py --onnx PATH` can install a previously converted,
verified artifact without PyTorch. Copy both executables and the provisioned
model directory when deploying; see [native-encoding.md](native-encoding.md).

`--experimental` selects the current text/document, image, audio and video
profiles. Use `--families image,video` to select only those families. A bare
threshold applies to every selected profile; use repeated family-specific
thresholds when appropriate:

```sh
./target/release/filetwin --data-dir .filetwin scan ./photos ./recordings \
  --experimental --families image,audio \
  --threshold image=0.95 --threshold audio=0.90
```

These cutoffs are user choices, not recommended calibrated values. For example,
cosine `0.95` does not mean a 95% probability that two files are duplicates.

## Supported file formats

The current experimental profiles support the following locally tested inputs
for **content-similarity encoding**. The decoder validates file contents; an
extension alone does not establish support. Select all four families with
`--experimental`; `--experimental-text` selects only the original UTF-8 reader.

| Family | Supported formats / common extensions | Processing scope |
| --- | --- | --- |
| Text | Strict UTF-8 text: `.txt`, `.md`, `.csv`, `.json`, `.xml`, `.html`, source code, logs and configuration files | Compare source text, including markup; preserve case and punctuation. Accept UTF-8 BOM and normalize line endings. |
| Documents (text family) | `.docx`; text-based `.pdf` | DOCX main body; PDF page text. No OCR. Every PDF page must contain extractable text. |
| Raster images | JPEG (`.jpg`, `.jpeg`), PNG, WebP, TIFF (`.tif`, `.tiff`), GIF, BMP, ICO, PNM (`.pbm`, `.pgm`, `.ppm`, `.pam`), TGA, DDS, QOI, farbfeld (`.ff`), Radiance HDR (`.hdr`), OpenEXR (`.exr`) | First image/frame for multipage or animated files; apply supported orientation metadata. Codec-specific restrictions apply. |
| HEIF/AVIF images | SDR `.heic`, `.heif`, `.hif`, `.avif` | Primary image or assembled tile grid, with cropping/rotation. Requires image profile v2 and FFmpeg 9. |
| Audio | `.wav`, `.mp3`, `.flac`, `.aac`, `.m4a`, `.aiff`/`.aif`, `.opus`, `.ogg`/`.oga`, `.mp2`, `.wma`, `.wv`, `.caf`, `.au`, `.ac3`, `.eac3`, `.mka`, audio-only `.mp4` | First audio stream; mono 16 kHz; up to 32 eight-second windows distributed across the recording. |
| Video | `.mp4`, `.mov`, `.mkv`, `.avi`, `.webm`, `.mpg`/`.mpeg`, `.ts`/`.mts`/`.m2ts`, `.flv`, `.wmv`, `.3gp` | First non-cover-art video stream; up to 32 frames across the timeline. Similarity measures visual content; audio and temporal order are excluded. |

Text/documents and audio produce 4,096-dimensional vectors. Images and video
produce 512-dimensional SSCD vectors. Every stored vector is normalized.

Native formats require the companion `filetwin-worker`. Images/video also need
the provisioned SSCD model and ONNX Runtime; PDFs need PDFium. HEIF/AVIF, audio
and video require FFmpeg 9 and ffprobe 9. Media codec availability depends on
the installed FFmpeg build; container support does not guarantee every codec
variant. Other recognized FFmpeg formats may decode, but the table above is the
tested format set. The [format coverage contract](format-coverage.md) records the
specific tested codecs and remaining qualification work.

### HEIC/AVIF support

| Input / profile | Current behavior |
| --- | --- |
| Standard dynamic range (SDR) HEIC/HEIF/AVIF with image profile v2 | **Supported.** FFmpeg 9 decodes the primary image or full tile grid and applies cropping/rotation before SSCD encoding. |
| HDR HEIC/HEIF/AVIF using PQ/HLG | **Currently unsupported.** A consistent brightness/color conversion to SDR (tone mapping) must be implemented and validated before encoding. |
| HEIC/HEIF/AVIF with the original image profile v1 | Unsupported by that older profile. It is retained so existing snapshots keep their original meaning. |

Both SDR and HDR images can use the same `.heic` or `.avif` extension. The HDR
restriction is determined from decoded media properties. It is an implementation
gap in HDR processing; it is not a blanket exclusion of HEIC/AVIF files.

`--experimental` selects image profile **v2**. Encoding SDR HEIC/AVIF requires the
configured FFmpeg/ffprobe paths plus SSCD and ONNX Runtime. Selecting v2 replaces
the current image cache binding after encoding; immutable results retain their
original profile. No source file or source metadata is modified.

### Currently unsupported for content similarity

| Category | Unsupported formats or conditions |
| --- | --- |
| Other document readers | Legacy Word `.doc`; spreadsheets such as `.xls`, `.xlsx`, `.xlsb`; presentations such as `.ppt`, `.pptx`; OpenDocument `.odt`, `.ods`, `.odp` |
| Scanned documents | Image-only/scanned PDFs requiring OCR; mixed PDFs with pages lacking extractable text, including blank pages |
| Other image readers | SVG rendering and dedicated camera-RAW decoding; separate HEIF auxiliary alpha/depth composition |
| HDR media | **Only the PQ/HLG HDR variants** of HEIF/AVIF and video are excluded by this restriction. SDR HEIC/AVIF is supported. A validated tone-mapping profile is required for these HDR inputs; the listed Radiance HDR/OpenEXR raster readers remain available. |
| Text encodings | UTF-16, UTF-32 and other non-UTF-8 text encodings |
| Other binary content | Generic archive traversal (`.zip`, `.rar`, `.7z`, etc.), binary executable analysis and disk images |
| Protected/unusable input | Encrypted/password-protected documents, unavailable codecs, malformed files, unknown media duration and inputs exceeding configured limits |

Empty text, silent audio and other insufficient content also produce explicit
per-file outcomes. Files that cannot be encoded receive an `unsupported`,
`failed` or `insufficient_content` outcome instead of a placeholder vector.
The job reports partial coverage and exits with code 3 when such outcomes occur,
while retaining usable results. Intentionally unselected families are excluded.

### Exact duplicates for any readable format

Use `--exact-duplicates compute` to include SHA-256 exact-copy evidence for
**any readable regular file** within the requested scope and limits, including
formats without a content decoder. Such files can join byte-identical groups
while retaining their explicit outcome for content similarity.

```sh
./target/release/filetwin --data-dir .filetwin scan /absolute/path/to/files \
  --experimental --threshold 0.95 --exact-duplicates compute --format jsonl
```

### Can unsupported files get vectors directly from their bytes?

FileTwin's existing vectors already come from file content. The pipeline is
**file bytes → decoded text, pixels or audio samples → normalized vector**.
Formats determine how those bytes must be decoded. Missing readers, OCR or HDR
conversion prevent the corresponding content representation from being produced.
Unrecognized extensions already fall back to strict UTF-8 text decoding when
no known binary format is detected.

A generic vector of byte-pattern features is technically possible without a
format decoder. It would measure **binary similarity**. For example, the same
photo saved as JPEG and HEIC can have substantially different encoded bytes;
a byte-pattern score cannot be assumed to reflect their visual similarity.
Compression, encryption and shared container headers also limit its usefulness.
Byte-level methods such as [ssdeep](https://ssdeep-project.github.io/ssdeep/)
illustrate this separate goal by matching shared sequences of bytes.

| Method | What it measures | FileTwin status |
| --- | --- | --- |
| Format-specific content vector | Similarity of extracted text, decoded images, recordings or footage | Implemented for the supported formats above |
| SHA-256 | Equality of original file bytes | Implemented with `--exact-duplicates compute` |
| Generic byte-pattern vector or fuzzy fingerprint | Similarity of encoded byte patterns | Possible addition; **not implemented** |

A future binary-similarity fallback would need a separate profile, explicit
selection, labeled results and independently evaluated thresholds. Its vectors
could not be compared directly with the existing text/image/audio/video vectors,
and a binary score of 0.95 would not establish 95% visual or textual similarity.
It would not add OCR, decrypt protected content or supply missing format readers.
Currently, unsupported files retain explicit outcomes and can still contribute
SHA-256 exact-copy evidence when requested.

## Inspect and export results

The `examples/text` fixture has three files, including two versions of a sentence with a small
punctuation edit. Use the returned IDs to inspect or export results:

```sh
./target/release/filetwin results --data-dir .filetwin \
  --run RUN_ID --kind groups --format json
./target/release/filetwin results --data-dir .filetwin \
  --run RUN_ID --kind members --group GROUP_ID --format json
./target/release/filetwin export --data-dir .filetwin \
  --run RUN_ID --report-format csv --report-dir ./report --format json
```

Replace `RUN_ID` and `GROUP_ID` with actual IDs. The export destination must be a
new directory whose parent exists. Reports include files, locations, groups,
members, pairs, errors, a summary, and a checksummed manifest. JSON, JSONL, and
CSV exports stream from persisted records. CSV includes `record_json` to retain
the complete record, including nested and lossless path fields.

## Commands and important parameters

| Command | Input | Output |
| --- | --- | --- |
| `scan PATH…` | Files/directories, explicit profile, threshold | Job summary, immutable snapshot and comparison run |
| `index PATH…` | Files/directories and explicit profile | Job summary and snapshot; no matching |
| `compare --snapshot ID` | Saved snapshot and threshold | A new run without reading originals |
| `run --request FILE` | Versioned JSON; `-` reads one request from stdin | Same pipeline as `scan`, `index`, or `compare` |
| `resume --job ID` | Cancelled/interrupted job with unchanged settings | New attempt of the same job |
| `status --job ID` | Existing job | Durable state, ownership, counts and resumability |
| `results --run ID --kind KIND` | Run/revision and page selection | One bounded result page |
| `results --snapshot ID --kind KIND` | Snapshot and file/location/error selection | One bounded result page |
| `export --run ID --report-format FORMAT --report-dir DIR` | Published run; `--snapshot` is also accepted | Report directory and manifest |
| `profiles list` | No index needed | Available immutable profile manifests |
| `doctor` | No index needed | Build capabilities and resolved paths |

`scan` and `index` accept multiple roots, recursively by default. Useful options:

| Parameter | Meaning / default |
| --- | --- |
| `--experimental` | Select current experimental profiles for all four families |
| `--experimental-text` | Keep the original UTF-8-only profile for compatibility |
| `--families text,image,audio,video` | Restrict selected families; must match explicit profile selection |
| `--profile FAMILY=PROFILE_ID` | Repeat to select exact IDs from `profiles list` |
| `--threshold [FAMILY\|PROFILE_ID=]SCORE` | Required for every selected profile; finite cosine cutoff in `[-1, 1]` |
| `--pair-scope all_selected\|within_each_source` | Compare across selected roots, or only shared source memberships |
| `--no-recursive` | Limit directory discovery to immediate files |
| `--include GLOB`, `--exclude GLOB` | Root-relative, case-sensitive patterns; literals, `*`, `?`, `**`; exclusions win |
| `--extensions txt,md` | Case-insensitive extension filter; no leading dots |
| `--exclude-hidden` | Exclude dot-prefixed path components; hidden entries are included by default |
| `--min-bytes N`, `--max-bytes N` | Inclusive file-size filters |
| `--cache-mode reuse\|refresh` | Reuse verified cached vectors, or encode again |
| `--validation fast\|strict` | Fast uses heuristic metadata checks; strict reports unavailable for each eligible file |
| `--exact-duplicates reuse_known\|compute\|off` | Reuse known SHA-256 evidence, hash eligible content, or suppress byte-identical matching |
| `--memory-bytes N` | Planning allowance; default 2 GiB, minimum 64 MiB |
| `--staging-bytes N` | Default 10 GiB; native input must fit with a 2 MiB protocol reserve |
| `--worker-path PATH` | Companion executable; CLI defaults to sibling `filetwin-worker` |
| `--onnxruntime-path PATH`, `--pdfium-path PATH` | Pinned libraries; default under `MODEL_DIR/runtime` |
| `--ffmpeg-path PATH`, `--ffprobe-path PATH` | Absolute runtime paths; CLI can resolve them from PATH |
| `--result-bytes N` | Cumulative work-record allowance; default 1 GiB; see preview limits below |
| `--max-runtime-seconds N\|none` | Accumulated processing time; unlimited by default |
| `--format human\|json\|jsonl` | Console transport; human on a terminal, JSONL otherwise |
| `--request-id TEXT` | Correlation ID for flags/queries; `run` takes it from JSON |
| `--progress-interval-ms N` | Event throttle; default 1000 ms |

Run each command with `--help` for all parameters. `--io-workers` and
`--inference-workers` are accepted upper bounds; this preview uses one encoder
and one reference scorer. Native encoding stages one file at a time from its
already opened source; the source is checked again before saving its vector.
Staging is removed after success, error or cancellation. Native workers have a
300-second per-file deadline, also bounded by the job's remaining time. No
network content is fetched. Image/video admission requires 512 MiB; native
document/audio admission requires 128 MiB. Large decoded frames can require more.
Sidecar enable flags return `unsupported_capability`. `index` rejects matching
parameters. `compare` accepts only memory, result, and time limits and uses the
snapshot's original profile, scope memberships, digests, and extraction outcomes.

`--kind` is one of `summary`, `groups`, `members`, `pairs`, `files`, `locations`,
or `errors`. Members require `--group`; locations require `--file`. Snapshot
queries support files, locations, and errors. Pages default to 100 records,
at most 1,000, and have a 4 MiB serialized-record ceiling. Reuse `next_cursor`
with the same selectors; it pins the original result revision. Summary queries
reject paging options. All qualifying pairs remain available, including pairs
that cross the final group boundaries.

## Calling from another application

Prefer an argument array when launching the CLI. Processing stays in the
foreground and never prompts. Edit the absolute source path in
[native-scan-request.json](examples/native-scan-request.json) for all four
families, or [scan-request.json](examples/scan-request.json) for text only,
then run:

```sh
./target/release/filetwin run --request examples/native-scan-request.json \
  --data-dir .filetwin --format jsonl --non-interactive
```

Each machine response has exactly these envelope fields:

```json
{
  "schema_version": 1,
  "invocation_id": "invocation_...",
  "request_id": "your-correlation-id",
  "sequence": 1,
  "type": "summary",
  "job_id": "job_...",
  "run_id": "run_...",
  "data": {}
}
```

`data` contains the actual typed summary/page/error, rather than all matching
files. JSON emits one terminal response. JSONL emits `accepted`, optional
progress/errors, then one `summary`. Queries emit one response. Treat an
accepted event without a summary as incomplete. Inspect both completeness and
the process exit code; a completed job can have failed or unsupported files.

| Exit code | Meaning |
| ---: | --- |
| 0 | Complete success under the declared mode, or successful query/export |
| 1 | Operational/storage/output failure |
| 2 | Invalid request, parameters, configuration, or unsupported capability |
| 3 | Partial coverage or an exhausted processing allowance |
| 4 | Another engine or job owns processing |
| 130 / 143 | Cooperative SIGINT / SIGTERM cancellation |

Drain stdout and stderr concurrently. SIGINT/SIGTERM request a checkpoint;
a second signal can force exit. A closed pipe cancels processing with
`output_closed`. A stalled pipe remains bounded and can be interrupted; it may
end without a complete final envelope. Query the accepted job ID afterward.

Requests are limited to 8 MiB. Duplicate JSON keys, unknown fields and incompatible
operation fields are rejected. JSON paths must be absolute; CLI path arguments
can be relative. POSIX paths that cannot be represented as UTF-8 use
`local_path: {"encoding":"posix_bytes","base64":"…"}` instead of `root`.
The underlying filesystem must support those filename bytes.

The [schemas](schemas/) use JSON Schema Draft 2020-12. Runtime validation also
checks installed profiles, thresholds, source availability, ownership, budgets,
and selector compatibility. The protocol and Rust API are preview contracts and
may change before a stable release; schema/database versions are explicit.

For a Rust host, add `filetwin-core` as a path dependency. Enable its
`bundled-sqlite` feature or provide a compatible system SQLite development
library. See the runnable [host example](crates/filetwin-core/examples/host.rs):

```rust
use filetwin_core::{Engine, HostServices, api::{EngineConfig, JobRequest}};

let engine = Engine::open(EngineConfig::new(data_dir), HostServices::default())?;
let handle = engine.submit(JobRequest::text_scan([source_dir], 0.7))?;
let summary = handle.wait()?;
engine.shutdown()?;
```

Supply absolute `PathBuf` values. `JobRequest::from_json` applies strict JSON
presence/duplicate validation when embedding a serialized caller. `JobHandle`
exposes bounded events, cancellation and an authoritative `wait()` result.
Cloned event receivers compete for messages. `Catalog::open_read_only` permits
queries while an engine owns processing; open one catalog per querying thread.
The library does not inspect CLI configuration/environment, install signals,
write to standard streams, change directory, or terminate the host. Diagnostic
callbacks run on the worker: keep them short and avoid synchronous waits on
that same worker.

For all-family encoding, set `EngineConfig.runtime: RuntimeConfig` with absolute
paths for `worker_path`, `onnxruntime_path`, `pdfium_path`, `ffmpeg_path` and
`ffprobe_path`, and point `model_dir` at the provisioned models. Then use
`JobRequest::experimental_scan(paths, cutoff)` or
`JobRequest::experimental_index(paths)`. Each runtime field is optional so hosts
can configure only the encoders they use. Rust hosts resolve these paths
themselves; the core does not read PATH or environment variables. The native
types stay inside `filetwin-worker`.

## Configuration and persistence

Use `--config FILE` or `FILETWIN_CONFIG` for an explicit TOML file; none is loaded
implicitly. See [filetwin.toml](examples/filetwin.toml). Precedence is command
parameters, `FILETWIN_DATA_DIR`/`FILETWIN_MODEL_DIR`/`FILETWIN_TEMP_DIR`, config,
then built-in defaults. Config engine paths are relative to the config file.
Defaults irrelevant to index/compare are removed before merging the request.
Explicit `profiles` and `threshold_overrides` maps replace the configured maps.
Native paths can also be supplied under `[engine.runtime]`; see
[native-config.toml](examples/native-config.toml).

The default data directory is `~/Library/Application Support/FileTwin` on macOS,
or `$XDG_DATA_HOME/filetwin` (fallback `~/.local/share/filetwin`) on Linux. Keep
the working index on a local filesystem. Only one processing engine can own a
data directory. SQLite uses WAL, FULL synchronization, and foreign keys.
Unknown/nonempty databases and incompatible schema versions are refused.

Sources are opened without following the root entry or discovered symlinks.
Aliases in the explicitly supplied root's **parent** are resolved once, allowing
paths such as `/var/folders/...` on macOS. The resolved root is saved in the
accepted request. Hard links share a file object/vector and retain all locations;
independent copies retain distinct file IDs. Filesystem identity evidence is
recorded; the ctime fallback can reduce reuse when birth time is unavailable.

Current cache bindings are mutable. Published snapshots and result revisions
are immutable. Comparing a snapshot never reads the original files; it also
does not assert that they are still current. A resumed discovery relists sources
and reuses committed cache entries. A resumed comparison retains its frozen
snapshot, pair cursor and accumulated allowances. Exhausted fixed allowances
require a new job; they cannot be increased through `resume`.

## Preview limits and platform qualification

- The text representation reads UTF-8 source text or extracted document text, with one leading BOM removed,
  CR/CRLF normalized, NFC normalization, and preserved case/punctuation.
  It hashes character n-grams into one normalized 4,096-dimensional vector.
  NUL, invalid UTF-8, excessive combining sequences and empty content produce
  explicit per-file outcomes. Image/audio/video encoders also produce one
  normalized vector each. No profile has passed the full accuracy qualification.
- Matching is exhaustive using the portable reference cosine scorer. Pair
  count grows quadratically; it has no BLAS acceleration or reuse of old pair
  scores. This preview has **not** demonstrated 10 TB performance.
- The memory setting governs admission and bounded internal buffers. Linux
  native workers also have an address-space limit; macOS has no hard RSS limit.
  Result allowance counts serialized inventory/pair work and
  group reservations; it is **not** a limit on SQLite, WAL, exports, publication
  copies, or lifetime disk use. Global quotas, pruning, backup/restore and cache
  tombstone reconciliation are later work. No absent cached location is used in
  a new comparison: each snapshot includes only that job's observed inventory.
  Forced host death may leave a private staged copy; decoder descendants are
  terminated, but crash-staging garbage collection is part of pending maintenance.
- Fast metadata freshness is heuristic. Sources that change during a read are
  reported stale; automatic retry and filesystem snapshot adapters are deferred.
  Strict consistency cannot currently make a file ready. Partial coverage is
  explicit. FileTwin never moves or deletes originals.
- The planned release targets are Apple Silicon macOS 14+ and Ubuntu 24.04/26.04
  on x86-64/ARM64. This implementation was exercised locally on **macOS 26.6.1
  ARM64**. The [CI matrix](.github/workflows/ci.yml) uses native hosted runners;
  those remote runs and minimum-version qualification remain pending. Ubuntu
  26.04 runner availability is currently a public preview. See GitHub's
  [runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -p filetwin-core --features bundled-sqlite --example schemas -- schemas --check
cargo build --locked --release -p filetwin-cli
python3 scripts/smoke.py target/release/filetwin
# With native assets and FFmpeg 9 available:
cargo build --locked --release --workspace --bins --examples
python3 scripts/native-smoke.py target/release/filetwin --model-dir .filetwin/models
# Every advertised raster codec and a broader document/audio/video matrix:
python3 scripts/setup-format-fixtures.py --directory target/format-fixtures
python3 scripts/format-smoke.py target/release/filetwin --model-dir .filetwin/models \
  --heif-fixtures target/format-fixtures
```

Regenerate schemas by omitting `--check` from the schema command. See the
[validation record](implementation-plan.md#validation-record) for completed
checks and remaining release gates.
