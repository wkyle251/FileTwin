# File format coverage

FileTwin supports the design's four content families. A supported family is not
a promise to decode every format or every variant ever created. Every discovered
regular file receives an explicit outcome under the selected profiles and limits.

For arbitrary readable regular files, `--exact-duplicates compute` records SHA-256
evidence and can group byte-identical files regardless of their format. Content
similarity additionally requires successful decoding into one of these profiles.
An unsupported, corrupt, encrypted, empty or insufficient input never receives
a placeholder vector. Unsupported/failed content makes coverage partial and the
CLI exits with code 3 while still publishing the usable results.

## Exercised inputs

The release-format test generates document/raster/media fixtures locally. Optional
checksum-pinned conformance inputs exercise HEIC features. Codec names below are
the actual fixture encodings, not a claim about every codec inside a container.

| Family | Formats exercised |
| --- | --- |
| Text | UTF-8 TXT, Markdown, CSV, JSON, XML, HTML and source-code filenames; UTF-8 BOM/CRLF; DOCX main body; text-based PDF |
| Image | PNG, JPEG, WebP, TIFF, BMP, GIF, ICO, PBM/PGM/PPM/PAM, TGA, QOI, farbfeld, Radiance HDR, OpenEXR and DDS BC1/DXT1 |
| Image additions | AVIF/AV1 still image; HEIC/HEVC primary item with thumbnails, four-tile reconstruction, cropping/mirroring/rotation; equivalent decoded PNG comparisons |
| Audio | WAV/PCM, MP3, FLAC, raw AAC, M4A/AAC, AIFF/AIF PCM, Opus, Ogg/Vorbis, OGA/FLAC, MP2, WMA/WMAv2, WavPack, CAF/PCM, AU/PCM, AC3/EAC3, MKA/FLAC and audio-only MP4/AAC |
| Video | MP4/MOV/MKV H.264, AVI/MPEG-4, WebM/VP9, MPEG-PS/MPEG-1/2, TS/MTS/M2TS H.264, FLV, WMV/WMV2 and 3GP/H.264 |

Plain-text markup/source files are compared as source text. PDF extraction uses
page text; DOCX extraction uses main-body text, with the exclusions recorded in
its immutable profile. Animated/multipage raster files use their first image.
HEIF uses the primary image or assembled grid, not a thumbnail or one tile.
Audio and video use their documented bounded timeline sampling policies.

The suite also checks content detection after changing file extensions, finite
unit-length stored vectors of the correct dimension, cache reuse without source
reads, persisted comparison after moving originals, and explicit failure outcomes.
It confirms that unsupported binary files can still form an exact-copy group.

## Remaining exclusions

- OCR/scanned PDF content; legacy binary Office; presentation/spreadsheet readers.
- Generic archive traversal, executable-content analysis and disk images.
- SVG rendering and RAW-camera formats without a declared reader.
- PQ/HLG HEIF/AVIF and video, pending a qualified tone-mapping profile. Separate
  HEIF auxiliary alpha/depth items are not composed; FFmpeg-exposed alpha is kept.
- Encrypted/password-protected content, absent runtime codecs, malformed files,
  unknown media duration, and inputs exceeding the configured resource limits.

These exclusions concern content similarity; readable original bytes can still
supply exact-copy evidence. Selecting fewer families records other recognized
families as excluded. Missing native dependencies remain explicit failures.

## Reproduce

Local verification on 2026-09-09 passed with **72 ready files** (12 text/document,
26 image, 20 audio and 14 video), **672 comparisons**, and **72 zero-read cache
hits**. Four HEIC/AVIF-to-PNG checks scored exactly 1.0. Six unsupported or invalid
inputs produced explicit outcomes, with byte-identical binary inputs still
forming an exact-copy pair. All 40 workspace tests also passed on this Mac.

Build and provision the native assets as described in [README.md](README.md).
The full fixture generator requires FFmpeg 9 with libsvtav1, libvpx-vp9,
libx264, libmp3lame, libopus and the listed native encoders. Processing support
depends on the codecs present in the installed FFmpeg build.

```sh
cargo build --locked --release --workspace --bins --examples
python3 scripts/setup-format-fixtures.py --directory target/format-fixtures
python3 scripts/format-smoke.py target/release/filetwin \
  --model-dir .filetwin/models --heif-fixtures target/format-fixtures \
  --report target/format-results.json
```

The setup command explicitly downloads reference inputs from the
[FFmpeg FATE suite](https://fate-suite.ffmpeg.org/heif-conformance/) and
[libheif example](https://github.com/strukturag/libheif/tree/master/examples),
checking the SHA-256 values frozen in the setup script. Test/processing commands
never download inputs. Omitting `--heif-fixtures` explicitly reports HEIC
conformance as not run. These are local developer checks; broader codec-variant,
platform and accuracy qualification remains in the implementation plan.
