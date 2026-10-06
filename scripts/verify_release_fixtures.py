#!/usr/bin/env python3
"""Verify a tagged release corpus. Opt-in cut preparation for FIG-4495."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

from capture_release_fixtures import (
    LEGS,
    MANIFEST_SCHEMA,
    ROOT,
    TAG_COMPONENT,
    git_env,
)


class VerificationError(ValueError):
    """The corpus is incomplete, altered, untagged, or contains service identities."""


def service_identities() -> list[bytes]:
    return [os.environ[name].encode() for name in (
        "LASH_POSTGRES_DATABASE_URL", "KILN_GATE_ID",
    ) if os.environ.get(name)]


def verify(corpus: Path, repo: Path = ROOT, *, forbidden: list[bytes] | None = None) -> list[Path]:
    """Read every declared byte and require an exact six-leg inventory."""
    forbidden = service_identities() if forbidden is None else forbidden
    try:
        manifest_path = corpus / "manifest.json"
        if manifest_path.is_symlink():
            raise VerificationError("manifest must not be a symlink")
        manifest = json.loads(manifest_path.read_bytes())
        if manifest["schema"] != MANIFEST_SCHEMA:
            raise VerificationError("unsupported manifest schema")
        tag = manifest["tag"]
        if not isinstance(tag, str) or not TAG_COMPONENT.fullmatch(tag):
            raise VerificationError("unsafe tag")
        result = subprocess.run(
            ["git", "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}"],
            cwd=repo, capture_output=True, text=True, check=False,
            env=git_env(),
        )
        if result.returncode or manifest["source_commit"] != result.stdout.strip():
            raise VerificationError("source_commit does not match the tag's commit")
        if manifest["dry_run"] is not False:
            raise VerificationError("dry_run must be false")
        legs = manifest["legs"]
        names = [leg["name"] for leg in legs]
        expected_names = {leg.name for leg in LEGS}
        if len(names) != len(set(names)) or set(names) != expected_names:
            raise VerificationError("missing leg, unknown leg, or duplicate leg")
        if {path.name for path in corpus.iterdir()} != expected_names | {"manifest.json"}:
            raise VerificationError("corpus inventory differs from manifest")
        paths = []
        for leg in legs:
            root = corpus / leg["name"]
            if root.is_symlink() or not root.is_dir() or not leg["files"]:
                raise VerificationError(f"{leg['name']}: missing or empty leg")
            declared = set()
            for record in leg["files"]:
                relative = record["path"]
                if (not isinstance(relative, str) or not relative
                    or PurePosixPath(relative).is_absolute()
                    or any(part in ("", ".", "..") for part in relative.split("/"))
                    or "\\" in relative):
                    raise VerificationError("unsafe file path")
                if relative in declared:
                    raise VerificationError("duplicate file path")
                declared.add(relative)
                path = root / relative
                if any(parent.is_symlink() for parent in (path, *path.parents) if parent != corpus.parent):
                    raise VerificationError("fixture must not traverse a symlink")
                data = path.read_bytes()
                if not data or type(record["bytes"]) is not int or len(data) != record["bytes"]:
                    raise VerificationError(f"{leg['name']}/{relative}: bytes mismatch or empty file")
                if hashlib.sha256(data).hexdigest() != record["sha256"]:
                    raise VerificationError(f"{leg['name']}/{relative}: sha256 mismatch")
                paths.append(path)
            actual = set()
            for path in root.rglob("*"):
                if path.is_symlink():
                    raise VerificationError("fixture tree contains a symlink")
                if path.is_file():
                    actual.add(path.relative_to(root).as_posix())
            if actual != declared:
                raise VerificationError(f"{leg['name']}: file inventory mismatch")
        for path in [manifest_path, *paths]:
            data = path.read_bytes()
            if any(value and value in data for value in forbidden) or re.search(
                rb"(?:postgres(?:ql)?|https?)://[^\s\"<>/]+:[^\s\"<>/]+@", data,
            ):
                raise VerificationError(f"{path.relative_to(corpus)}: contains a credential or service identity")
        return paths
    except (OSError, ValueError, KeyError, TypeError) as error:
        if isinstance(error, VerificationError):
            raise
        raise VerificationError(f"invalid corpus: {error}") from error


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("--repo", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    try:
        paths = verify(args.corpus, args.repo)
    except VerificationError as error:
        print(f"verify-release-fixtures error: {error}", file=sys.stderr)
        return 2
    print(f"verified {len(paths)} fixtures across {len(LEGS)} legs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
