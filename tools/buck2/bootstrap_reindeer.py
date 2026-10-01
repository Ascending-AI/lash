#!/usr/bin/env python3
"""Fetch the repository-pinned Reindeer binary into ignored local state."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import tempfile
import urllib.request


LOCK = pathlib.Path(__file__).with_name("reindeer-lock.json")
DEFAULT_OUTPUT = pathlib.Path(__file__).with_name("bin") / "reindeer"


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_lock() -> dict:
    lock = json.loads(LOCK.read_text(encoding="utf-8"))
    if lock.get("schema") != 1 or "asset" not in lock:
        raise SystemExit("unsupported Reindeer lock")
    return lock


def current(output: pathlib.Path, lock: dict) -> bool:
    digest_file = output.with_suffix(".sha256")
    return (
        output.is_file()
        and digest_file.is_file()
        and digest_file.read_text(encoding="ascii").strip()
        == lock["asset"]["executable_sha256"]
        and sha256(output) == digest_file.read_text(encoding="ascii").strip()
    )


def install(output: pathlib.Path, lock: dict) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    asset = lock["asset"]
    with tempfile.TemporaryDirectory(prefix="lash-reindeer-") as raw:
        temporary = pathlib.Path(raw)
        archive = temporary / "reindeer.zst"
        with urllib.request.urlopen(asset["url"]) as response, archive.open("wb") as out:
            shutil.copyfileobj(response, out)
        if archive.stat().st_size != asset["compressed_size"]:
            raise SystemExit("Reindeer asset size does not match the lock")
        actual = sha256(archive)
        if actual != asset["sha256"]:
            raise SystemExit(
                f"Reindeer checksum mismatch: expected {asset['sha256']}, got {actual}"
            )
        staged = temporary / "reindeer"
        with staged.open("wb") as out:
            subprocess.run(["unzstd", "-c", archive], check=True, stdout=out)
        staged.chmod(0o755)
        staged.replace(output)
        output.with_suffix(".sha256").write_text(sha256(output) + "\n", encoding="ascii")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--output", type=pathlib.Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()
    output = args.output.resolve()
    lock = load_lock()
    if current(output, lock):
        print(f"Reindeer {lock['version']} is current: {output}")
        return 0
    if args.check:
        print(f"Reindeer is missing or stale: {output}")
        return 1
    install(output, lock)
    if not current(output, lock):
        raise SystemExit("installed Reindeer did not pass its lock check")
    print(f"installed Reindeer {lock['version']}: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
