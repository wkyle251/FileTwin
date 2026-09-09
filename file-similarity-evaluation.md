# Existing-project evaluation and architecture references

Historical research note for the pre-0.2 design. The current
[architecture](file-similarity-architecture.md) supersedes its persistence,
matching and cloud proposals: FileTwin now returns vectors directly, optionally
reuses one JSON file, hashes every current source, and delegates similarity
decisions to its caller. Third-party descriptions below record the original
review; they have not been re-evaluated as part of this interface change.

Reviewed: 2026-09-08.
Scope: the independent Rust application described in
[the architecture](file-similarity-architecture.md).

This is a source/documentation evaluation plus small local reasoning probes.
No third-party application was installed or benchmarked, no neural model was
run, and no 10 TB or cloud-account scan was performed. Source links below refer
to the branches reviewed; they are not pinned dependency versions. Before a
runtime comparison, record a release/commit, build options, model artifact
checksums, and platform. Development-branch features may differ from releases.

## 1. Decisions from the review

Keep the independent Rust core and one active content vector per file. Use
existing projects as benchmarks and selective component candidates. A wholesale
fork would couple our source identities, portable cache, and single-vector
policy to a different application's scanning and matching assumptions.

| Project | Evidence and overlap | Fit for our design | Decision |
| --- | --- | --- | --- |
| Czkawka / Krokiet | Source and core guide: similar images, videos, music by content, cached signatures, reusable Rust core. | Strong media baseline; temporal video signatures differ from our pooled vector. | Benchmark first; evaluate individual Rust components through adapters. |
| RustDupe | README and document source: perceptual image matching, PDF/DOCX/TXT extraction, SimHash, persistent hash cache. | Useful text and exact-copy reference; end-to-end similarity behavior still needs a run. | Reference algorithms; do not select the whole app as our engine. |
| FiftyOne Brain | Official API documentation and similarity interface: existing embeddings, neighbor search, near-duplicate views, interchangeable indexes. | Strong reference for evaluation and result inspection; adds a Python dataset stack. | Optional development tool; keep production runtime in Rust. |
| EmbedAnything | Official repository and video guide: Rust ingestion/inference, multiple models, sampled video-frame embeddings. | Potential inference adapter; model choice and aggregation still belong to our profiles. | Compare a narrow adapter with direct ONNX Runtime before selecting it. |
| Cloud Duplicate Finder | Vendor product documentation: cloud duplicate management and similar-photo scans. | Useful product reference; algorithm, revision guarantees, and full modality coverage are unverified. | Reference the workflow; no backend dependency selected. |

The judgments above are engineering assessments, not measured accuracy or speed
rankings. The following sections supply the evidence for each row.

## 2. Findings and implications

### Czkawka / Krokiet

The [core guide](https://github.com/qarmin/czkawka/blob/master/instructions/Instruction_Core.md)
documents perceptual image hashes, audio content fingerprints, and video
comparisons across temporal windows, including subclip settings. These are
appropriate baselines for resized/recompressed media and short additions.
Their actual recall on moving watermarks and our short spoken examples is unknown.

The [video implementation](https://github.com/qarmin/czkawka/blob/master/czkawka_core/src/tools/similar_videos/core.rs)
loads cached signatures before processing uncached files, saves results, and
calls the video grouping engine. Its sampling parameters participate in the
video cache filename.

The [cache implementation](https://github.com/qarmin/czkawka/blob/master/czkawka_core/src/common/cache.rs)
uses paths, sizes, and modification dates in reuse checks. Adopt the separation
of discovery, cached work, extraction, and comparison. Our design additionally
needs explicit freshness evidence, stable provider identities, and portable
profile definitions.

The [project README](https://github.com/qarmin/czkawka)
describes the reusable Rust core and distinguishes its MIT code/core from the
GPL-3.0-only Krokiet/Cedinia applications. A dependency decision must name the
specific component; choosing Rust does not require adopting its GUI.

### RustDupe

The [README](https://github.com/MasuRii/RustDupe)
documents SQLite hash caching, image perceptual hashes, and similar-document
scans. Its published throughput figures have not been reproduced here.

The [document source](https://github.com/MasuRii/RustDupe/blob/master/src/scanner/document.rs)
extracts PDF/DOCX/plain text, lowercases text, removes ASCII punctuation,
normalizes whitespace, and computes a 64-bit SimHash from word three-grams,
with a short-text fallback. This is a concrete compact content-overlap baseline.
Its Hamming distances are not cosine scores or percentages.

We reproduced only the ASCII normalization behavior in a small local Python
probe; this did not execute RustDupe or its SimHash implementation:

| Inputs | Normalized strings equal? | Lesson for our text profile |
| --- | --- | --- |
| `Hello World! Im human!` / `Hello World! I'm human!` | Yes | Covers the user's ASCII apostrophe example. |
| `Im human` / `I’m human` | No | Curly apostrophes need an explicit Unicode policy. |
| `value 1.0` / `value 10` | Yes | Removing punctuation loses decimal information. |
| `C++` / `C` | Yes | Removing punctuation loses symbol information. |

Equal normalized strings make the subsequent representation unable to distinguish
those inputs. Unequal strings do not establish whether the similarity threshold
would match them. Keep meaningful punctuation in our main features and evaluate
targeted apostrophe tolerance. Whether a numerical edit should still count as
similar belongs in the labeled dataset, not an exact-copy claim.

### FiftyOne Brain

The [near-duplicate API](https://docs.voxel51.com/brain/index.html#near-duplicates)
accepts precomputed embeddings, exposes distances and neighbor relationships,
and permits changing the duplicate threshold without regenerating embeddings.
The [similarity interface](https://github.com/voxel51/fiftyone-brain/blob/main/fiftyone/brain/similarity.py)
separates index configuration, adding/removing embeddings, and restricting views.

Adopt these separations: encoding profile, retrieval configuration, match
threshold, and displayed groups need independent versions. Use an optional
dataset export to inspect missed matches. A 2D visualization is useful for
inspection; original-space scores remain the group-membership authority.
Its documented reference-to-neighbor lists do not establish our stricter rule
that every pair in a displayed group must pass.

### EmbedAnything

The [repository](https://github.com/StarlightSearch/EmbedAnything)
documents Rust inference/ingestion, ONNX and Candle backends, streaming, and S3
import. Some remote integrations also appear in its roadmap; the entire planned
provider set is not established by that list.

The [video guide](https://embed-anything.com/guides/video/)
samples every Nth frame, caps frame count, and returns embeddings with frame
indexes. For our design, requesting a capped frame sequence must not accidentally
cover only the beginning of a long video. Our adapter must enforce timestamp
coverage and aggregate temporary outputs into one vector.

Model-level pooling options do not establish whole-file pooling. Support for
audio input also does not establish same-recording accuracy. Benchmark each
actual model/profile and retain our own aggregation, source revision, and cache
contracts. Evaluate native model loading, cancellation, peak memory, and whether
the required model can run without additional production runtimes.

### Cloud Duplicate Finder

The [vendor site](https://cloudduplicatefinder.com/)
lists Google Drive, OneDrive, Dropbox, Box, and S3, and advertises similar-photo
scans for Google Drive and OneDrive. Treat these as product claims; this review
did not inspect its implementation or validate advertised throughput.

Adopt clear account/scope selection and grouped review as product references.
Do not infer zero content transfer, general audio/video near-copy matching,
PikPak/iCloud support, or revision-safe caching from cloud duplicate support.
Our transfer planner must count actual bytes and requests.

## 3. Component and model choices

| Area | Initial choice or experiment | Alternative/reference | Promotion condition |
| --- | --- | --- | --- |
| Text/documents | Streaming character/word feature hashing into one normalized vector. | RustDupe's compact SimHash; semantic embeddings as a separate experiment. | Small-edit recall, Unicode behavior, symbol retention, and bounded memory. |
| Images | Evaluate SSCD copy-detection descriptors in the intended native runtime. | Czkawka perceptual hashes as the speed/storage baseline. | Watermark/resize/recompression accuracy plus runtime and artifact provenance. |
| Audio | Keep the fixed-size recording descriptor experimental. | Czkawka's temporal acoustic fingerprints as a sequence-aware baseline. | Same-recording positives, short additions, and unrelated same-category negatives pass. |
| Video | Timestamp sampling and pooled frame descriptors; test 16/32/64 frames. | Czkawka temporal windows; TMK for a fixed-size temporal reference. | Required transformations pass with acceptable extraction and transfer cost. |
| Matching | Exact blocked cosine on original normalized vectors. | FiftyOne/Faiss for evaluating candidate retrieval. | Exhaustive reference correctness; approximate mode must meet separate recall gates. |
| Caching | Local SQLite, immutable vector payloads, revision-bound file records. | Czkawka cache separation and RustDupe persistence patterns. | Correct reuse, atomic publication, rename handling, and restart recovery. |

SSCD is specifically designed for image copy detection. Its repository is
archived and declares MIT for the code; record the provenance and terms of
the exact weights and any conversion separately.
[SSCD repository](https://github.com/facebookresearch/sscd-copy-detection)

TMK supplies a fixed-length temporal signature with its own scoring and tradeoffs,
including weak clip matching. It is a research comparison, not a drop-in
512-dimensional cosine replacement.
[TMK documentation](https://github.com/facebook/ThreatExchange/tree/main/tmk)

Our local toy calculation also confirmed that mean-pooling two idealized scene
vectors in reversed order gives cosine 1 (within floating-point rounding).
This is a mathematical limitation, not a measured video-model result.

Record component licenses separately from model licenses when selecting
dependencies. RustDupe declares MIT; EmbedAnything and FiftyOne Brain declare
Apache-2.0. No code from these projects has been copied into the application.
[RustDupe](https://github.com/MasuRii/RustDupe#license),
[EmbedAnything](https://github.com/StarlightSearch/EmbedAnything),
[FiftyOne Brain](https://github.com/voxel51/fiftyone-brain)

## 4. Changes carried into the architecture

| Finding | Architecture response |
| --- | --- |
| Byte identity and perceptual similarity are different evidence. | Reuse verified byte-identical content when available; never apply same-size filtering to the similarity route. |
| A compact fingerprint alone does not establish recording-level accuracy. | Profiles stay experimental until transformation-specific evaluation passes. |
| Library defaults may sample frames or pool tokens differently. | The application owns deterministic preprocessing, timestamps, aggregation, and the profile manifest. |
| Path/stat caches trade certainty for speed. | Store freshness evidence and support digest verification without forcing full reads on every fast scan. |
| Changing search settings should not invalidate expensive encodings. | Version representation, retrieval, decision threshold, and grouping independently. |
| Millions of matches can dominate vector memory. | Budget result edges, persist resumable block work, and compress verified exact-copy classes. |
| Cloud support is a set of concrete capabilities. | Test revision consistency, range access, placeholders, and actual transfer consumption per adapter. |

The runtime benchmark protocol and rollout requirements are specified in
[architecture section 11](file-similarity-architecture.md#11-validation-and-implementation-sequence).
The source review is complete; application accuracy, deployment compatibility,
and 10 TB performance remain unmeasured.
