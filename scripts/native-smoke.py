#!/usr/bin/env python3
"""Real native encoder integration checks; requires provisioned models and FFmpeg 9."""
import argparse
import json
import math
import os
import random
from pathlib import Path
import shutil
import sqlite3
import struct
import subprocess
import tempfile
import time
import wave
import zipfile
import zlib


def png(path, width, height, pixels, orientation=None):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    raw = b"".join(b"\0" + bytes(sum((list(pixels[y * width + x]) for x in range(width)), [])) for y in range(height))
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    if orientation:
        exif = b"II*\0\x08\0\0\0" + struct.pack("<H", 1) + struct.pack("<HHIHHI", 0x112, 3, 1, orientation, 0, 0)
        data += chunk(b"eXIf", exif)
    path.write_bytes(data + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def pdf(path, text):
    content = f"BT /F1 12 Tf 50 750 Td ({text}) Tj ET".encode()
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>", b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
               b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
               b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
               b"<< /Length " + str(len(content)).encode() + b" >>\nstream\n" + content + b"\nendstream"]
    data = bytearray(b"%PDF-1.7\n")
    offsets = [0]
    for i, obj in enumerate(objects, 1):
        offsets.append(len(data))
        data.extend(f"{i} 0 obj\n".encode() + obj + b"\nendobj\n")
    xref = len(data)
    data.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        data.extend(f"{offset:010d} 00000 n \n".encode())
    data.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode())
    path.write_bytes(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--ffmpeg", default=shutil.which("ffmpeg"))
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    model_dir = args.model_dir.resolve()
    report = {}
    with tempfile.TemporaryDirectory(prefix="filetwin-native-check-") as tmp:
        tmp = Path(tmp)
        source = tmp / "input"
        source.mkdir()
        data = tmp / "data"
        def ffmpeg(*cmd):
            subprocess.run([args.ffmpeg, "-hide_banner", "-loglevel", "error", "-y", *map(str, cmd)], check=True, timeout=60)
        def cli(*cmd, expected=0):
            p = subprocess.run([str(binary), "--data-dir", str(data), "--model-dir", str(model_dir),
                                "--ffmpeg-path", args.ffmpeg, "--format", "jsonl", *map(str, cmd)], capture_output=True, text=True, timeout=300)
            events = [json.loads(line) for line in p.stdout.splitlines()]
            assert p.returncode == expected, (p.returncode, p.stderr, [e for e in events if e["type"] in ["summary", "error"]])
            return events[-1]["data"]
        text = "FileTwin checks document text across formats."
        (source / "document.txt").write_text(text + "\n")
        with zipfile.ZipFile(source / "document.docx", "w", zipfile.ZIP_DEFLATED) as z:
            z.writestr("[Content_Types].xml", '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>')
            z.writestr("word/document.xml", f'<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>')
        pdf(source / "document.pdf", text)
        width, height = 96, 64
        pixels = [((x * 3 + y * 7) % 256, (x * 11 + y * 5) % 256, (x * y) % 256) for y in range(height) for x in range(width)]
        png(source / "picture.png", width, height, pixels)
        for suffix in ["jpg", "webp", "tiff", "bmp", "gif", "ppm", "ico"]:
            subprocess.run([str(binary.parent / "examples/transcode"), str(source / "picture.png"), str(source / f"picture.{suffix}")], check=True, timeout=30)
        png(source / "oriented.png", width, height, pixels, 6)
        rotated = [pixels[(height - 1 - x) * width + y] for y in range(width) for x in range(height)]
        png(source / "rotated.png", height, width, rotated)
        for name, base in [("recording", 440), ("different", 910)]:
            with wave.open(str(source / f"{name}.wav"), "wb") as wav:
                wav.setnchannels(1); wav.setsampwidth(2); wav.setframerate(16000)
                samples = [int(12000 * math.sin(2 * math.pi * (base + 30 * math.sin(i / 6000)) * i / 16000)) for i in range(3 * 16000)]
                wav.writeframes(struct.pack("<" + "h" * len(samples), *samples))
        ffmpeg("-i", source / "recording.wav", "-ar", "44100", "-b:a", "128k", source / "recording.mp3")
        ffmpeg("-i", source / "recording.wav", "-c:a", "aac", source / "audio-only.mp4")
        ffmpeg("-i", source / "recording.wav", "-af", "apad=pad_dur=1", source / "recording-appended.wav")
        ffmpeg("-f", "lavfi", "-i", "testsrc2=size=320x240:rate=12:duration=3", "-c:v", "libx264", "-pix_fmt", "yuv420p", source / "movie.mp4")
        ffmpeg("-i", source / "movie.mp4", "-vf", "scale=160:120", "-c:v", "mpeg4", "-q:v", "4", source / "movie.avi")
        summary = cli("scan", source, "--experimental", "--threshold", "-1", "--exact-duplicates", "compute")
        assert summary["counts"]["files_ready"] == len(list(source.iterdir())), summary
        files = cli("results", "--run", summary["run_id"], "--kind", "files")["items"]
        by_name = {Path(f["locator"]["root"]).name:f for f in files}
        assert by_name["audio-only.mp4"]["family"] == "audio"
        assert {f["family"] for f in files} == {"text", "image", "audio", "video"}
        db = sqlite3.connect(data / "index.sqlite3")
        def score(a, b):
            vectors = []
            for name in [a, b]:
                raw = db.execute("SELECT payload FROM vectors WHERE id=?", (by_name[name]["vector_id"],)).fetchone()[0]
                vectors.append(struct.unpack("<" + "f" * (len(raw) // 4), raw))
            a, b = vectors
            return sum(x*y for x,y in zip(a,b)) / math.sqrt(sum(x*x for x in a)*sum(x*x for x in b))
        report["scores"] = {
            "text_docx":score("document.txt", "document.docx"), "text_pdf":score("document.txt", "document.pdf"),
            "image_jpeg":score("picture.png", "picture.jpg"), "image_exif_orientation":score("oriented.png", "rotated.png"),
            "audio_mp3":score("recording.wav", "recording.mp3"), "audio_aac":score("recording.wav", "audio-only.mp4"),
            "audio_appended_silence":score("recording.wav", "recording-appended.wav"),
            "audio_different":score("recording.wav", "different.wav"), "video_transcode_resize":score("movie.mp4", "movie.avi")}
        assert report["scores"]["text_docx"] > 0.999999
        assert report["scores"]["text_pdf"] > 0.95
        assert report["scores"]["image_exif_orientation"] > 0.999999
        assert report["scores"]["image_jpeg"] > 0.8
        assert report["scores"]["audio_mp3"] > 0.85
        assert report["scores"]["audio_aac"] > 0.85
        assert report["scores"]["audio_different"] < 0.5
        assert report["scores"]["audio_appended_silence"] > 0.9
        assert report["scores"]["video_transcode_resize"] > 0.8
        for name in ["movie.mp4", "movie.avi"]:
            extraction = by_name[name]["extraction"]
            frames = extraction["frames"]
            assert 1 <= len(frames) <= 32
            assert len({f["decoded_pts"] for f in frames}) == len(frames)
            assert max(f["relative_seconds"] for f in frames) > 0.8 * extraction["duration_seconds"]
            report[name] = {"distinct_frames":len(frames), "last_pts_seconds":max(f["relative_seconds"] for f in frames)}
        pairs = cli("results", "--run", summary["run_id"], "--kind", "pairs")["items"]
        ids = {f["file_id"]:f for f in files}
        for pair in pairs:
            if pair["match_kind"] == "similar_content":
                assert ids[pair["file_a"]]["profile_id"] == ids[pair["file_b"]]["profile_id"] == pair["profile_id"]
        report["counts"] = summary["counts"]
        cached = cli("scan", source, "--experimental", "--threshold", "-1")
        assert cached["counts"]["cache_hits"] == len(files) and cached["counts"]["bytes_read"] == 0
        filtered = cli("scan", source / "audio-only.mp4", "--experimental", "--families", "audio", "--threshold", "0.95")
        assert filtered["counts"]["files_ready"] == 1
        renamed = tmp / "originals-renamed"
        source.rename(renamed)
        frozen = cli("compare", "--snapshot", summary["snapshot_id"], "--threshold", "0.95")
        assert frozen["counts"]["bytes_read"] == 0
        # Corrupt/empty/unsupported input is explicit and must never get a vector.
        bad = tmp / "bad"; bad.mkdir()
        (bad / "broken.png").write_bytes(b"\x89PNG\r\n\x1a\ninvalid")
        (bad / "broken.pdf").write_bytes(b"%PDF-1.7\ninvalid")
        pdf(bad / "blank.pdf", "")
        with zipfile.ZipFile(bad / "archive.zip", "w") as z:
            z.writestr("arbitrary.txt", "Not a DOCX document")
        failure = cli("scan", bad, "--experimental", "--threshold", "0.95", expected=3)
        assert failure["counts"]["files_failed"] == 4 and failure["counts"]["files_ready"] == 0
        limited = cli("scan", renamed / "picture.png", "--experimental", "--families", "image", "--threshold", "0.95", "--staging-bytes", "1", "--cache-mode", "refresh", expected=3)
        assert limited["counts"]["files_ready"] == 0
        # A one-frame clip still gets one descriptor; empty midpoint targets do
        # not manufacture duplicate evidence or discard the available frame.
        still = tmp / "still.mp4"
        ffmpeg("-i", renamed / "picture.png", "-frames:v", "1", "-r", "1", "-c:v", "libx264", "-pix_fmt", "yuv420p", still)
        short = cli("scan", still, "--experimental", "--families", "video", "--threshold", "0.95")
        short_file = cli("results", "--run", short["run_id"], "--kind", "files")["items"][0]
        assert short_file["extraction"]["distinct_frames"] == 1
        delayed = tmp / "delayed.mp4"
        ffmpeg("-i", renamed / "recording.wav", "-itsoffset", "1", "-i", renamed / "movie.mp4", "-map", "0:a:0", "-map", "1:v:0", "-c:a", "aac", "-c:v", "copy", delayed)
        delayed_summary = cli("scan", delayed, "--experimental", "--families", "video", "--threshold", "0.95")
        delayed_file = cli("results", "--run", delayed_summary["run_id"], "--kind", "files")["items"][0]
        by_name["delayed.mp4"] = delayed_file
        assert delayed_file["extraction"]["stream_start_seconds"] >= 0.99
        assert max(f["decoded_pts_seconds"] for f in delayed_file["extraction"]["frames"]) > 3.8
        report["scores"]["video_shifted_timestamps"] = score("movie.mp4", "delayed.mp4")
        assert report["scores"]["video_shifted_timestamps"] > 0.99999
        noise_paths = []
        for seed in [0, 1]:
            path = tmp / f"noise-{seed}.wav"
            rng = random.Random(seed)
            with wave.open(str(path), "wb") as wav:
                wav.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
                wav.writeframes(struct.pack("<" + "h" * 48000, *[rng.randrange(-10000, 10001) for _ in range(48000)]))
            noise_paths.append(path)
        noise_summary = cli("scan", *noise_paths, "--experimental", "--families", "audio", "--threshold", "-1")
        noise_pair = cli("results", "--run", noise_summary["run_id"], "--kind", "pairs")["items"][0]
        report["scores"]["unrelated_noise_recordings"] = noise_pair["score"]
        assert noise_pair["score"] < 0.3
        # Verify cleanup by the real worker when the embedding host is killed.
        pid_file = tmp / "decoder.pid"
        fake = tmp / "blocked-ffmpeg"
        fake.write_text(f"#!/bin/sh\nprintf '%s' \"$$\" > '{pid_file}'\nexec /bin/sleep 120\n")
        fake.chmod(0o700)
        host = subprocess.Popen([str(binary), "--data-dir", str(tmp/"orphan-check"), "--model-dir", str(model_dir),
                                 "--ffmpeg-path", str(fake), "scan", str(renamed/"recording.wav"), "--experimental", "--families", "audio", "--threshold", "0.95", "--format", "jsonl"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic()+10
            while not pid_file.exists():
                assert host.poll() is None and time.monotonic()<deadline
                time.sleep(0.02)
            pid = int(pid_file.read_text())
            host.kill()
            host.communicate(timeout=5)
            deadline = time.monotonic()+5
            while True:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                proc_stat = Path(f"/proc/{pid}/stat")
                if proc_stat.exists() and ") Z " in proc_stat.read_text():
                    break
                assert time.monotonic()<deadline, "Orphan decoder survived its host"
                time.sleep(0.02)
        finally:
            if host.poll() is None:
                host.kill(); host.communicate(timeout=5)
        report["checks"] = ["all families", "document extraction", "image formats and EXIF", "audio codec/rate changes and hard negatives including unrelated noise", "video transcode and timeline coverage", "mixed profile partition", "zero-read cache", "audio-only MP4 detection", "compare without originals", "corrupt and blank inputs", "staging admission", "single-frame video", "shifted video stream timestamps", "native orphan cleanup after host SIGKILL"]
    print(json.dumps(report, indent=2))
    if args.report:
        args.report.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
