# FileTwin implementation plan

Date: 2026-09-09
Status: milestone 1, all-family encoding, and saved-score/matrix implementation complete; qualification/maintenance remain planned

## Caller progress counters

- [x] Expose processed inventory counts and nullable totals through JSONL progress, status, summaries and the Rust API; leave percentage calculation to callers.
- [x] Count all examined comparison candidates separately from actual vector comparisons, including scope/profile skips and hash-only checks; expose saved-score reuse totals for grouping.
- [x] Preserve cancellation/resume counters, recover candidate progress from older cursors, and keep legacy published results readable.
- [x] Document counter meanings, unknown/zero totals, streaming CLI input/output and Rust event handling in the README and architecture; regenerate the summary schema.
- [x] Validate 50 workspace tests, including mixed file outcomes, cache hits, hard links, live native progress, skipped comparisons, exhausted budgets, and old/new resume checkpoints.
- [x] Pass formatting, Clippy, release build and 11 schema checks; verify live JSONL and status for 240 generated files/28,680 candidates, saved-score reuse totals, and the runnable Rust host example.

## Saved scores, matrices, and later grouping

- [x] Add explicit full-score retention with optional grouping and no mandatory cutoff in score-only mode; preserve the existing threshold workflow.
- [x] Persist unique compatible pair scores independently of threshold decisions; publish score pages and JSON/JSONL/CSV artifacts.
- [x] Add bounded matrix pages with stable axes, immutable revision selection, real zero/negative scores, null reasons, and coverage.
- [x] Filter score queries by a cursor-bound minimum score; group an exhaustive saved revision without reading vectors or originals.
- [x] Preserve conservative all-pairs groups, separate SHA-256 evidence, original pair scope, source failures, result allowances, and cancellation/resume checkpoints.
- [x] Upgrade schema v1 transactionally to v2 for grouping cursors; preserve old IDs, vectors, snapshots and results, and allow read-only v1 queries.
- [x] Document CLI parameters, JSON requests and responses, matrix paging, storage limits, and runnable Rust integration in the README; generate the additional matrix schemas.
- [x] Finish regression checks and release CLI verification: 49 tests, Clippy, formatting, 11 generated schemas, schema-validated CLI workflows, legacy exports, and the runnable Rust score/matrix example passed locally.

## Current implementation: all four encoding families

- [x] Generalize profile selection, vector validation, cache bindings, snapshot comparison and grouping to independent text/image/audio/video profiles.
- [x] Add a bounded native worker protocol, cancellation/deadlines, explicit runtime paths and verified model provisioning.
- [x] Convert the selected SSCD model to ONNX and verify Rust inference against its TorchScript reference; implement oriented raster decoding.
- [x] Add bounded DOCX and isolated PDFium extraction into the same document-capable text representation.
- [x] Implement an experimental recording descriptor from decoded audio and test encoding changes and hard negatives.
- [x] Implement whole-timeline, up-to-32-frame video sampling with actual timestamps and pooled SSCD descriptors.
- [x] Exercise mixed scans, persisted comparisons/cache, corrupt inputs, resource limits and native failures; update usage, architecture and capability reporting.

This work advances the encoding parts of milestones 3 and 4. Profile calibration,
OCR, remote providers, maintenance and scale qualification remain separate work.

## Milestone 1: executable local-text developer preview

Implement the first local-text slice selected in the architecture. Deliver a
native `filetwin` executable and a reusable `filetwin-core` crate. This milestone
is complete only when the documented commands run, the integration tests pass,
and the differences from the target architecture are explicit.

- [x] Create the Rust 2024 workspace, pin Rust 1.98.1, and lock dependencies.
- [x] Define shared typed requests, errors, summaries, events, and query/export
  contracts; reject unknown fields, duplicate JSON keys, and invalid combinations.
- [x] Freeze a reproducible experimental 4,096-dimensional text profile with
  streaming UTF-8/BOM/newline/NFC handling and bounded normalization state.
  Require explicit experimental profile selection and thresholds when grouping;
  do not claim a calibrated production default.
- [x] Implement local regular-file discovery, no-follow access, hard-link and
  overlapping-root handling, filters, per-file outcomes, and stable locators.
- [x] Implement SQLite WAL/FULL storage, exclusive processing ownership, cached
  vectors, durable jobs, immutable snapshots/results, and read-only queries.
- [x] Implement exhaustive bounded-memory reference cosine comparison and
  deterministic groups whose every pair qualifies under the requested scope.
- [x] Implement cancellation, restart/resume, work-record budgets, and observable
  partial coverage. Resume may relist directories and reuse committed vectors;
  it must preserve a comparison's frozen snapshot and accumulated allowances.
- [x] Implement `scan`, `index`, `compare`, `run`, `resume`, `status`, `results`,
  `export`, `profiles list`, and `doctor`, with human/JSON/JSONL output.
- [x] Publish schemas, CLI and Rust examples, usage documentation, and CI for
  the selected OS/CPU targets. Describe unsupported capabilities explicitly.
- [x] Test streaming/text edge cases, cache invalidation, ownership, alias/scope
  rules, frozen results, cancellation/resume, limits, CLI/library parity, and
  subprocess protocol failures; run formatting, Clippy, tests, and a release
  build, then exercise the release executable on a sample collection.

The preview uses the portable reference scorer first. Native BLAS acceleration,
parallel extraction tuning, and incremental score reuse across snapshots follow correctness
qualification; no section 9 throughput figures apply to this implementation.
Strict source snapshots, sidecar trust/import/export, cloud providers, and
unsupported readers must return explicit unsupported/capability outcomes rather
than silently weaken the request. Local fast freshness remains heuristic.
The preview's record allowance is distinct from physical managed-storage quotas;
it charges inventory/pair work and grouping reservations. It does not bound
SQLite/WAL, duplicated publications or exports. The memory parameter controls
admission and bounded buffers, not an OS RSS limit. These limitations are exposed
in the README and capabilities output rather than advertised as completed scale
qualification.

## Milestone 2: release qualification and operational maintenance

Build the architecture's 200-original-per-family pilot corpus starting with text;
separate calibration and held-out originals and include hard negatives. Promote
profiles only after measured accuracy and coverage gates. Add model provisioning,
backup/restore/migrations/retention APIs, authenticated sidecars, total storage
quotas, cache tombstone reconciliation, bounded source retries, and
filesystem-specific stable-revision adapters. Qualify signed release
archives on every advertised OS version and actual target hardware.

## Milestone 3: images and document readers (implementation complete; promotion pending)

SSCD `sscd_disc_mixup` is converted, checksum-pinned and tested against the
official model. The isolated worker, ONNX Runtime CPU, raster codecs, PDFium and
bounded DOCX extraction are implemented. Promote each capability independently
after the full corpus/platform gates. OCR remains a separate addition.

## Milestone 4: scale and media experiments

Add Accelerate/OpenBLAS comparison kernels with reference cutoff verification,
bounded parallel extraction, and incremental pair reuse. Measure first scan,
rescan, resume, dense output, memory, and real 10 TB I/O. Evaluate the selected
FFmpeg/video sampling baseline and recording-level spectral descriptor now
implemented in the worker. Both retain experimental status until accuracy gates
pass; smoke fixtures are not a replacement for that evaluation.

## Milestone 5: remote deployment

Implement cloud adapters and provider-specific consistency/credential flows.
Add an optional Linux container image when a server deployment requires it.
Docker remains unnecessary for the native CLI and Rust library.

## Validation record

Additional format audit completed locally on 2026-09-09, macOS ARM64:

| Check | Result |
| --- | --- |
| Workspace tests | 40 passed, including primary-image/grid selection, compatible-brand detection and immutable image-v1 compatibility |
| Formatting, Clippy, release build and schema freshness | Passed |
| Expanded format matrix | 72 ready files: 12 text/document, 26 image, 20 audio and 14 video |
| Persisted comparison/cache | 672 within-profile pairs compared after moving originals; 72 cache hits and zero source bytes read on rescan |
| HEIF/AVIF equivalence | Primary HEIC, four-tile HEIC, cropped/rotated HEIC and AVIF each score 1.0 against their decoded PNG equivalents |
| Explicit unsupported inputs | Six inputs produce explicit outcomes without vectors; duplicate arbitrary binary files still produce a SHA-256 exact-copy pair |

Image profile v2 adds the bounded FFmpeg 9 HEIF/AVIF reader. Image v1 remains
unchanged and registered for existing snapshots. The checked format matrix and
remaining exclusions are in [format-coverage.md](format-coverage.md). Reproduce
with `scripts/setup-format-fixtures.py` and `scripts/format-smoke.py`; this run's
local artifact is `target/format-audit/format-results.json`. No Linux runtime,
full codec-variant, or accuracy qualification is inferred from this Mac run.

Earlier baseline, completed locally on 2026-09-08, macOS 26.6.1 / ARM64, Rust 1.98.1
(the saved-score validation above records the newer checks):

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --locked --workspace` | 37 tests passed: 3 text, 18 core integration, 5 native-boundary/cache, 8 CLI subprocess and 3 extractor tests |
| `cargo check --locked -p filetwin-core --no-default-features` | Passed with the host's system SQLite linkage |
| Generated-schema freshness check | All 9 baseline schemas match the Rust contracts |
| JSON Schema Draft 2020-12 validation | All 9 baseline schemas valid; actual release CLI envelopes, summaries, pages, accepted requests and profile validate |
| `cargo build --locked --release --workspace --bins --examples` | Passed; `target/release/filetwin` and `target/release/filetwin-worker` |
| Release smoke workflow | 3 ready files, 1 similar pair, 1 group; rescan has 3 cache hits / 0 bytes read; compare after deleting disposable originals has the same match / 0 bytes read |
| JSON/JSONL/CSV exports | All 7 artifacts per run exported; record counts and SHA-256 checksums verified |
| Native format smoke | 20 ready files across four families, 59 within-profile pairs at cutoff -1, 4 groups; 20 cache hits / 0 source bytes read on rescan |
| Native boundary | Malformed worker replies isolated; cancellation kills decoder descendants; real worker cleans descendants after host SIGKILL; staging limit enforced |
| Native media coverage | Audio-only MP4 classified as audio; 31/32 distinct video samples reach 2.9167 s in 3-second fixtures; single-frame fallback tested |
| SSCD conversion parity | Two local PNG inputs; maximum component errors 2.31e-7 and 2.67e-7 against official TorchScript; reference cosine > 0.999999999998 |

Native fixture scores (uncalibrated, not accuracy estimates): TXT/DOCX 1.0;
TXT/PDF 0.992006; PNG/JPEG 0.893024; EXIF-oriented/equivalent rotated image 1.0;
WAV/MP3 1.0; WAV/AAC 0.995965; WAV/appended silence 0.987179; distinct synthetic
recordings -0.018383; unrelated noise recordings 0.001744; MP4/resized AVI
0.843488; identical footage with shifted stream timestamps 1.0. These examples demonstrate why a
universal 0.95 cutoff cannot be advertised as reliable copy detection.

Reproducible native checks are in `scripts/native-smoke.py` and
`scripts/verify-sscd.py`; local results are under `target/native-assets/`.

The smoke workflow is reproducible with
`python3 scripts/smoke.py target/release/filetwin`. The local run also used the
optional `--schemas` check with Python `jsonschema`, installed only in the ignored
`target/schema-validation` environment. Its captured responses are in
`target/validation/smoke.json` (a local validation artifact, not fixture IDs to
reuse against another index).

Integration coverage includes changed-content invalidation; hard links and
overlapping source scopes; clique grouping and retained cross-group pairs;
immutable snapshots/cursors and result revisions; empty/invalid-text digest
evidence; explicit strict-mode failures; budget exhaustion; cancellation,
SIGINT/SIGTERM, killed-owner recovery and frozen comparison resume; closed/stalled
pipes; and host-callback failure. POSIX-byte paths round-trip on this host; its
filesystem rejects an invalid UTF-8 filename, so actual discovery of such names
still needs the Linux CI run.

CI configuration is not evidence that Linux, the macOS 14 minimum, every named
OS version, or the full filesystem/scale release matrix has passed. Those gates,
accuracy calibration and production packaging remain outstanding.
