#!/usr/bin/env python3
"""Build-time conversion only. Production FileTwin uses native ONNX inference."""
import argparse
import hashlib
from pathlib import Path

SOURCE_SHA = "9f26bd4c848cc19b73d2ae92eea6e04886f61a7b764ceb7a13aeee62e6a6db56"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--check-sha")
    args = parser.parse_args()
    if hashlib.sha256(args.source.read_bytes()).hexdigest() != SOURCE_SHA:
        raise SystemExit("Official TorchScript artifact checksum mismatch")
    import torch
    import onnx
    if torch.__version__.split("+")[0] != "2.14.0" or onnx.__version__ != "1.22.0":
        raise SystemExit("Install scripts/model-requirements.txt in a build-only Python environment")
    torch.set_num_threads(1)
    model = torch.jit.load(str(args.source), map_location="cpu").eval()
    with torch.no_grad(), torch.jit.optimized_execution(False):
        torch.onnx.export(
            model, (torch.zeros(1, 3, 320, 320),), str(args.output),
            input_names=["image"], output_names=["embedding"], opset_version=17,
            dynamo=False, external_data=False, do_constant_folding=False,
        )
    model = onnx.load(args.output)
    # Remove exporter stack traces and platform-specific package suffixes.
    # Keeping BN as graph operations avoids platform-dependent offline fusion.
    def clean_graph(graph):
        graph.doc_string = ""
        for node in graph.node:
            node.doc_string = ""
            for attr in node.attribute:
                if attr.HasField("g"):
                    clean_graph(attr.g)
                for child in attr.graphs:
                    clean_graph(child)
    clean_graph(model.graph)
    model.graph.name = "filetwin_sscd_v1"
    model.doc_string = ""
    model.producer_version = "2.14.0"
    onnx.checker.check_model(model)
    data = model.SerializeToString(deterministic=True)
    actual = hashlib.sha256(data).hexdigest()
    if args.check_sha and actual != args.check_sha:
        args.output.unlink(missing_ok=True)
        raise SystemExit(f"Conversion checksum mismatch: {actual}; no model installed")
    args.output.write_bytes(data)
    print(actual)


if __name__ == "__main__":
    main()
