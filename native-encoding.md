# Native encoding implementation

The current implementation supplies encoders for all four content families in
the design. Each file has one normalized float32 vector under one immutable
profile. Comparisons and groups require equal family **and** profile ID. A
95% display cutoff means cosine >= 0.95; it is not a confidence estimate.

## Representations and coverage

| Profile name | Representation | Coverage and limitations |
| --- | --- | --- |
| `experimental-text-v1` | 4,096-dimensional signed character n-grams | Original strict UTF-8 reader, retained unchanged for old snapshots |
| `experimental-documents-v1` | Same text features, with a new reader-policy manifest | UTF-8; DOCX main body; PDFium page text. These inputs can match each other. DOCX auxiliary parts, layout and OCR are excluded. Empty/scanned PDF pages prevent a complete text vector. |
| `experimental-image-sscd-v1` | SSCD `sscd_disc_mixup`, ResNet50, 512 dimensions | Original raster-only profile retained unchanged for existing snapshots |
| `experimental-image-sscd-v2` | Same SSCD representation; expanded reader policy | First raster image/frame with EXIF orientation, or FFmpeg 9 primary SDR HEIF/AVIF image including tile-grid reconstruction and crop/rotation. RGB8, alpha on black, triangle resize directly to 320×320, official ImageNet channel normalization |
| `experimental-audio-landmarks-v1` | 4,096-dimensional signed spectral-peak-triplet sketch | First audio stream, mono 16 kHz; up to 32 disjoint eight-second windows centered across the timeline. Files up to 256 seconds cover the complete timeline. FFT 1024/hop 256, periodic Hann; peak triples at offsets (7,3,0), (15,7,0), (31,15,0), (63,31,0); signed square-root counts and L2 normalization. |
| `experimental-video-sscd-v1` | Mean of normalized SSCD frame descriptors, then L2 normalization | 32 midpoint targets across the entire timeline, deduplicated by decoded PTS. The first frame is a fallback if no midpoint yields a frame. Actual integer PTS, time base, seconds and missing targets are retained. Audio and temporal order are excluded. |

`profiles list` is the authoritative source for profile IDs and full manifests.
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
v2 selection recomputes image vectors under its new cache binding.

Media signatures and extension hints route recognized containers to ffprobe.
The detected streams decide audio versus video, ignoring attached cover art;
an audio-only MP4 is audio even when its extension is `.mp4`. Unselected families
are excluded. Unsupported codecs, malformed media and unknown durations return
explicit failures. Video uses FFmpeg's SDR RGB conversion and autorotation;
PQ/HLG HDR video is rejected until a tone-mapping profile is qualified.

## Native boundary and limits

`filetwin-core` opens original files without following symlinks and copies a
cache miss into a private temporary directory. It counts source bytes read and
optionally hashes original bytes during the copy. Native reads of that private
copy and model/runtime reads are not counted again as source I/O. Before saving
a result, the coordinator verifies the source's descriptor and pathname still
refer to the observed revision. Fast freshness remains heuristic.

The companion receives an internal versioned JSON request through a bounded
file. The public CLI protocol is separate. Worker stdout/stderr are private,
size-limited files, so native output cannot corrupt JSONL or block the host on a
full pipe. The coordinator checks cancellation and job time while waiting, with
a 300-second per-file ceiling. A private process group contains the worker and
all FFmpeg children. Cancellation, failure and normal termination clean up the
group and staging; a worker also terminates its group if its host disappears.
Crash-staging garbage collection and model reuse between files remain later work.

Native source staging must fit `staging_bytes` minus a 2 MiB protocol reserve.
The default is 10 GiB and can be raised for larger files. Buffers and image
dimensions are capped. Native document/audio admission requires 128 MiB;
image/video requires 512 MiB. Larger frames can require a higher allowance.
Linux additionally applies a worker address-space limit. macOS has no reliable
hard RSS limit. These are developer safeguards, not demonstrated 10 TB capacity.

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
- Native worker model and library checksums again before inference/loading.

`ort`/`ort-sys` are pinned to `2.0.0-rc.13` with binary-download features off.
ONNX inference uses CPU, one thread, sequential execution and disabled graph
optimizations. Model memory is verified before loading. FFmpeg and ffprobe must
be version 9; their full version strings are recorded in file provenance.

Deploy `filetwin`, its sibling `filetwin-worker`, and the provisioned model/runtime
directory plus notices. The target-specific runtime libraries differ across
macOS ARM64, Linux x86-64 and Linux ARM64; the ONNX model is portable. A host can
use `setup-native.py --onnx PATH` to provision an existing verified model without
PyTorch or model conversion; Python 3 still runs the setup script. Running the
deployed binaries with provisioned assets requires no Python installation.
The source setup never substitutes an artifact if a checksum differs. Signed
bundles and Ubuntu/minimum-macOS runtime qualification remain release work.

Rust hosts set absolute paths in `EngineConfig.runtime`. CLI users can use
[native-config.toml](examples/native-config.toml) or global path flags. Read-only
queries, cached comparison and plain UTF-8 encoding work without native assets.
Source file metadata is never modified; vectors remain in the SQLite index.

## Validation commands

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release --workspace --bins --examples
python3 scripts/native-smoke.py target/release/filetwin --model-dir .filetwin/models
python3 scripts/setup-format-fixtures.py --directory target/format-fixtures
python3 scripts/format-smoke.py target/release/filetwin --model-dir .filetwin/models \
  --heif-fixtures target/format-fixtures
target/model-tools/bin/python scripts/verify-sscd.py FIRST.png SECOND.png \
  --model-dir .filetwin/models \
  --torchscript target/native-assets/sscd_disc_mixup.torchscript.pt
```

The smoke test generates its own fixtures. It checks document equivalence,
image formats/orientation, audio re-encoding/appended silence/hard negatives,
video transcode/timestamps/short clips, unrelated noise recordings, family partitioning, cache reuse,
comparison without originals, malformed/blank inputs, staging limits and native
orphan cleanup. The parity harness sends the exact Rust-preprocessed tensor to
the official TorchScript model and compares the returned components.
