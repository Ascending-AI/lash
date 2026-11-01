#!/usr/bin/env python3
"""Package a stripped Linux worker, its dependency notices and deployment manifest."""
from __future__ import annotations

import argparse
import ast
import hashlib
import io
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile

from generate_notices import generate_notices

GLIBC_FLOOR = "2.34"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def worker_target(source: Path, dialects: list[str]) -> str:
    """Select the generated binary rule by its exact production feature set."""
    features = {f"dialect-{dialect}" for dialect in dialects}
    matches = []
    for node in ast.parse((source / "crates/lash-vm-worker/BUCK").read_text()).body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                and isinstance(node.value.func, ast.Name)
                and node.value.func.id == "lash_rust_feature_binary"):
            continue
        fields = {arg.arg: arg.value for arg in node.value.keywords}
        name = ast.literal_eval(fields["name"])
        if name.startswith("lash-vm-worker__bin") and set(ast.literal_eval(fields["crate_features"])) == features:
            matches.append(f"//crates/lash-vm-worker:{name}")
    if len(matches) != 1:
        raise ValueError(f"expected one worker target for {sorted(features)}, found {matches}; run kiln sync")
    return matches[0]


def check_glibc(worker: Path) -> str:
    symbols = subprocess.check_output(["readelf", "--dyn-syms", "--wide", str(worker)], text=True)
    versions = {match for match in re.findall(r"@GLIBC_([0-9]+(?:\.[0-9]+)+)\b", symbols)}
    if not versions:
        raise ValueError("worker has no versioned glibc imports")
    required = max(versions, key=lambda value: tuple(map(int, value.split("."))))
    if tuple(map(int, required.split("."))) > tuple(map(int, GLIBC_FLOOR.split("."))):
        raise ValueError(f"worker requires glibc {required}, newer than declared floor {GLIBC_FLOOR}")
    print(f"glibc check: required {required}, declared floor {GLIBC_FLOOR}: passed")
    return required


def package(source: Path, worker: Path, output: Path, version: str,
            target: str | None = None, dialects: list[str] | None = None) -> Path:
    source, worker = source.resolve(), worker.resolve()
    if not worker.is_file():
        raise ValueError(f"missing worker executable: {worker}")
    info = json.loads(subprocess.check_output([str(worker), "--version"], text=True))
    if info["debug"] or info["testing"]:
        raise ValueError("worker must be an optimized build without testing")
    if info["os"] != "linux":
        raise ValueError("worker must target Linux")
    compiled_target = f"{info['os']}-{info['arch']}"
    if target is not None and target != compiled_target:
        raise ValueError(f"target {target} differs from compiled target {compiled_target}")
    compiled_dialects = sorted(info["dialects"])
    if not compiled_dialects or len(set(compiled_dialects)) != len(compiled_dialects):
        raise ValueError("worker must name its compiled dialects exactly once")
    if dialects is not None and sorted(dialects) != compiled_dialects:
        raise ValueError(f"requested dialects {dialects} differ from compiled dialects {compiled_dialects}")
    triple = {"x86_64": "x86_64-unknown-linux-gnu", "aarch64": "aarch64-unknown-linux-gnu"}[info["arch"]]
    features = ",".join(f"dialect-{dialect}" for dialect in compiled_dialects)
    notices = generate_notices(source, triple, features=features)
    with tempfile.TemporaryDirectory(prefix="lash-worker-") as temporary:
        stripped = Path(temporary) / "lash-vm-worker"
        shutil.copy2(worker, stripped)
        subprocess.run(["strip", "--strip-all", str(stripped)], check=True)
        check_glibc(stripped)
        manifest = {
            "version": version,
            "target": compiled_target,
            "dialects": compiled_dialects,
            "protocol_version": info["protocol_version"],
            "minimum_supported_protocol_version": info["minimum_supported_protocol_version"],
            "glibc_minimum": GLIBC_FLOOR,
            "binary": {"path": "bin/lash-vm-worker", "sha256": digest(stripped)},
            "notices": {"path": "THIRD-PARTY-NOTICES", "sha256": hashlib.sha256(notices).hexdigest()},
        }
        output.mkdir(parents=True, exist_ok=True)
        archive = output / f"lash-sdk-worker-{version}-{compiled_target}-{'-'.join(compiled_dialects)}.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(stripped, arcname=manifest["binary"]["path"])
            for name, content in [(manifest["notices"]["path"], notices),
                                  ("manifest.json", (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode())]:
                entry = tarfile.TarInfo(name)
                entry.size = len(content)
                tar.addfile(entry, io.BytesIO(content))
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest(archive)}  {archive.name}\n")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path.cwd())
    parser.add_argument("--dialects", required=True, help="comma-separated compiled dialect names")
    parser.add_argument("--print-target", action="store_true", help="select the Buck worker target without packaging")
    parser.add_argument("--worker", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--version")
    parser.add_argument("--target", help="optional assertion of the compiled OS and architecture")
    args = parser.parse_args()
    dialects = args.dialects.split(",")
    if args.print_target:
        print(worker_target(args.source, dialects))
    else:
        if not all([args.worker, args.out, args.version]):
            parser.error("packaging requires --worker, --out and --version")
        print(package(args.source, args.worker, args.out, args.version, args.target, dialects))


if __name__ == "__main__":
    main()
