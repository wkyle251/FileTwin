# FileTwin

FileTwin finds similar local text, documents, images, audio and video. It provides a native `filetwin`
CLI, JSON/JSONL output for other applications, and the `filetwin-core` Rust
library. It indexes content, saves immutable snapshots, and can retain every
compatible pairwise score for matrix output and later filtering/grouping.

**License: PolyForm Noncommercial 1.0.0.** Personal, educational and other
permitted noncommercial uses are allowed. Commercial use outside the license's
explicit permissions requires a separate written license. See [License](#license).

**Status: all four encoding families implemented, developer preview 0.1.0-dev.**
Profiles remain experimental. `--all-scores` needs no cutoff; grouping requires
explicit thresholds, which are not accuracy percentages. Unsupported formats
and failed extraction produce explicit outcomes.
OCR, cloud sources, calibration and production-scale qualification remain in the
[implementation plan](implementation-plan.md).
The [architecture](file-similarity-architecture.md) describes the broader target.
See [Supported file formats](#supported-file-formats) for the current format list,
runtime requirements and exclusions.
For caller-controlled cutoffs, start with [scores and matrices](#calculate-scores-then-filter-or-group).

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
  --experimental-text --all-scores --format json
```

For all four encoding families, build the companion worker and explicitly
provision the selected SSCD model, ONNX Runtime and PDFium. Python 3 runs the
setup script; PyTorch is needed only to convert the model during setup. The CLI,
core library and companion worker are written in Rust. Production processing
uses the native libraries and FFmpeg without invoking Python or downloading assets.

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
  --experimental --all-scores --format jsonl
```

The native assets are installed under `.filetwin/models` by this command.
`setup-native.py --onnx PATH` can install a previously converted,
verified artifact without PyTorch; the setup script still requires Python 3.
The Python smoke, format and parity scripts are developer test tools. Running
FileTwin with provisioned assets requires no Python installation. Copy both
executables and the provisioned model directory when deploying; see
[native-encoding.md](native-encoding.md).

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

## Calculate scores, then filter or group

For faster encoding and optional Apple/NVIDIA acceleration, see
[Encoding performance and GPU support](#encoding-performance-and-gpu-support).

Use this workflow when a CLI caller or another application chooses the cutoff
after seeing the scores. All paths below are local; scan data and reports should
remain outside the source repository or in a Git-ignored directory.

```sh
# 1. Encode each file and save every compatible pairwise score; no cutoff needed.
./target/release/filetwin --data-dir .filetwin scan /absolute/path/to/files \
  --experimental --all-scores --exact-duplicates compute --format json

# 2. Use the run_id returned in the summary to read a matrix.
./target/release/filetwin --data-dir .filetwin matrix --run SCORE_RUN_ID \
  --format json

# 3. Filter saved pairs at any cutoff. This reads neither files nor vectors.
./target/release/filetwin --data-dir .filetwin results --run SCORE_RUN_ID \
  --kind scores --min-score 0.80 --page-size 100 --format json

# 4. Optionally build conservative groups from the same saved scores.
./target/release/filetwin --data-dir .filetwin group --run SCORE_RUN_ID \
  --threshold 0.95 --format json
./target/release/filetwin --data-dir .filetwin group --run SCORE_RUN_ID \
  --threshold 0.80 --format json
```

`SCORE_RUN_ID` is an actual `run_id`, not a snapshot ID. The first command returns
a summary, not the matrix itself. Each `group` command returns a **new** group
run ID; use `results --run GROUP_RUN_ID --kind groups`, then `--kind members
--group GROUP_ID` to read it. The saved score run remains unchanged.

To score an existing snapshot, use
`compare --snapshot SNAPSHOT_ID --all-scores`. This calculates scores from saved
vectors without re-encoding or reading originals. After that, `results`, `matrix`
and `group` reuse those scores. `compare` itself starts a fresh comparison; it
does not reuse scores from earlier runs or other snapshots.

The existing `scan --experimental --threshold 0.95` workflow still immediately
filters and groups, retaining only qualifying pairs. Adding `--all-scores
--threshold 0.95` both retains all scores and builds those initial groups.
`--all-scores` without a threshold clears configured cutoff defaults and skips
all grouping, including exact-duplicate grouping. SHA-256 evidence is still
retained when requested and is available to a later `group` command.

### Inputs and parameters

| Input | CLI | Meaning |
| --- | --- | --- |
| Files/directories | `scan PATH… --experimental --all-scores` | Recursive input; all four current families selected. Native encoders need the setup above. |
| Saved vectors | `compare --snapshot ID --all-scores` | Produce a new score run from an immutable snapshot. |
| Matrix source | `matrix --run ID [--revision N]` | Read all retained scores as a rectangular matrix page. |
| Matrix window | `--row-offset N --column-offset N --row-limit N --column-limit N` | Zero-based offsets; each limit defaults to 128 and must be 1–256. |
| Filtered score page | `results --run ID --kind scores [--min-score S]` | Inclusive cosine cutoff in `[-1, 1]`; omit it to read every retained compatible pair. |
| Score-page continuation | `--cursor TOKEN --page-size N [--revision N]` | Existing paging rules apply; keep the same `--min-score` when reusing a cursor. |
| Group input | `group --run ID [--revision N] --threshold [FAMILY\|PROFILE_ID=]S` | Requires a completed, exhaustive score revision; the selected revision and original pair scope are frozen in the new job. |
| Storage/time allowance | `--result-bytes N --max-runtime-seconds N` | Supported by scoring and grouping; an exhausted allowance produces explicit partial coverage. |

For matrix windows, `rows` and `columns` follow the same immutable file-ID order.
Keep the returned `result_revision` on subsequent requests. Both
`next_row_offset` and `next_column_offset` are independent: fetch every column
window for a row window, reset the column offset, then advance the row offset.
The matrix page does not claim to include every file when either continuation offset is
non-null. Each response has a 4 MiB ceiling; request a smaller window if needed.

### Matrix and score outputs

With `--format json`, stdout contains one versioned envelope. A matrix response
has `type: "matrix"`. This **abridged illustrative response** shows the layout;
full fields are defined in [matrix-page.schema.json](schemas/matrix-page.schema.json):

```json
{
  "schema_version": 1,
  "type": "matrix",
  "data": {
    "snapshot_id": "snapshot_example",
    "run_id": "run_example",
    "result_revision": 1,
    "metric": "cosine",
    "score_range": [-1.0, 1.0],
    "total_files": 2,
    "row_offset": 0,
    "column_offset": 0,
    "rows": [{"file_id": "file_a"}, {"file_id": "file_b"}],
    "columns": [{"file_id": "file_a"}, {"file_id": "file_b"}],
    "scores": [[1.0, 0.97], [0.97, 1.0]],
    "unavailable_reasons": [[null, null], [null, null]],
    "next_row_offset": null,
    "next_column_offset": null
  }
}
```

Each full row/column record also contains `vector_id`, a lossless `locator`,
`family`, `profile_id`, and processing `state`. `scores[i][j]` compares `rows[i]`
with `columns[j]`. Scores retain full precision in `[-1, 1]`; multiply by 100
only for display. Compare original scores against the cutoff, not rounded text.

The matrix is symmetric. Self-similarity is 1 for files with valid vectors;
ignore the diagonal when filtering duplicates. One row represents a file
object; hard-link paths are available through the existing location queries.
Byte-identical evidence remains separate from cosine scores, including for
unsupported formats that have no vector.

| Cell | Meaning |
| --- | --- |
| Number, including `0` or a negative score | A valid saved comparison. |
| `null` / `file_unavailable` | At least one file has no usable vector; inspect its state and error records. Its diagonal is also null. |
| `null` / `incompatible_profile` | Different families or encoding profiles cannot be compared, even when dimensions match. |
| `null` / `outside_scope` | The requested pair scope excluded this pair. |
| `null` / `not_computed` | The published score revision did not reach this pair. Inspect `completeness`; missing values are never treated as zero. |

`results --kind scores` returns an ordinary `type: "page"` envelope. Each item
contains `file_a`, `file_b`, `vector_a`, `vector_b`, `family`, `profile_id`,
`metric`, `scorer`, `score`, `calibration_status`, and
`match_kind: "similarity_score"`. These are measurements, not threshold decisions.
Filtering does not change them. Score pages and exports store each unique
non-self pair once, with `file_a < file_b`; the matrix supplies the mirrored view.
For score pages, read `results --run ID --revision N --kind summary` to check
coverage before interpreting missing pairs; completeness is carried directly
in matrix responses.

The scoring summary reports `counts.scores_retained` and `counts.pairs_compared`.
A later grouping summary reports `counts.scores_reused`; its `pairs_compared`,
`vectors_encoded`, and `bytes_read` are zero. Groups still guarantee that **every
member pair** meets the chosen cutoff. Connected similarity chains are not
automatically one group. Extraction failures remain visible in matrix cells,
error queries, summaries, and group-run coverage.

Full-score retention takes `N * (N - 1) / 2` records per compatible set, so it is
an explicit option. At 10,000 compatible files that is 49,995,000 scores. Matrix
windows and paginated score records bound response memory, not total storage.
The default result allowance is 1 GiB of charged work records; database,
publication, and export overhead add further disk use. Incomplete score runs
remain queryable, but `group` requires exhaustive comparison coverage. An older
threshold-only run must first be replaced by `compare --all-scores` over its
snapshot; discarded scores cannot be recovered by a query.

For a complete machine-readable export, use `export --run SCORE_RUN_ID
--report-format jsonl --report-dir /absolute/path/to/new-report`. Score runs add
`scores.jsonl` (or `.json`/`.csv`) to the normal report artifacts. Matrix output
can be redirected to a file with `matrix ... --format json > matrix.json`.

### JSON requests and Rust callers

[score-scan-request.json](examples/score-scan-request.json) is a complete request
for all four families. Set its absolute source path before running:

```sh
./target/release/filetwin --data-dir .filetwin run \
  --request examples/score-scan-request.json --format json
```

Its `matching` input is:

```json
{"retrieval":"exact","score_retention":"all","grouping":"none"}
```

To group a saved run through JSON, submit `operation: "group"`,
`source_run_id`, optional positive `source_revision`, and
`matching.threshold_overrides` containing one cutoff per selected profile ID.
For example, this request groups an **image-only** score run:

```json
{
  "schema_version": 1,
  "operation": "group",
  "source_run_id": "run_example",
  "source_revision": 1,
  "matching": {
    "threshold_overrides": {
      "sha256:88a7383952aabd98b9bc41e66b355fc569e9070697040026e4476db04408700b": 0.8
    }
  }
}
```

An all-family run needs all four cutoffs, including selected profiles with no
ready files. The CLI's bare `--threshold 0.8` expands them automatically.
`group` rejects discovery/encoding parameters and cannot expand or shrink the
saved pair scope.

Rust callers use `JobRequest::experimental_scores(paths)` (or
`JobRequest::text_scores(paths)`), `Catalog::matrix(MatrixQuery::new(run_id))`,
`Catalog::results(ResultsQuery { kind: "scores", min_score: Some(0.8), ... })`,
and `JobRequest::group_scores(run_id, thresholds_by_profile_id)`.
See the runnable [scores library example](crates/filetwin-core/examples/scores.rs),
[matrix query schema](schemas/matrix-query.schema.json), and
[processing request schema](schemas/job-request.schema.json).

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
members, pairs, errors, a summary, and a checksummed manifest. Runs that retain
all scores also export a `scores` artifact. JSON, JSONL, and
CSV exports stream from persisted records. CSV includes `record_json` to retain
the complete record, including nested and lossless path fields.

## Commands and important parameters

| Command | Input | Output |
| --- | --- | --- |
| `scan PATH…` | Files/directories, explicit profile, `--all-scores` and/or threshold | Job summary, immutable snapshot and comparison run |
| `index PATH…` | Files/directories and explicit profile | Job summary and snapshot; no matching |
| `compare --snapshot ID` | Saved snapshot, `--all-scores` and/or threshold | A new run without reading originals |
| `matrix --run ID` | Score run/revision and row/column window | Matrix page with file axes, scores, null reasons, and coverage |
| `group --run ID` | Exhaustive score run and new thresholds | New groups/pairs run; no vector comparison |
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
| `--all-scores` | Retain all compatible scores; without `--threshold`, skip grouping |
| `--threshold [FAMILY\|PROFILE_ID=]SCORE` | Required for every selected profile when filtering/grouping during a job; finite cosine cutoff in `[-1, 1]` |
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

Run each command with `--help` for all parameters. Native encoders use a bounded
pool of reusable processes. `--inference-workers` defaults to 2 and controls
concurrent native files; the effective count also depends on the memory allowance.
Discovery, text hashing and source staging run on the coordinator;
`--io-workers` remains an upper bound with one staging reader. Comparison uses
one reference scorer. Each original is checked again before saving its vector.
Staging is removed after success, error or cancellation. Native workers have a
300-second per-file deadline, also bounded by the job's remaining time. No
network content is fetched. Image/video admission requires 512 MiB; native
document/audio admission requires 128 MiB. Large decoded frames can require more.
Sidecar enable flags return `unsupported_capability`. `index` rejects matching
parameters. `compare` and `group` accept only memory, result, and time limits. `compare` uses the
snapshot's original profile, scope memberships, digests, and extraction outcomes.

`--kind` is one of `summary`, `groups`, `members`, `pairs`, `scores`, `files`, `locations`,
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
  --data-dir .filetwin --format jsonl --non-interactive --progress-interval-ms 250
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

For live progress, read stdout one line at a time and handle `type: "progress"`.
`--progress-interval-ms` throttles updates (default 1,000 ms); it does not set a
guaranteed delivery frequency. Short jobs or stages may finish without a progress
event. This applies to processing commands such as `scan`, `index`, `compare`,
`group` and `resume`. Queries such as `matrix` and `results`, and report exports,
return one response without a progress stream.

An illustrative progress event, with envelope fields and some counters omitted:

```json
{
  "type": "progress",
  "data": {
    "stage": "comparing",
    "counts": {
      "files_processed": 120,
      "files_total": 120,
      "files_ready": 120,
      "vectors_encoded": 120,
      "pairs_processed": 2000,
      "pairs_total": 7140,
      "pairs_compared": 2000,
      "scores_retained": 2000,
      "scores_reused": 0,
      "scores_total": null
    },
    "elapsed_seconds": 2.5,
    "eta_seconds": null
  }
}
```

These raw counters are available in progress events, `status` and terminal
summaries, and as fields of the Rust `Counts` type:

| Counter | Meaning |
| --- | --- |
| `files_processed` | Inventory entries with a recorded ready, failed or excluded outcome; includes cache hits. |
| `files_total` | Final recorded inventory size; `null` until discovery finishes or a saved snapshot is loaded. |
| `files_ready`, `files_failed`, `files_excluded` | Outcome breakdown. Failures include unsupported formats and insufficient content. |
| `vectors_encoded`, `cache_hits` | Encoding work completed and cached vectors reused. |
| `pairs_processed`, `pairs_total` | Candidate pairs examined and the total candidate work for comparison. |
| `pairs_compared` | Actual compatible vector comparisons; can be less than `pairs_processed`. |
| `scores_retained` | Similarity scores saved by an all-scores run. |
| `scores_reused`, `scores_total` | Saved similarity scores inspected and the total to inspect during a `group` job. Exact-duplicate records are separate. |
| `bytes_read`, `bytes_hashed` | Source bytes read and hashed; comparison/grouping of saved data does not read originals. |

The current `discovery` stage includes encoding. It visits and processes files
incrementally, so `files_discovered` is not a final total during a scan.
Inventory counts share the snapshot's entry semantics: hard-linked file aliases
share one entry, while excluded paths can include directories and symlinks.
Errors that prevent recording an entry are reported separately; a known inventory
size does not imply complete source coverage. On discovery resume, inventory
counts can reset as directories are relisted; `attempt_id` identifies the attempt.

For `comparing`, callers can calculate `pairs_processed / pairs_total` when the
total is positive. Candidates are all unique, non-self pairs of snapshot entries
with a vector or known SHA-256. Processing includes scope/profile skips and
hash-only checks, so use `pairs_processed` as the numerator. `pairs_total` is
`null` before comparison is prepared, or for an operation that does no comparison;
zero means there are no candidates. Saved-score `filtering` provides
`scores_reused / scores_total`, followed by a separate `grouping` stage.

FileTwin leaves percentages to the calling application. Counters can remain
unchanged while one file or a grouping step is being processed; there is no
per-file encoding percentage or ETA (`eta_seconds` is null). Use stage labels
and counters to display activity, and the terminal summary to determine completion.
Older published results can omit these newly added counters; their immutable
payloads are preserved. Resumed comparisons recover candidate progress from
their stored cursor when the old checkpoint has no `pairs_processed` field.

A caller can also poll durable state using the `job_id` from `accepted`:

```sh
./target/release/filetwin --data-dir .filetwin status --job JOB_ID --format json
```

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
use filetwin_core::{Engine, HostServices, api::{EngineConfig, JobEvent, JobRequest}};

let engine = Engine::open(EngineConfig::new(data_dir), HostServices::default())?;
let handle = engine.submit(JobRequest::text_scan([source_dir], 0.7))?;
for event in handle.events() {
    if let JobEvent::Progress { data, .. } = event {
        // The host can forward these counts to its UI or another API.
        eprintln!("progress: {data}");
    }
}
let summary = handle.wait()?;
engine.shutdown()?;
```

Supply absolute `PathBuf` values. `JobRequest::from_json` applies strict JSON
presence/duplicate validation when embedding a serialized caller. `JobHandle`
exposes bounded events, cancellation and an authoritative `wait()` result.
For live updates, consume `handle.events()` while the job runs and handle
`JobEvent::Progress { data, .. }`; its `data` has the same progress fields shown
above. Events may be dropped when the bounded queue is full, so use `wait()`
for the final outcome. Cloned event receivers compete for messages.
`Catalog::open_read_only` permits
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
Unknown/nonempty databases and newer schema versions are refused. Opening an
existing FileTwin schema-v1 index with the engine upgrades it transactionally to
schema v2, adding a grouping checkpoint cursor while preserving snapshots,
vectors and results. Read-only queries can still inspect v1 indexes. Older
FileTwin binaries cannot open an upgraded v2 index.

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
  count grows quadratically; there is no BLAS acceleration or incremental score
  reuse across snapshots. `group` and score queries reuse an existing score run.
  This preview has **not** demonstrated 10 TB performance.
- The memory setting governs admission and bounded internal buffers. Linux
  CPU workers also have an address-space limit; CUDA workers use a bounded
  inference arena because GPU drivers require large virtual address ranges.
  This is not a total GPU memory ceiling; macOS has no hard RSS limit.
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
  ARM64**. The [CI matrix](.github/workflows/ci.yml) checks Rust builds, tests and
  the CLI on native hosted runners; full native-runtime/GPU and minimum-version
  qualification remain pending. Ubuntu
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

## Encoding performance and GPU support

`--experimental` now selects optimized CPU image/video profiles by default.
FileTwin reuses the model within each native worker and processes native files
concurrently. Optimized video profiles decode clips up to 60 seconds once,
selecting the same 32 midpoint targets rounded to the stream time base. Longer
clips use sparse seeks. Sampling is not reduced to obtain the speedup.

On an Apple M5 Pro, a 24-file sample (12 images and 12 videos) took **68.2 s
before this optimization, 11.9 s with optimized CPU, and 6.3 s with CoreML**.
These are fresh-index encoding/hash times from one local run, approximately
5.7× and 10.9× faster; filesystem/system caches were not cleared. NVIDIA timing
has not been measured. See the [benchmark and validation record](native-encoding.md#optimization-validation)
for settings and vector-agreement checks.

| Backend | CLI selection with `--experimental` | Deployment requirements |
| --- | --- | --- |
| Optimized CPU (default) | `--backend cpu` | Existing pinned native assets; macOS ARM64 and Linux x86-64/ARM64. |
| Apple GPU / Neural Engine | `--backend coreml` | Apple Silicon macOS and the pinned CoreML-enabled ONNX Runtime. CoreML chooses hardware for supported operations. |
| NVIDIA GPU | `--backend cuda` | Linux x86-64, the pinned CUDA 12 ONNX Runtime bundle, compatible NVIDIA driver, CUDA 12 runtime libraries and cuDNN 9. |
| Original CPU reference | `--backend reference` | Original image-v2/video-v1 profiles, one inference thread and graph optimizations disabled. Worker/model reuse still applies. |

```sh
# Portable optimized CPU, with two concurrent native files and two threads each.
./target/release/filetwin --data-dir .filetwin scan /absolute/path/to/files \
  --experimental --backend cpu --inference-workers 2 --inference-threads 2 \
  --all-scores --format jsonl

# Apple Silicon: use the same model directory and explicitly select CoreML.
./target/release/filetwin --data-dir .filetwin scan /absolute/path/to/files \
  --experimental --backend coreml --all-scores --format jsonl

# Linux x86-64 with NVIDIA: explicitly install the CUDA 12 runtime variant.
# --onnx points to the existing checksum-verified model; alternatively use
# --conversion-python as in the initial setup instructions.
python3 scripts/setup-native.py --model-dir .filetwin/models \
  --onnx .filetwin/models/sscd_disc_mixup.onnx --onnxruntime-variant cuda12
./target/release/filetwin --data-dir .filetwin scan /absolute/path/to/files \
  --experimental --backend cuda --cuda-device-id 0 --inference-workers 1 \
  --all-scores --format jsonl
```

Setup installs and verifies ONNX Runtime plus its CUDA/shared provider libraries;
it does not install an NVIDIA driver, CUDA toolkit/runtime or cuDNN. The system
loader must find those dependencies. For nonstandard locations, repeat
`--cuda-library-dir /absolute/path/to/lib`; the library equivalent is
`EngineConfig.runtime.cuda_library_dirs`. The worker does not inherit the host's
`LD_LIBRARY_PATH`. See the [ONNX CUDA requirements](https://onnxruntime.ai/docs/execution-providers/CUDA-ExecutionProvider.html).
`setup-native.py --target linux-x86_64` also supports provisioning a deployment
bundle from another host. Production encoding remains native Rust; Python is a
setup/test tool. Docker is optional.

An explicitly requested unavailable GPU produces `runtime_unavailable`; FileTwin
does not silently substitute a CPU profile. Supported GPU sessions may execute
unsupported model operations on CPU. Select `--backend cpu` for the portable
fallback. GPU support accelerates the image model and video frame embeddings;
text/document features, audio fingerprints, hashing and comparison remain on CPU.

`--inference-workers N` is capped at 64 and at one worker per 512 MiB of the total
memory allowance (at least one worker). The allowance is divided across those
workers; large images may need `--memory-bytes` increased or fewer workers.
`--inference-threads N` accepts 1–64, defaults to 2, and applies per model session;
reference profiles always use one. `--cuda-device-id N` selects a nonnegative
CUDA device index. CUDA's arena receives a per-worker allowance, but neither it
nor macOS provides a hard total CPU/GPU memory ceiling. CUDA workers do not apply
Linux `RLIMIT_AS`, which conflicts with GPU virtual-address reservations.

The public JSON request stores the selected **profile IDs** and existing
`limits.inference_workers`; `--backend` is a CLI shortcut for selecting profiles,
not a JSON request field. Obtain IDs with `profiles list`. Rust callers can use
`JobRequest::experimental_scores_with_backend(paths, profile::Backend::Coreml)`
or `experimental_scan_with_backend(paths, cutoff, backend)`. Set
`EngineConfig.runtime.inference_threads` and `cuda_device_id` as needed.

Accepted events report the effective worker count. Ready image/video file records
include `extraction.inference` with `backend`, `model_reused`, `model_load_seconds`,
`inference_seconds`, thread count and CUDA device, plus `extraction.worker_seconds`.
Video records include `decoder_strategy` and the actual sampled frame timestamps.
Existing file/progress counts and result matrices retain their formats.
Rebuild and deploy both `filetwin` and `filetwin-worker` together: the internal
worker protocol is now version 2; the public JSON schema remains version 1.

Reference, optimized CPU, CoreML and CUDA profiles have different IDs. Old
snapshots remain readable and retain their original meaning; selecting another
backend recomputes affected vectors. Cross-profile matrix cells are incompatible,
even when dimensions match. Accelerated arithmetic can shift scores slightly,
so cutoffs remain caller choices. CoreML compilation is cached under
`DATA_DIR/inference-cache`, keyed by model checksum/runtime version; this cache
is separate from vector reuse and is outside the staging/result allowances.

To reuse unchanged vectors on later scans, keep the same data directory and
profiles and use `--cache-mode reuse` (the default). `--cache-mode refresh`
deliberately encodes again. See [native-encoding.md](native-encoding.md) for the
validation record and remaining platform qualification.

## License

FileTwin's original source code, scripts, examples and documentation are licensed
under the [PolyForm Noncommercial License 1.0.0](LICENSE)
(`PolyForm-Noncommercial-1.0.0`). This applies to the CLI, `filetwin-core` Rust
library, companion worker and their compiled binaries.

| Example use | Permission |
| --- | --- |
| Organizing your own files, personal study, or a hobby project with no anticipated commercial application | Allowed under the license. |
| Classroom teaching, student projects, or noncommercial academic research | Allowed under the license. |
| Modifying FileTwin for a permitted purpose, or sharing copies and permitted modifications | Allowed subject to the license, including its notice requirements. |
| Using FileTwin in a paid or advertising-funded app, a commercial hosted service, paid client work, or a company's internal business operations | Requires a separate written license unless an explicit permitted purpose in the license applies. |

The license also explicitly permits use by charitable organizations, educational
institutions, public research organizations, public safety or health organizations,
environmental protection organizations, and government institutions, regardless
of their funding source or obligations resulting from that funding. The table is
a practical summary; the complete [LICENSE](LICENSE) controls these permissions.

Calling the CLI from another application or embedding the Rust library is still
use of FileTwin under these terms. Making an app free to download does not by
itself make its business use noncommercial. For use outside the permitted
purposes, request a separate written license from the copyright holder through
the [FileTwin repository](https://github.com/wkyle251/FileTwin).

FileTwin is **source-available with a noncommercial license**. It does not meet
the [Open Source Definition](https://opensource.org/osd), which requires allowing
commercial use.

When redistributing, include the license text or its official URL and the
`Required Notice:` copyright line from [LICENSE](LICENSE). Dependencies, model
weights, native runtimes and external test fixtures remain subject to their own
licenses; this license does not replace their terms or grant rights to input
files. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and preserve the relevant
upstream notices when distributing a bundle.
