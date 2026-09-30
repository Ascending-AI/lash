#!/usr/bin/env python3
"""Bundle the worker and the exact source tree its SDK client must build from."""
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
    identity = subprocess.check_output([str(worker), "--build-identity"], text=True).strip()
    if not identity.startswith("lash-worker/") or "/debug-false/testing-false" not in identity:
        raise ValueError(f"worker must be an optimized SDK build without testing: {identity}")
    parts = identity.split("/")
    if len(parts) != 6:
        raise ValueError(f"malformed worker identity: {identity}")
    compiled_target = f"{parts[3]}-{parts[2]}"
    if target is not None and target != compiled_target:
        raise ValueError(f"target {target} differs from compiled target {compiled_target}")
    target = compiled_target
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached"], cwd=source
    ).decode().split("\0")
    paths = sorted(set(path for path in paths if path))
    required = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "crates/lash-vm-client/build.rs", "crates/lash-vm-worker/build/fingerprint.rs"}
    if not required.issubset(paths):
        raise ValueError("SDK source tree lacks worker identity inputs")
    for name in paths:
        path = source / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"source must contain regular files: {name}")
    manifest = {
        "version": version,
        "target": target,
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
        "build_identity": identity,
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
