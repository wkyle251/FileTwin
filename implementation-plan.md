# FileTwin 0.2 implementation plan

Goal: encode a directory, return every file's content ID and available vector,
and optionally reuse/save one portable vector file. The caller owns similarity
filtering and grouping. This replaces the previous database architecture.

## Implementation

- [x] Replace job/catalog APIs with `Encoder`, `EncodeRequest`, callback progress,
  shared cancellation and a directly returned `VectorFile`.
- [x] Remove SQLite and catalog, snapshots, matching, matrices, paging, exports,
  thresholds, experimental opt-ins and TOML request configuration.
- [x] Reduce CLI to a directory, optional previous vector JSON, and output/model/
  backend/worker options. Emit one stdout result and JSONL progress on stderr.
- [x] Use complete-original SHA-256 IDs, independent of file path. Preserve one
  record per path and deduplicate encoding of identical content within a call.
- [x] Validate reusable vectors by content/profile, dimension, normalization and
  checksum. Fully rehash originals before reuse; preserve encoding profile IDs.
- [x] Implement bounded, validated vector-file reads and atomic explicit saves;
  exclude vector/model/staging artifact paths and refuse unrelated output targets.
- [x] Preserve all four native/text families, HEIC/AVIF SDR coverage, pinned
  artifacts, reusable workers, CPU/CoreML/CUDA selection, and source validation.
- [x] Return hashes and explicit null-vector errors for unsupported readable
  content. Return reusable partial results on cancellation and clean workers/temp.
- [x] Migrate Rust examples, JSON schemas, CLI/native/format smoke tools, README,
  architecture and native-format documentation to the new contract.

## Validation

- [x] Unit/integration tests for portable roundtrips, rename/copy identity,
  changed bytes despite restored mtime, in-place cache updates, invalid caches,
  source protection, progress, non-UTF-8 paths and cancellation.
- [x] Worker contract tests for actual concurrency, session reuse, replacement
  after crashes, invalid/oversized responses, source changes, staging admission,
  backend cache separation and decoder cleanup.
- [x] CLI subprocess tests and text smoke, including generated-schema validation.
- [x] Final formatting, Clippy, complete workspace tests, release binaries/examples
  and generated-schema freshness checks.
- [x] Native CPU/CoreML integration and all advertised format fixtures.
- [x] Real collection fresh encode and saved-vector reuse, with source-integrity
  checks and all detailed artifacts retained only under ignored `target/`.
- [x] Update the seven-target macOS/Linux CI workflow to check the new CLI and
  schemas. Per-commit platform results are recorded in GitHub Actions.

Local verification on 2026-09-09, macOS ARM64:

| Check | Result |
| --- | --- |
| Workspace tests | 34 passed, including CLI SIGINT/SIGTERM partial saves |
| Formatting / Clippy / release binaries and examples | Passed |
| Four generated schemas and CLI text smoke | Passed |
| Native integrations | CPU and CoreML passed; workers reused and cleaned up |
| Format fixtures | 72 ready and reused; six explicit unsupported/invalid outcomes |
| HEIC/AVIF equivalence | Four decoded-image comparisons scored 1.0 |
| Real collection, fresh CoreML | 355 files; 354 ready; 79.144 s wall time including JSON saving/output |
| Real collection, saved vectors | 354 reused, zero encodings; 2.596 s wall time, including rehashing all sources |
| Original integrity / prior encoding agreement | Contents and mtimes unchanged; all 354 vectors agreed with prior profiles, and all 173 video frame lists matched |

The one metadata file in the real collection retained its SHA-256 ID and returned
an invalid-text error with a null vector. The portable result was about 3.8 MB.
Detailed source paths, vectors and reports remain in ignored local `target/`.
Timings are single runs on this collection, not general throughput guarantees.

## Preserved optimization evidence

Before the interface rewrite, the reusable worker/GPU implementation was tested
on a local 24-file image/video sample. Fresh encoding with hashing took 68.219 s
before optimization, 11.871 s with optimized CPU, and 6.271 s with CoreML. These
are historical single-run measurements, not a benchmark of the 0.2 interface.
Profile IDs and inference policies are preserved; current correctness checks
must still verify the rewritten pipeline.

## Deferred qualification

- Physical NVIDIA execution, numerical agreement and performance. The Linux
  x86-64 CUDA 12 bundle and provider pins are provisioned/verified, but this Mac
  cannot establish NVIDIA hardware results.
- Broader real-collection precision/recall and per-profile score calibration.
- HDR tone mapping, OCR and other document readers; uncommon codec variants.
- Native bundle signing, minimum-OS/runtime qualification and distribution.
- Streaming vector-file formats if collections exceed the present memory/JSON
  bounds. No database or background service is required by the current design.
