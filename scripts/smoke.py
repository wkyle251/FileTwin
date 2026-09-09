"""Exercise a built executable using disposable copies of the sample files.

Optional --schemas requires jsonschema in the invoking Python environment.
The application and the default smoke check need no Python dependencies.
"""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
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
        validators["job-request"].validate(json.loads((project / "examples/scan-request.json").read_text()))

    responses = []
    environment = {k: v for k, v in os.environ.items() if not k.startswith("FILETWIN_")}
    with tempfile.TemporaryDirectory(prefix="filetwin-smoke-") as directory:
        temporary = Path(directory).resolve()
        source = temporary / "source"
        data = temporary / "data"
        shutil.copytree(project / "examples/text", source)

        def invoke(*command, transport="json"):
            completed = subprocess.run(
                [str(executable), "--data-dir", str(data), "--format", transport, *map(str, command)],
                capture_output=True, text=True, env=environment, timeout=30, check=True,
            )
            assert not completed.stderr, completed.stderr
            events = [json.loads(line) for line in completed.stdout.splitlines()]
            for event in events:
                if validators:
                    validators["envelope"].validate(event)
                    payload_schema = {"summary": "job-summary", "page": "result-page", "error": "error"}.get(event["type"])
                    if payload_schema:
                        validators[payload_schema].validate(event["data"])
                    if event["type"] == "accepted":
                        validators["job-request"].validate(event["data"]["resolved_request"])
            responses.extend(events)
            return events[-1]["data"]

        doctor = invoke("doctor")
        assert not data.exists(), "doctor initialized an index"
        profile = invoke("profiles", "list")["profiles"][0]
        if validators:
            validators["profile"].validate(profile)
        scanned = invoke("scan", source, "--experimental-text", "--threshold", "0.7", transport="jsonl")
        assert scanned["counts"]["files_ready"] == 3
        assert scanned["counts"]["similar_pairs"] == 1
        assert scanned["counts"]["groups"] == 1
        cached = invoke("scan", source, "--experimental-text", "--threshold", "0.7")
        assert cached["counts"]["cache_hits"] == 3
        assert cached["counts"]["bytes_read"] == 0
        assert cached["counts"]["vectors_encoded"] == 0
        shutil.rmtree(source)  # Only disposable copies owned by this smoke check.
        compared = invoke("compare", "--snapshot", scanned["snapshot_id"], "--threshold", "0.7")
        assert compared["counts"]["bytes_read"] == 0
        assert compared["counts"]["similar_pairs"] == 1
        status = invoke("status", "--job", compared["job_id"])
        assert status["status"] == "completed" and not status["owner_active"]
        for kind in ["summary", "groups", "pairs", "files", "errors"]:
            page = invoke("results", "--run", compared["run_id"], "--kind", kind)
            if kind == "groups":
                members = invoke("results", "--run", compared["run_id"], "--kind", "members", "--group", page["items"][0]["group_id"])
                assert len(members["items"]) == 2
            if kind == "files":
                assert len(page["items"]) == 3
                locations = invoke("results", "--run", compared["run_id"], "--kind", "locations", "--file", page["items"][0]["file_id"])
                assert len(locations["items"]) == 1
        for report_format in ["json", "jsonl", "csv"]:
            report = temporary / report_format
            manifest = invoke("export", "--run", compared["run_id"], "--report-format", report_format, "--report-dir", report)
            assert len(manifest["artifacts"]) == 7
            for artifact in manifest["artifacts"]:
                content = (report / artifact["file"]).read_bytes()
                assert hashlib.sha256(content).hexdigest() == artifact["sha256"]
                if report_format == "json":
                    rows = json.loads(content)
                elif report_format == "jsonl":
                    rows = [json.loads(line) for line in content.splitlines()]
                else:
                    rows = list(csv.DictReader(content.decode().splitlines()))
                assert len(rows) == artifact["records"]
        record = {
            "version": doctor["app_version"], "platform": doctor["platform"],
            "architecture": doctor["architecture"], "profile_id": profile["profile_id"],
            "scan_counts": scanned["counts"], "rescan_counts": cached["counts"],
            "compare_without_originals_counts": compared["counts"],
            "schemas_validated": len(validators), "responses": responses,
        }
    if args.record:
        args.record.parent.mkdir(parents=True, exist_ok=True)
        args.record.write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps({k: v for k, v in record.items() if k != "responses"}, indent=2))


if __name__ == "__main__":
    main()
