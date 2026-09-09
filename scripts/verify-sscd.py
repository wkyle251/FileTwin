#!/usr/bin/env python3
"""Compare the production Rust tensor/inference path with official TorchScript."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import numpy as np
import torch


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("images", type=Path, nargs="+")
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--torchscript", type=Path, required=True)
    parser.add_argument("--harness", type=Path, default=Path("target/release/examples/parity"))
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    assert hashlib.sha256(args.torchscript.read_bytes()).hexdigest() == "9f26bd4c848cc19b73d2ae92eea6e04886f61a7b764ceb7a13aeee62e6a6db56", "Reference model checksum mismatch"
    torch.set_num_threads(1)
    model = torch.jit.load(str(args.torchscript), map_location="cpu").eval()
    suffix = "dylib" if platform.system() == "Darwin" else "so"
    vectors = []
    cases = []
    for path in args.images:
        request = {"version":1, "path":str(path.resolve()), "format":"raster", "profile_id":"",
                   "model_dir":str(args.model_dir.resolve()), "runtime":{"onnxruntime_path":str(args.model_dir.resolve()/f"runtime/libonnxruntime.{suffix}")}, "memory_bytes":2*1024**3}
        p = subprocess.run([str(args.harness.resolve())], input=json.dumps(request).encode(), capture_output=True, check=True, timeout=60)
        values = np.frombuffer(p.stdout, dtype="<f4")
        assert values.size == 3*320*320+512
        tensor = values[:3*320*320].copy().reshape(1,3,320,320)
        actual = values[3*320*320:].astype(np.float64)
        actual /= np.linalg.norm(actual)
        with torch.no_grad():
            reference = model(torch.from_numpy(tensor)).numpy()[0].astype(np.float64)
        reference /= np.linalg.norm(reference)
        case = {"case":len(cases)+1, "max_abs_component_error":float(np.max(np.abs(reference-actual))), "reference_cosine":float(reference@actual)}
        assert case["max_abs_component_error"] < 0.00002 and case["reference_cosine"] > 0.99999999, case
        cases.append(case)
        vectors.append(actual)
    report = {"cases":cases,"pair_cosines":[{"a":a+1,"b":b+1,"score":float(vectors[a]@vectors[b])} for a in range(len(vectors)) for b in range(a+1,len(vectors))]}
    print(json.dumps(report, indent=2))
    if args.report:
        args.report.write_text(json.dumps(report, indent=2)+"\n")


if __name__ == "__main__":
    main()
