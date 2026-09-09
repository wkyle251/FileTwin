#!/usr/bin/env python3
"""Explicitly download checksum-pinned image conformance fixtures for local tests.

FileTwin processing and the format tests never download input files themselves.
The third-party test inputs remain under the selected local fixture directory.
"""
import argparse
import hashlib
import json
from pathlib import Path
import urllib.request


FIXTURES = [
    {"file": "primary.heic", "url": "https://raw.githubusercontent.com/strukturag/libheif/master/examples/example.heic",
     "sha256": "7f8b363e4936c0666a25f64f3a92fda10bd8e5453be4592530b65a55dd98f3f2"},
    {"file": "grid.heic", "url": "https://fate-suite.ffmpeg.org/heif-conformance/C007.heic",
     "sha256": "9b948314afed828fb29dd8265933145a2209c0abdc0fbb21bc17e060b3ba3706"},
    {"file": "transformed.heic", "url": "https://fate-suite.ffmpeg.org/heif-conformance/MIAF007.heic",
     "sha256": "370489520fd6f97dab4dbf2e81c1d77f9e19b0bfb28996245a2dc313af8ea81c"},
    {"file": "hdr.hif", "url": "https://fate-suite.ffmpeg.org/heif/P1001091.HIF",
     "sha256": "f8151ff6f2b71cad4eb97ab98453630074283d423c4c434c1e60b34775ccf02f"},
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, default=Path("target/format-fixtures"))
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    for entry in FIXTURES:
        path = args.directory / entry["file"]
        if path.exists() and hashlib.sha256(path.read_bytes()).hexdigest() == entry["sha256"]:
            continue
        with urllib.request.urlopen(entry["url"], timeout=60) as response:
            data = response.read(16 * 1024 * 1024 + 1)
        if len(data) > 16 * 1024 * 1024 or hashlib.sha256(data).hexdigest() != entry["sha256"]:
            raise RuntimeError(f"Fixture integrity check failed: {entry['file']}")
        pending = path.with_suffix(path.suffix + ".download")
        pending.write_bytes(data)
        pending.replace(path)
    (args.directory / "sources.json").write_text(json.dumps(FIXTURES, indent=2) + "\n")
    print(f"Verified {len(FIXTURES)} conformance fixtures in {args.directory.resolve()}")


if __name__ == "__main__":
    main()
