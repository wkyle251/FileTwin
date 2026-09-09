"""Check the directory/vector-file CLI with disposable text fixtures.

Python is only a development tool. Optional --schemas requires jsonschema.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("--schemas", action="store_true")
    parser.add_argument("--record", type=Path)
    args = parser.parse_args()
    executable = args.executable.resolve(strict=True)
    project = Path(__file__).resolve().parent.parent
    validators = {}
    if args.schemas:
        from jsonschema import Draft202012Validator
        for path in (project / "schemas").glob("*.schema.json"):
            schema = json.loads(path.read_text())
            Draft202012Validator.check_schema(schema)
            validators[path.name.removesuffix(".schema.json")] = Draft202012Validator(schema)
    environment = {k: v for k, v in os.environ.items() if not k.startswith("FILETWIN_")}
    with tempfile.TemporaryDirectory(prefix="filetwin-smoke-") as directory:
        temporary = Path(directory).resolve()
        source = temporary / "source"
        shutil.copytree(project / "examples/text", source)
        saved = source / "vectors.json"

        def invoke(*arguments, expected=0):
            completed = subprocess.run([str(executable), *map(str, arguments)], cwd=temporary,
                                       capture_output=True, text=True, env=environment, timeout=30)
            assert completed.returncode == expected, (completed.returncode, completed.stderr)
            result = json.loads(completed.stdout)
            events = [json.loads(line) for line in completed.stderr.splitlines()]
            assert events[0]["data"]["stage"] == "discovering"
            assert events[-1]["data"]["stage"] == "completed"
            assert events[-1]["data"]["counts"] == result["summary"]["counts"]
            if validators:
                validators["vector-file"].validate(result)
                for event in events:
                    assert event["type"] == "progress"
                    validators["progress"].validate(event["data"])
            return result

        first = invoke(source, "--output", saved)
        assert json.loads(saved.read_text()) == first
        assert first["summary"]["counts"]["files_ready"] == 3
        for file in first["files"]:
            content = (source / file["path"]).read_bytes()
            assert file["file_id"] == "sha256:" + hashlib.sha256(content).hexdigest()
            vector = file["vector"]
            assert len(vector) == 4096 and all(math.isfinite(v) for v in vector)
            assert abs(sum(v * v for v in vector) - 1) < 1e-5
            assert file["vector_sha256"] == hashlib.sha256(struct.pack("<4096f", *vector)).hexdigest()
        cached = invoke(source, saved, "--output", saved)
        assert cached["summary"]["counts"]["cache_hits"] == 3
        assert cached["summary"]["counts"]["vectors_encoded"] == 0
        assert cached["summary"]["counts"]["bytes_hashed"] == sum(f["bytes"] for f in first["files"])
        renamed = temporary / "renamed"
        source.rename(renamed)
        moved = invoke(renamed, renamed / saved.name)
        assert moved["summary"]["counts"]["cache_hits"] == 3
        assert [f["file_id"] for f in first["files"]] == [f["file_id"] for f in moved["files"]]
        assert not list(temporary.rglob("*.sqlite*")) and not (temporary / ".filetwin").exists()
        broken = temporary / "broken.json"
        broken.write_text('{"format":"wrong"}')
        error = subprocess.run([str(executable), str(renamed), str(broken)], capture_output=True, text=True, env=environment, timeout=30)
        assert error.returncode == 2 and not error.stdout
        fatal = json.loads(error.stderr)
        assert fatal["type"] == "error"
        if validators:
            validators["error"].validate(fatal["error"])
        record = {"platform": platform.system(), "architecture": platform.machine(),
                  "first_counts": first["summary"]["counts"], "reuse_counts": cached["summary"]["counts"],
                  "schemas_validated": len(validators), "checks": ["directory input", "inline vectors and SHA-256 IDs",
                  "atomic output", "reusable portable file", "rename reuse", "structured progress and errors", "no database"]}
    if args.record:
        args.record.parent.mkdir(parents=True, exist_ok=True)
        args.record.write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record, indent=2))


if __name__ == "__main__":
    main()
