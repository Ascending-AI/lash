#!/usr/bin/env python3
"""The kernel crates depend on nothing else in lash (docs/kernel/design.md §9).

`lash-kernel-*`, `lash-dialect-*` and `lash-ext-*` crates depend only on each
other and on third-party crates, so the set can move to its own repository.
This check reads every workspace member's manifest and fails when one of
those crates names, in any dependency table (normal, dev, build or
target-specific), a crate of this repository outside the set.

A dependency is a crate of this repository when it is given by `path`, in the
member's manifest or in the `[workspace.dependencies]` entry it inherits.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

KERNEL_SET = re.compile(r"^lash-(kernel|dialect|ext)-")
DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def in_kernel_set(package: str, directory: str) -> bool:
    return bool(KERNEL_SET.match(package) or KERNEL_SET.match(directory))


def dependency_tables(manifest: dict) -> list[tuple[str, dict]]:
    tables = [(name, manifest.get(name, {})) for name in DEPENDENCY_TABLES]
    for target, body in manifest.get("target", {}).items():
        tables.extend(
            (f"target.{target}.{name}", body.get(name, {})) for name in DEPENDENCY_TABLES
        )
    return tables


def violations(root: Path) -> list[str]:
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    shared = workspace.get("dependencies", {})
    # Directory -> package name, for every member.
    members: dict[Path, str] = {}
    manifests: dict[Path, dict] = {}
    for member in workspace.get("members", []):
        directory = (root / member).resolve()
        manifest = tomllib.loads((directory / "Cargo.toml").read_text(encoding="utf-8"))
        members[directory] = manifest["package"]["name"]
        manifests[directory] = manifest

    found: list[str] = []
    for directory, manifest in sorted(manifests.items()):
        package = members[directory]
        if not in_kernel_set(package, directory.name):
            continue
        relative = directory.relative_to(root.resolve())
        for table, dependencies in dependency_tables(manifest):
            for key, spec in sorted(dependencies.items()):
                spec = spec if isinstance(spec, dict) else {}
                base = root
                if spec.get("workspace"):
                    spec = shared.get(key, {})
                    spec = spec if isinstance(spec, dict) else {}
                else:
                    base = directory
                if "path" not in spec:
                    continue
                target = (base / spec["path"]).resolve()
                name = members.get(target, spec.get("package", key))
                if in_kernel_set(name, target.name):
                    continue
                found.append(
                    f"{relative}/Cargo.toml: [{table}] `{key}` is the lash crate `{name}`; "
                    f"`{package}` may depend only on lash-kernel-*, lash-dialect-*, "
                    "lash-ext-* and third-party crates"
                )
    return found


def main() -> int:
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
    found = violations(root)
    for violation in found:
        print(violation, file=sys.stderr)
    if found:
        print(
            f"check-kernel-boundary: {len(found)} violation(s); see docs/kernel/design.md §9",
            file=sys.stderr,
        )
        return 1
    print("check-kernel-boundary: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
