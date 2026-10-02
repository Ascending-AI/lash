#!/usr/bin/env python3
"""Bundle an explicitly selected worker and optional SDK sources for inspection."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def package(source: Path, worker: Path, output: Path, version: str, target: str | None = None) -> Path:
    source = source.resolve()
    worker = worker.resolve()
    if not worker.is_file():
        raise ValueError(f"missing worker executable: {worker}")
    info = json.loads(subprocess.check_output([str(worker), "--version"], text=True))
    if info["debug"] or info["testing"]:
        raise ValueError("worker must be an optimized SDK build without testing")
    if info["os"] != "linux":
        raise ValueError("worker must target Linux")
    compiled_target = f"{info['os']}-{info['arch']}"
    if target is not None and target != compiled_target:
        raise ValueError(f"target {target} differs from compiled target {compiled_target}")
    target = compiled_target
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached"], cwd=source
    ).decode().split("\0")
    paths = sorted(set(path for path in paths if path))
    for name in paths:
        path = source / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"source must contain regular files: {name}")
    manifest = {
        "version": version,
        "target": target,
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
        "worker": info,
        "worker_binary": "bin/lash-vm-worker",
        "worker_sha256": digest(worker),
        "source_sha256": {name: digest(source / name) for name in paths},
        "build": "cargo build --locked --release -p lash-internal-vm-worker --bin lash-vm-worker",
    }
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"lash-sdk-worker-{version}-{target}.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(worker, arcname="bin/lash-vm-worker")
        for name in paths:
            tar.add(source / name, arcname=f"sdk/{name}", recursive=False)
        encoded = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
        info = tarfile.TarInfo("manifest.json")
        info.size = len(encoded)
        tar.addfile(info, io.BytesIO(encoded))
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest(archive)}  {archive.name}\n")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, default=Path.cwd())
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", help="optional assertion of the compiled OS and architecture")
    args = parser.parse_args()
    print(package(args.source, args.worker, args.out, args.version, args.target))


if __name__ == "__main__":
    main()
