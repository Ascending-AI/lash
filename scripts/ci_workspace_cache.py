#!/usr/bin/env python3
"""Keep restored Cargo outputs usable without hiding source changes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import subprocess
import sys
from typing import Any


MANIFEST_VERSION = 1


def tracked_files(root: Path) -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
    )
    return [os.fsdecode(path) for path in result.stdout.split(b"\0") if path]


def validate_relative_path(relative: str) -> PurePosixPath:
    path = PurePosixPath(relative)
    if path.is_absolute() or not path.parts or any(part in ("", ".", "..") for part in path.parts):
        raise ValueError(f"unsafe tracked path in workspace cache manifest: {relative!r}")
    return path


def safe_regular_file(root: Path, relative: str) -> Path:
    path = validate_relative_path(relative)

    candidate = root.joinpath(*path.parts)
    mode = candidate.lstat().st_mode
    if not stat.S_ISREG(mode):
        raise ValueError(f"workspace cache manifest path is not a regular file: {relative!r}")
    candidate.resolve(strict=True).relative_to(root.resolve(strict=True))
    return candidate


def content_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def workspace_digest(root: Path) -> str:
    digest = hashlib.sha256()
    for relative in tracked_files(root):
        path = safe_regular_file(root, relative)
        encoded = relative.encode("utf-8")
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        digest.update(bytes.fromhex(content_digest(path)))
    return digest.hexdigest()


def snapshot(root: Path, manifest_path: Path) -> None:
    files: dict[str, dict[str, Any]] = {}
    for relative in tracked_files(root):
        path = safe_regular_file(root, relative)
        files[relative] = {
            "sha256": content_digest(path),
            "mtime_ns": path.stat().st_mtime_ns,
        }

    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = manifest_path.with_suffix(f"{manifest_path.suffix}.tmp")
    temporary.write_text(
        json.dumps({"version": MANIFEST_VERSION, "files": files}, sort_keys=True),
        encoding="utf-8",
    )
    temporary.replace(manifest_path)


def restore(root: Path, manifest_path: Path) -> int:
    if not manifest_path.exists():
        print("workspace cache source-mtime manifest not found; leaving checkout mtimes unchanged")
        return 0

    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("version") != MANIFEST_VERSION or not isinstance(manifest.get("files"), dict):
        raise ValueError("unsupported workspace cache source-mtime manifest")

    tracked = set(tracked_files(root))
    restored = 0
    for relative, metadata in manifest["files"].items():
        validate_relative_path(relative)
        if relative not in tracked:
            continue
        if not isinstance(metadata, dict):
            raise ValueError(f"invalid workspace cache metadata for {relative!r}")
        expected_digest = metadata.get("sha256")
        mtime_ns = metadata.get("mtime_ns")
        if not isinstance(expected_digest, str) or not isinstance(mtime_ns, int) or mtime_ns < 0:
            raise ValueError(f"invalid workspace cache metadata for {relative!r}")

        path = safe_regular_file(root, relative)
        if content_digest(path) != expected_digest:
            continue
        os.utime(path, ns=(mtime_ns, mtime_ns))
        restored += 1

    print(f"restored source mtimes for {restored} content-identical tracked files")
    return restored


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("digest", "restore", "snapshot"))
    parser.add_argument("manifest", nargs="?", type=Path)
    args = parser.parse_args()

    root = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        ).stdout.strip()
    )
    if args.command == "digest":
        print(workspace_digest(root))
        return 0
    if args.manifest is None:
        parser.error(f"{args.command} requires a manifest path")
    manifest_path = args.manifest if args.manifest.is_absolute() else root / args.manifest
    if args.command == "restore":
        restore(root, manifest_path)
    else:
        snapshot(root, manifest_path)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.SubprocessError, ValueError, json.JSONDecodeError) as error:
        print(f"workspace cache metadata error: {error}", file=sys.stderr)
        sys.exit(1)
