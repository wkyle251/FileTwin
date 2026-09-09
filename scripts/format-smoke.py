#!/usr/bin/env python3
"""Exercise every advertised format through the release CLI and saved vectors.

Fixtures are generated locally; native runtime assets and FFmpeg 9 are required.
This is a format/contract check, not an accuracy calibration corpus.
"""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import time
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--ffmpeg", default=shutil.which("ffmpeg"))
    parser.add_argument("--heif-fixtures", type=Path, help="Conformance fixtures from setup-format-fixtures.py")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    binary, models = args.binary.resolve(), args.model_dir.resolve()
    environment = {k: v for k, v in os.environ.items() if not k.startswith("FILETWIN_")}
    environment["FILETWIN_FFMPEG"] = str(Path(args.ffmpeg).resolve())
    spec = importlib.util.spec_from_file_location("native_smoke", Path(__file__).with_name("native-smoke.py"))
    fixtures = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixtures)
    with tempfile.TemporaryDirectory(prefix="filetwin-format-check-") as temporary:
        base = Path(temporary)
        source = base / "input"
        source.mkdir()
        expected = {}
        image_equivalents = []

        def case(name, family, description):
            expected[name] = {"family": family, "fixture": description}
            return source / name

        def ffmpeg(*arguments):
            command = [args.ffmpeg, "-hide_banner", "-loglevel", "error", "-y", "-threads", "1", *map(str, arguments)]
            result = subprocess.run(command, capture_output=True, text=True, timeout=90)
            assert result.returncode == 0, (command, result.stderr)

        def cli(*arguments, expected_exit=0):
            command = [str(binary), *map(str, arguments), "--model-dir", str(models)]
            result = subprocess.run(command, capture_output=True, text=True, env=environment, timeout=300)
            response = json.loads(result.stdout)
            assert result.returncode == expected_exit, (command, result.returncode, response, result.stderr)
            return response

        body = "FileTwin compares the complete document body across supported formats."
        for suffix in ["txt", "md", "csv", "json", "xml", "html", "rs", "py", "ts"]:
            case(f"text.{suffix}", "text", "strict UTF-8 source text").write_text(body + "\n")
        case("utf8-bom.txt", "text", "UTF-8 BOM and CRLF").write_bytes(b"\xef\xbb\xbf" + body.encode() + b"\r\n")
        document = case("document.docx", "text", "Word main-body text")
        with zipfile.ZipFile(document, "w", zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("[Content_Types].xml", '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>')
            archive.writestr("word/document.xml", f'<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{body}</w:t></w:r></w:p></w:body></w:document>')
        fixtures.pdf(case("document.pdf", "text", "PDF with extractable page text"), body)

        width, height = 96, 64
        pixels = [((x * 3 + y * 7) % 256, (x * 11 + y * 5) % 256, (x * y) % 256)
                  for y in range(height) for x in range(width)]
        reference = case("picture.png", "image", "RGB PNG")
        fixtures.png(reference, width, height, pixels)
        for suffix in ["jpg", "webp", "tiff", "bmp", "gif", "ico", "pbm", "pgm", "ppm", "pam", "tga", "qoi", "ff", "hdr", "exr"]:
            output = case(f"picture.{suffix}", "image", f"image-rs {suffix} encoder fixture")
            subprocess.run([str(binary.parent / "examples/transcode"), str(reference), str(output)], check=True, timeout=30)
        # DDS BC1/DXT1: complete 124-byte header and independently specified blocks.
        header = [124, 0x81007, height, width, width * height // 2, 0, 0] + [0] * 11
        header += [32, 4, int.from_bytes(b"DXT1", "little"), 0, 0, 0, 0, 0, 0x1000, 0, 0, 0, 0]
        blocks = b"".join(struct.pack("<HHI", 0xF800 if (x + y) % 2 else 0x07E0, 0x001F, 0xE4E4E4E4)
                          for y in range(height // 4) for x in range(width // 4))
        case("picture.dds", "image", "DDS BC1/DXT1 blocks").write_bytes(b"DDS " + struct.pack("<31I", *header) + blocks)
        shutil.copyfile(reference, case("renamed-image.bin", "image", "PNG content with unrelated extension"))
        avif = case("picture.avif", "image", "AV1 still image in AVIF")
        ffmpeg("-i", reference, "-vf", "scale=128:128", "-c:v", "libsvtav1", "-preset", "12", "-crf", "20",
               "-svtav1-params", "lp=1", "-frames:v", "1", avif)
        avif_reference = case("avif-reference.png", "image", "decoded AVIF pixels for comparison")
        ffmpeg("-i", avif, "-map", "0:0", "-frames:v", "1", "-pix_fmt", "rgba", avif_reference)
        image_equivalents.append((avif.name, avif_reference.name))
        if args.heif_fixtures:
            for name, description in [("primary.heic", "primary item with thumbnails"),
                                      ("grid.heic", "four-tile HEIC, full 2560 by 1440 image"),
                                      ("transformed.heic", "cropping, mirroring and rotation")]:
                input_path = case(name, "image", description)
                shutil.copyfile(args.heif_fixtures / name, input_path)
                reference_path = case(name + ".png", "image", "decoded HEIC pixels for comparison")
                ffmpeg("-i", input_path, "-map", "[0:g:0]" if name == "grid.heic" else "0:0",
                       "-frames:v", "1", "-pix_fmt", "rgba", reference_path)
                image_equivalents.append((name, reference_path.name))

        wav = case("recording.wav", "audio", "PCM signed 16-bit WAV, three seconds")
        ffmpeg("-f", "lavfi", "-i", "aevalsrc=0.4*sin(2*PI*(440+30*sin(t))*t):s=16000:d=3", "-c:a", "pcm_s16le", wav)
        audio = [
            ("mp3", ["-c:a", "libmp3lame"]), ("flac", ["-c:a", "flac"]),
            ("aac", ["-c:a", "aac"]), ("m4a", ["-c:a", "aac"]),
            ("aiff", ["-c:a", "pcm_s16be"]), ("aif", ["-c:a", "pcm_s16be", "-f", "aiff"]),
            ("opus", ["-c:a", "libopus"]), ("ogg", ["-c:a", "vorbis", "-strict", "experimental", "-ac", "2"]),
            ("oga", ["-c:a", "flac", "-f", "ogg"]), ("mp2", ["-c:a", "mp2", "-ar", "44100"]),
            ("wma", ["-c:a", "wmav2"]), ("wv", ["-c:a", "wavpack"]),
            ("caf", ["-c:a", "pcm_s16le"]), ("au", ["-c:a", "pcm_s16be"]),
            ("ac3", ["-c:a", "ac3"]), ("eac3", ["-c:a", "eac3"]),
            ("mka", ["-c:a", "flac"]), ("mp4", ["-c:a", "aac"]),
        ]
        for suffix, options in audio:
            ffmpeg("-i", wav, *options, case(f"recording.{suffix}", "audio", " ".join(options)))
        shutil.copyfile(wav, case("renamed-audio.bin", "audio", "WAV content with unrelated extension"))

        movie = case("movie.mp4", "video", "H.264 MP4, one second, eight frames")
        ffmpeg("-f", "lavfi", "-i", "testsrc2=size=160x120:rate=8:duration=1", "-c:v", "libx264", "-pix_fmt", "yuv420p", movie)
        video = [
            ("mov", ["-c:v", "copy"]), ("mkv", ["-c:v", "copy"]),
            ("avi", ["-c:v", "mpeg4"]), ("webm", ["-c:v", "libvpx-vp9", "-threads", "1"]),
            ("mpg", ["-c:v", "mpeg2video", "-r", "25"]),
            ("mpeg", ["-c:v", "mpeg1video", "-r", "25"]),
            ("ts", ["-c:v", "copy", "-f", "mpegts"]),
            ("mts", ["-c:v", "copy", "-f", "mpegts"]),
            ("m2ts", ["-c:v", "copy", "-f", "mpegts", "-mpegts_m2ts_mode", "1"]),
            ("flv", ["-c:v", "flv"]), ("wmv", ["-c:v", "wmv2"]),
            ("3gp", ["-c:v", "copy"]),
        ]
        for suffix, options in video:
            ffmpeg("-i", movie, *options, case(f"movie.{suffix}", "video", " ".join(options)))
        shutil.copyfile(movie, case("renamed-video.bin", "video", "MP4 content with unrelated extension"))

        start = time.monotonic()
        original_hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in source.iterdir()}
        saved = base / "vectors.json"
        encoded = cli(source, "--output", saved)
        files = encoded["files"]
        assert len(files) == len(expected), (len(files), len(expected))
        coverage = []
        for file in files:
            name = file["path"]
            wanted = expected[name]
            assert file["state"] == "ready" and file["family"] == wanted["family"], (name, file)
            assert file["file_id"] == "sha256:" + original_hashes[name]
            if name == "grid.heic":
                assert file["extraction"]["tile_grid"] and (file["extraction"]["decoded_width"], file["extraction"]["decoded_height"]) == (2560, 1440), file
            if name == "transformed.heic":
                assert (file["extraction"]["decoded_width"], file["extraction"]["decoded_height"]) == (360, 640), file
            if name == "primary.heic":
                assert (file["extraction"]["decoded_width"], file["extraction"]["decoded_height"]) == (1280, 854), file
            dimensions = 512 if wanted["family"] in ["image", "video"] else 4096
            vector = file["vector"]
            assert len(vector) == dimensions, name
            raw = struct.pack("<" + "f" * dimensions, *vector)
            assert file["vector_sha256"] == hashlib.sha256(raw).hexdigest(), name
            assert all(math.isfinite(value) for value in vector) and abs(sum(value * value for value in vector) - 1) < 1e-5, name
            coverage.append({"file": name, **wanted, "dimensions": dimensions, "state": file["state"]})
        cached = cli(source, saved)["summary"]
        assert cached["counts"]["cache_hits"] == len(expected) and cached["counts"]["vectors_encoded"] == 0, cached
        assert cached["counts"]["bytes_hashed"] == sum(f["bytes"] for f in files)
        assert {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in source.iterdir()} == original_hashes
        renamed = base / "renamed-originals"; source.rename(renamed)
        assert cli(renamed, saved)["summary"]["counts"]["cache_hits"] == len(files)
        family_counts = {family: sum(value["family"] == family for value in expected.values()) for family in ["text", "image", "audio", "video"]}
        by_name = {f["path"]: f for f in files}
        scores = {}
        for pair in image_equivalents:
            a, b = [by_name[n] for n in pair]
            assert a["profile_id"] == b["profile_id"]
            av, bv = a["vector"], b["vector"]
            score = sum(x*y for x,y in zip(av,bv)) / math.sqrt(sum(x*x for x in av)*sum(y*y for y in bv))
            scores[frozenset(pair)] = score
            assert score > 0.999999, (pair, score)
        # Supported families must retain honest outcomes for unreadable or
        # unsupported content, including exact-byte matches across such files.
        bad = base / "unsupported"
        bad.mkdir()
        for name in ["opaque-a.bin", "opaque-b.bin"]:
            (bad / name).write_bytes(b"\x80\0\xffopaque binary payload")
        (bad / "broken.avif").write_bytes(b"\0\0\0\x18ftypavif\0\0\0\0mif1avif")
        with zipfile.ZipFile(bad / "archive.zip", "w") as archive:
            archive.writestr("payload.txt", "An archive is not a document")
        fixtures.pdf(bad / "scanned-or-blank.pdf", "")
        if args.heif_fixtures:
            shutil.copyfile(args.heif_fixtures / "hdr.hif", bad / "hdr.hif")
        unsupported = cli(bad, expected_exit=3)
        counts = unsupported["summary"]["counts"]
        assert counts["files_ready"] == 0 and counts["files_failed"] == len(list(bad.iterdir())), counts
        failed = {f["path"]: f for f in unsupported["files"]}
        assert all(f["file_id"] is not None and f["vector"] is None and f.get("error") for f in failed.values())
        assert failed["opaque-a.bin"]["file_id"] == failed["opaque-b.bin"]["file_id"]
        report = {"cases": sorted(coverage, key=lambda value: (value["family"], value["file"])),
                  "family_counts": family_counts, "files_ready": len(expected),
                  "cache_hits": cached["counts"]["cache_hits"],
                  "heif_conformance": "passed" if args.heif_fixtures else "not_run",
                  "image_equivalent_scores": [{"a":a,"b":b,"score":scores[frozenset([a,b])]} for a,b in image_equivalents],
                  "explicit_unsupported_files": counts["files_failed"],
                  "checks": ["all advertised raster codecs", "AVIF still images", "UTF-8 and documents", "audio containers/codecs", "video containers/codecs", "content detection despite renamed extensions", "finite normalized portable vectors", "verified cache", "rename reuse", "unchanged source bytes", "explicit unsupported outcomes and content IDs for arbitrary readable bytes"],
                  "elapsed_seconds": time.monotonic() - start}
        if args.report:
            args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps({key: value for key, value in report.items() if key != "cases"}, indent=2))


if __name__ == "__main__":
    main()
