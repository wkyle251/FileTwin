#!/usr/bin/env python3
"""Explicitly provision pinned model/native artifacts. Never called by a scan."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
SOURCE_SHA = "9f26bd4c848cc19b73d2ae92eea6e04886f61a7b764ceb7a13aeee62e6a6db56"
SOURCE_URL = "https://dl.fbaipublicfiles.com/sscd-copy-detection/sscd_disc_mixup.torchscript.pt"


def sha(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def fetch(url, path, expected):
    if path.exists() and sha(path) == expected:
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as f:
        temp = Path(f.name)
    try:
        print(f"Downloading {url}", flush=True)
        with urllib.request.urlopen(url, timeout=60) as source, temp.open("wb") as out:
            shutil.copyfileobj(source, out, 1024 * 1024)
        if sha(temp) != expected:
            raise RuntimeError(f"Checksum mismatch: {url}")
        temp.replace(path)
    finally:
        temp.unlink(missing_ok=True)
    return path


def atomic_copy(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as f:
        temp = Path(f.name)
    try:
        shutil.copyfile(source, temp)
        temp.replace(destination)
    finally:
        temp.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--assets-dir", type=Path, default=ROOT / "target/native-assets")
    parser.add_argument("--conversion-python", type=Path, help="Build-only Python with model-requirements.txt installed")
    parser.add_argument("--onnx", type=Path, help="Use an already converted, checksum-verified model (no PyTorch required)")
    args = parser.parse_args()
    model_dir = args.model_dir.resolve()
    assets = args.assets_dir.resolve()
    system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
    arch = {"arm64":"aarch64", "aarch64":"aarch64", "x86_64":"x86_64", "AMD64":"x86_64"}.get(platform.machine())
    target = f"{system}-{arch}"
    lock = json.loads((ROOT / "crates/filetwin-worker/runtime-artifacts.json").read_text())
    if target not in lock["onnxruntime"]:
        raise SystemExit(f"No native artifacts pinned for {target}")
    expected = re.search(r'pub const SSCD_MODEL_SHA256: &str =\s*"([0-9a-f]{64})"',
                         (ROOT / "crates/filetwin-core/src/profile.rs").read_text()).group(1)
    onnx = args.onnx.resolve() if args.onnx else assets / "sscd_disc_mixup.onnx"
    if not onnx.exists() or sha(onnx) != expected:
        if args.onnx:
            raise SystemExit("Supplied ONNX artifact does not match this build")
        if not args.conversion_python:
            raise SystemExit("Supply --conversion-python after installing scripts/model-requirements.txt, or --onnx with the pinned artifact")
        source = fetch(SOURCE_URL, assets / "sscd_disc_mixup.torchscript.pt", SOURCE_SHA)
        with tempfile.TemporaryDirectory(dir=assets) as tmp:
            output = Path(tmp) / "model.onnx"
            subprocess.run([str(args.conversion_python.resolve()), str(ROOT / "scripts/convert-sscd.py"),
                            str(source), str(output), "--check-sha", expected], check=True)
            atomic_copy(output, onnx)
    installed = {"target":target,"model_sha256":expected,"model_source":SOURCE_URL,"artifacts":{}}
    for component, platforms in lock.items():
        artifact = platforms[target]
        archive = fetch(platforms["base_url"] + artifact["archive"], assets / artifact["archive"], artifact["sha256"])
        suffix = "dylib" if system == "macos" else "so"
        destination = model_dir / "runtime" / f"lib{component}.{suffix}"
        destination.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(archive) as tar, tempfile.TemporaryDirectory(dir=assets) as tmp:
            member = tar.getmember(artifact["member"])
            if not member.isfile():
                raise RuntimeError("Pinned library is not a regular archive member")
            output = Path(tmp) / "library"
            with tar.extractfile(member) as source, output.open("wb") as out:
                shutil.copyfileobj(source, out)
            if sha(output) != artifact["library_sha256"]:
                raise RuntimeError("Native library checksum mismatch")
            atomic_copy(output, destination)
            # Preserve all shipped license/notice texts; never extract archive
            # paths directly, and never create links from an archive.
            for notice in tar.getmembers():
                if notice.isfile() and any(s in notice.name.lower() for s in ["license", "notice", "copying"]):
                    safe = notice.name.replace("/", "_").replace("\\", "_")
                    out = model_dir / "notices" / component / safe
                    out.parent.mkdir(parents=True, exist_ok=True)
                    out.write_bytes(tar.extractfile(notice).read())
        installed["artifacts"][component] = {**artifact,"version":platforms["version"]}
    atomic_copy(onnx, model_dir / "sscd_disc_mixup.onnx")
    atomic_copy(ROOT / "THIRD_PARTY_NOTICES.md", model_dir / "notices/THIRD_PARTY_NOTICES.md")
    (model_dir / "installed-artifacts.json").write_text(json.dumps(installed, indent=2) + "\n")
    print(f"Verified native assets installed at {model_dir}")
    print("FFmpeg 9 and ffprobe 9 must be installed separately; run filetwin doctor to inspect paths.")


if __name__ == "__main__":
    main()
