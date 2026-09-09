# Native encoding implementation

The current implementation supplies encoders for all four content families in
the design. Each file has one normalized float32 vector under one immutable
profile. Caller comparisons require equal family **and** profile ID. A
95% display cutoff means cosine >= 0.95; it is not a confidence estimate.

## Representations and coverage

| Profile name | Representation | Coverage and limitations |
| --- | --- | --- |
| `experimental-text-v1` | 4,096-dimensional signed character n-grams | Original strict UTF-8 reader, retained as an immutable profile definition |
| `experimental-documents-v1` | Same text features, with a new reader-policy manifest | UTF-8; DOCX main body; PDFium page text. These inputs can match each other. DOCX auxiliary parts, layout and OCR are excluded. Empty/scanned PDF pages prevent a complete text vector. |
| `experimental-image-sscd-v1` | SSCD `sscd_disc_mixup`, ResNet50, 512 dimensions | Original raster-only profile retained unchanged for compatibility |
| `experimental-image-sscd-v2` | Same SSCD representation; expanded reader policy | First raster image/frame with EXIF orientation, or FFmpeg 9 primary SDR HEIF/AVIF image including tile-grid reconstruction and crop/rotation. RGB8, alpha on black, triangle resize directly to 320×320, official ImageNet channel normalization |
| `experimental-audio-landmarks-v1` | 4,096-dimensional signed spectral-peak-triplet sketch | First audio stream, mono 16 kHz; up to 32 disjoint eight-second windows centered across the timeline. Files up to 256 seconds cover the complete timeline. FFT 1024/hop 256, periodic Hann; peak triples at offsets (7,3,0), (15,7,0), (31,15,0), (63,31,0); signed square-root counts and L2 normalization. |
| `experimental-video-sscd-v1` | Mean of normalized SSCD frame descriptors, then L2 normalization | 32 midpoint targets across the entire timeline, deduplicated by decoded PTS. The first frame is a fallback if no midpoint yields a frame. Actual integer PTS, time base, seconds and missing targets are retained. Audio and temporal order are excluded. |
| `experimental-image-sscd-{cpu,coreml,cuda}-v1` | Same model and image preprocessing, optimized execution | Separate backend-specific IDs; optimized CPU is the new default. CoreML and CUDA are explicit choices. |
| `experimental-video-sscd-{cpu,coreml,cuda}-v1` | Same model and 32 midpoint targets, optimized execution | Clips up to 60 seconds use one continuous decode with targets rounded to the stream time base; longer clips retain sparse seeks. Backend and decoder policy are included in the profile ID. |

`filetwin_core::profile::profiles()` returns profile IDs and full manifests. The
`experimental-*` names are immutable algorithm labels, not CLI opt-in flags.
Both audio pooling and video averaging lose some temporal information. Short
insertions can shift samples, and short overlaps can be missed. Synthetic smoke
tests establish implementation behavior; they do not establish real-collection
precision/recall or provide default thresholds.

Audio/video sampling uses the selected stream's start time and duration, with
remaining container duration as a fallback. Decoder seeks account for stream
offsets; delayed video streams retain their end-of-timeline coverage. The audio
sketch uses signed hashes to avoid a positive collision baseline, and triples
across time to distinguish recordings with similar spectral distributions.

Image codecs are explicitly enabled in the worker's Cargo manifest. The format
suite exercises PNG/JPEG/WebP/TIFF/BMP/GIF/ICO/PNM/TGA/DDS/QOI/farbfeld/HDR/OpenEXR,
plus AVIF and HEIC conformance fixtures. See [format-coverage.md](format-coverage.md).
SVG rendering, legacy Office, slides and spreadsheets are not implemented.
General ZIPs are not treated as documents unless the DOCX package/body structure
is present.

HEIF/AVIF uses an explicit primary item or primary tile-grid selection, preserving
FFmpeg's crop/rotation and recording decoded dimensions. It does not require a
media duration. Color/depth/alpha items are not combined independently; alpha
is retained when present in FFmpeg's RGBA output. ICC conversion is excluded.
PQ/HLG inputs return unsupported until a tone-mapping profile is qualified.
The reader admits at most 256 items and 32,768 pixels per dimension. The sum of
canvas and selected-item pixels, multiplied by 32 bytes, must fit half the worker
memory allowance. PNG transfer and decode buffers are capped independently.
FFmpeg/ffprobe are required for this path, while other image formats continue
to use the Rust raster decoder. The original v1 image ID remains registered;
the current CPU/CoreML/CUDA image profiles include the expanded reader policy.

Media signatures and extension hints route recognized containers to ffprobe.
The detected streams decide audio versus video, ignoring attached cover art;
an audio-only MP4 is audio even when its extension is `.mp4`. All families are
attempted automatically. Unsupported codecs, malformed media and unknown durations return
explicit failures. Video uses FFmpeg's SDR RGB conversion and autorotation;
PQ/HLG HDR video is rejected until a tone-mapping profile is qualified.

## Native boundary and limits

`filetwin-core` opens original files without following symlinks and hashes all
original bytes, including on cache hits. Native candidates are copied to private
staging during hashing when the source fits admission; copies are discarded on
cache hits. Native reads of that private
copy and model/runtime reads are not counted again as source I/O. Before saving
a result, the coordinator verifies the source's descriptor and pathname still
refer to the observed revision. Reuse uses the verified content ID and profile,
never only mtime, inode or pathname. File IDs are SHA-256 of complete originals.

The companion receives internal protocol-v3 JSON lines through nonblocking pipes
(64 KiB requests and 1 MiB responses/diagnostics). Both executables must be rebuilt
together. Public output uses vector-file schema version 1. The coordinator drains stdout and
stderr while checking cancellation and per-file deadlines; a full pipe cannot block it.
Native processes handle one request at a time and are reused within one invocation,
including a verified model session shared by images and video frames. A failed or
malformed worker is discarded and replaced for subsequent files. A private
process group contains each worker and its FFmpeg descendants. Cancellation,
timeouts and invocation termination kill all groups and remove staging; a worker also
terminates its group if its host disappears. Per-file source revision validation
and result assembly remain on the coordinator. There is no database. SIGKILL or
power loss may leave temporary directories; crash-staging garbage collection and
process reuse across invocations are not implemented.

The sum of active native staging must fit `staging_bytes`, reserving an additional
2 MiB for each pending file. File growth is checked against the remaining allowance.
The default is 10 GiB and can be raised for larger files. Buffers and image
dimensions are capped. Native document/audio admission requires 128 MiB;
image/video requires 512 MiB. Larger frames can require a higher allowance.
The native worker count defaults to two and is capped at 64 and one per 512 MiB
of the invocation memory allowance (at least one). Each worker receives an equal share
of that allowance. Lower the count for unusually large decoded inputs. Linux
CPU workers have an address-space limit; CUDA workers omit it because GPU drivers
reserve large virtual ranges, and configure the CUDA arena allowance instead.
CUDA's arena is not a total GPU/process memory cap. macOS has no reliable hard
RSS limit. These are developer safeguards, not demonstrated 10 TB capacity.

DOCX XML is bounded to 32 MiB, 256 nesting levels, 4,096 ZIP entries and an 8 MiB
central directory. ZIP64/multi-volume packages, DTDs, external entities and
duplicate member names are rejected. PDF text is bounded to 32 MiB UTF-8 and
10,000 pages. No file receives a dummy zero vector when extraction fails.

FFmpeg receives a seekable file descriptor through its `fd:` protocol; source
paths and nested network/file URLs are unavailable to its demuxer. Output uses
`pipe:`. The original pathname is never sent to the decoder. Libraries are
isolated from the embedding Rust application's address space and global state.

## Setup, pinning and deployment

Run the build/setup commands in [README.md](README.md). `setup-native.py` is
explicit provisioning, not a processing dependency. It verifies:

- Official SSCD TorchScript source SHA-256; conversion with pinned PyTorch/ONNX
  versions; canonical ONNX output SHA-256. The converter retains BN operations
  and removes exporter diagnostics to keep the artifact reproducible.
- ONNX Runtime 1.28.2 and PDFium Chromium 8044 archive/library SHA-256 values in
  [runtime-artifacts.json](crates/filetwin-worker/runtime-artifacts.json).
- Native worker model and library checksums again before inference/loading;
  reused sessions continue to use those verified in-memory artifacts.
- CUDA 12 Linux x86-64 runtime and CUDA/shared provider libraries, when explicitly
  selected during setup. CUDA 12, cuDNN 9 and an NVIDIA driver are supplied by the
  deployment environment. TensorRT is not selected or bundled by FileTwin setup.

`ort`/`ort-sys` are pinned to `2.0.0-rc.13` with binary-download features off.
Reference profiles retain one CPU thread and disabled graph optimizations.
Optimized profiles enable level-3 graph optimizations, sequential graph execution
and bounded intra-op threads (default two). CoreML uses MLProgram with all compute
units eligible and low-precision GPU accumulation disabled; CUDA uses device 0
by default, heuristic convolution search and TF32 disabled. Model precision and
operator fallback still depend on the provider, so each backend has distinct
profile IDs and must be qualified independently. A requested GPU that cannot
initialize fails explicitly, while successfully initialized providers can execute
unsupported graph nodes on CPU. Per-file extraction records include backend,
thread count, model reuse and loading/inference/worker timings.

CoreML compilation is serialized across workers and cached in the invocation's
private temporary directory, keyed by runtime version and model SHA-256. It is
removed on normal return, error or cancellation; its size is outside source-copy
staging allowances. CUDA library search directories are explicit host settings;
worker environments do not inherit `LD_LIBRARY_PATH`. FFmpeg and ffprobe remain
version 9 and their full version strings are recorded in file provenance.

Deploy `filetwin`, its sibling `filetwin-worker`, and the provisioned model/runtime
directory plus notices. The target-specific runtime libraries differ across
macOS ARM64, Linux x86-64 and Linux ARM64; the ONNX model is portable. A host can
use `setup-native.py --onnx PATH` to provision an existing verified model without
PyTorch or model conversion; Python 3 still runs the setup script. Running the
deployed binaries with provisioned assets requires no Python installation.
The source setup never substitutes an artifact if a checksum differs. Signed
bundles and Ubuntu/minimum-macOS runtime qualification remain release work.

Rust hosts set absolute paths in `EncoderConfig.runtime`. CLI users provision
`--model-dir` and may override companion/FFmpeg paths with the environment
variables documented in the README. Plain UTF-8 encoding and reuse of already
verified compatible vectors work without native assets. Source metadata is never
modified; vectors are returned directly and optionally saved to one JSON file.
On reused records, extraction provenance describes the original encoding.

## Validation commands

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release --workspace --bins --examples
python3 scripts/native-smoke.py target/release/filetwin --model-dir .filetwin/models
# Apple Silicon, using the same provisioned assets:
python3 scripts/native-smoke.py target/release/filetwin --model-dir .filetwin/models --backend coreml
# Linux x86-64 with the CUDA 12 bundle and installed NVIDIA dependencies:
python3 scripts/native-smoke.py target/release/filetwin --model-dir .filetwin/models --backend cuda
python3 scripts/setup-format-fixtures.py --directory target/format-fixtures
python3 scripts/format-smoke.py target/release/filetwin --model-dir .filetwin/models \
  --heif-fixtures target/format-fixtures
target/model-tools/bin/python scripts/verify-sscd.py FIRST.png SECOND.png \
  --model-dir .filetwin/models \
  --torchscript target/native-assets/sscd_disc_mixup.torchscript.pt
```

The smoke test generates its own fixtures. It checks document equivalence,
image formats/orientation, audio re-encoding/appended silence/hard negatives,
video transcode/timestamps/short clips, unrelated noise recordings, compatible profiles, cache reuse,
rename reuse, unchanged originals, malformed/blank inputs and native
orphan cleanup. The parity harness sends the exact Rust-preprocessed tensor to
the official TorchScript model and compares the returned components.

## Historical optimization evidence

Measured locally on 2026-09-09, Apple M5 Pro (18 CPU cores, 64 GiB RAM), macOS
ARM64, release build, pinned SSCD/ONNX Runtime 1.28.2 and FFmpeg 9.0.1.
These measurements preceded the 0.2 interface rewrite. The same 24-file sample
contained 12 images and 12 videos. Each run forced fresh encoding and hashing,
with no cached vectors. Filesystem and operating-system model caches were not cleared;
these single runs describe this sample, not a general hardware guarantee.

| Encoding path | Wall time, including staging and hashing | Speedup over old executable |
| --- | ---: | ---: |
| Pre-optimization executable, one worker/thread, model loaded per file | 68.219 s | 1× |
| Optimized CPU, 2 workers × 2 inference threads | 11.871 s | 5.7× |
| CoreML, 2 workers, new FileTwin compilation-cache directory | 6.271 s | 10.9× |

Both optimized paths selected exactly the original video's decoded timestamps
for every sample clip. Minimum original/optimized vector cosine was greater
than 0.99999999997. Across all compatible image/image and video/video pairs,
the largest absolute score change was 8.58e-7 for CPU and 5.05e-7 for CoreML.
Each optimized run loaded two model sessions and reused them for the other 22
files. These checks establish agreement on this sample; backend-specific profile
IDs remain separate because arithmetic and supported operations can vary.

A separate full-collection check covered 355 files (181 images, 173 videos and
one metadata file that failed strict UTF-8 decoding). Fresh CoreML encoding and
whole-file hashing took **79.855 s**, compared with **988.762 s** in the previous
run, a 12.4× improvement. All 354 media files remained ready; their original
contents, modification times and recorded hashes were unchanged. Every video's
decoded timestamps matched the old run. The largest absolute change among all
31,168 compatible pair scores was 9.02e-7. Version 0.2 preserves these encoding
profiles and leaves pairwise comparison to callers of the returned vectors.

The 0.2 checks cover portable cache validation and source hashing alongside real
concurrent worker admission, process reuse/replacement, cancellation and decoder
cleanup, source changes, staging limits, preserved profile identities and explicit
backend selection. See [implementation-plan.md](implementation-plan.md) for current
validation status. The original reference path also has a TorchScript
tensor/inference parity harness. The generated schema set now consists of vector
files, progress, errors and profiles.

The Linux x86-64 CUDA 12 bundle was provisioned from Microsoft's official
archive. Its archive, core runtime, CUDA provider and shared-provider checksums
were verified, and the installed libraries were identified as x86-64 ELF.
**NVIDIA inference, numerical agreement, memory use and speed remain untested
on physical NVIDIA hardware.** Run the CUDA smoke command above on the target
machine; selecting CUDA reports initialization failures instead of substituting
a CPU profile. Other Linux/macOS release qualification remains separate.

Private collection paths, source files, vectors and detailed timing logs remain
in the ignored local `target` directory and are not distributed with the source.
