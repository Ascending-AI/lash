#!/usr/bin/env python3
"""Materialize the package-only runtime OFF graph without copying Rust sources."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import tomllib


def value(item: object) -> str:
    if isinstance(item, str):
        return json.dumps(item)
    if isinstance(item, bool):
        return str(item).lower()
    if isinstance(item, list):
        return "[" + ", ".join(value(entry) for entry in item) + "]"
    if isinstance(item, dict):
        return (
            "{"
            + ", ".join(
                json.dumps(key) + " = " + value(entry) for key, entry in item.items()
            )
            + "}"
        )
    if isinstance(item, int):
        return str(item)
    raise TypeError(f"unsupported Cargo value: {item!r}")


def write_manifest(path: Path, manifest: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(
            json.dumps(key) + " = " + value(item) + "\n"
            for key, item in manifest.items()
        )
    )


def materialize(root: Path, output: Path) -> list[str]:
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    if any(
        any(character in member for character in "*?[")
        for member in workspace["members"]
    ):
        raise ValueError(
            "runtime OFF source discovery requires explicit workspace members"
        )
    manifests = sorted(
        {
            path / "Cargo.toml"
            for pattern in workspace["members"]
            for path in root.glob(pattern)
        }
    )
    patches = {}
    for manifest in manifests:
        directory = manifest.parent
        relative = directory.relative_to(root)
        original = tomllib.loads(manifest.read_text())
        package = {
            key: workspace["package"][key]
            if isinstance(item, dict) and item.get("workspace")
            else item
            for key, item in original["package"].items()
            if key not in ("metadata", "readme", "include", "exclude")
        }
        # This is a library check; Cargo's auto-discovered test/bin targets do
        # not belong to the command, nor do their dev dependencies.
        package.update(autoexamples=False, autotests=False, autobenches=False)
        cooked = {"package": package}
        for key in ("lib", "bin", "features"):
            if key in original:
                cooked[key] = original[key]

        def dependencies(entries: dict) -> dict:
            result = {}
            for name, entry in entries.items():
                if isinstance(entry, str):
                    result[name] = entry
                    continue
                entry = dict(entry)
                base = directory
                if entry.pop("workspace", False):
                    inherited = workspace["dependencies"][name]
                    inherited = (
                        {"version": inherited}
                        if isinstance(inherited, str)
                        else dict(inherited)
                    )
                    inherited["features"] = inherited.get("features", []) + entry.pop(
                        "features", []
                    )
                    inherited.update(entry)
                    entry = inherited
                    base = root
                if "path" in entry:
                    dependency = (base / entry["path"]).resolve()
                    dependency.relative_to(root)
                    entry["path"] = os.path.relpath(dependency, directory)
                result[name] = entry
            return result

        for kind in ("dependencies", "build-dependencies"):
            if kind in original:
                cooked[kind] = dependencies(original[kind])
        for target, sections in original.get("target", {}).items():
            cooked.setdefault("target", {})[target] = {
                kind: dependencies(entries)
                for kind, entries in sections.items()
                if kind in ("dependencies", "build-dependencies")
            }
        target = output / relative
        write_manifest(target / "Cargo.toml", cooked)
        for entry in directory.iterdir():
            if entry.name in ("Cargo.toml", "BUILD", "BUILD.bazel"):
                continue
            (target / entry.name).symlink_to(entry)
        patches[package["name"]] = {"path": "../" + relative.as_posix()}

    # Keep dependencies outside the root package's directory. Cargo otherwise
    # promotes path dependencies to members and unifies their default features.
    witness = output / "witness"
    write_manifest(
        witness / "Cargo.toml",
        {
            "package": {
                "name": "lash-runtime-off-witness",
                "version": "0.0.0",
                "edition": workspace["package"]["edition"],
            },
            "workspace": {"resolver": workspace["resolver"]},
            "lib": {"path": "lib.rs"},
            "dependencies": {
                "lash": {
                    "package": "lash-runtime",
                    "path": "../crates/lash",
                    "default-features": False,
                }
            },
            # rules_rs needs locators for transitive path dependencies. These
            # patches locate packages; they do not activate dependencies/features.
            "patch": {"crates-io": patches},
        },
    )
    (witness / "lib.rs").write_text("pub use lash::*;\n")
    (witness / "Cargo.lock").write_text(
        (root / "tools/bazel/runtime-off.Cargo.lock").read_text()
    )
    return [str(path.relative_to(root)) for path in manifests]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    print(json.dumps(materialize(args.root.resolve(), args.output.resolve())))
