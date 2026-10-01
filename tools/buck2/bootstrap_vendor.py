#!/usr/bin/env python3
"""Materialize Reindeer's ignored, locked source tree without changing Cargo policy."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import subprocess


ROOT = pathlib.Path(__file__).resolve().parents[2]
HERE = ROOT / "tools/buck2"
VENDOR = ROOT / "vendor"
RECEIPT = VENDOR / ".lash-buck2-vendor.json"


def digest(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    value.update(path.read_bytes())
    return value.hexdigest()


def identity() -> dict:
    inputs = [
        ROOT / "third-party/Cargo.toml",
        ROOT / "third-party/Cargo.lock",
        HERE / "reindeer.toml",
        HERE / "reindeer-lock.json",
    ] + sorted((HERE / "fixups").rglob("*"))
    return {
        "schema": 1,
        "inputs": {
            path.relative_to(ROOT).as_posix(): digest(path)
            for path in inputs
            if path.is_file()
        },
    }


def expected_directories() -> set[str]:
    import tomllib

    lock = tomllib.loads((ROOT / "third-party/Cargo.lock").read_text(encoding="utf-8"))
    return {
        f"{package['name']}-{package['version']}"
        for package in lock["package"]
        if package["name"] != "lash-buck2-third-party"
    }


def current(wanted: dict) -> bool:
    if VENDOR.is_symlink() or not RECEIPT.is_file():
        return False
    try:
        actual = json.loads(RECEIPT.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False
    return actual == wanted and expected_directories() <= {
        path.name for path in VENDOR.iterdir() if path.is_dir()
    }


def install(wanted: dict) -> None:
    cargo = os.environ.get("KILN_REAL_CARGO")
    if not cargo:
        raise SystemExit("source ./env.sh first (KILN_REAL_CARGO is unset)")
    if VENDOR.is_symlink():
        raise SystemExit(f"refusing symlinked vendor directory: {VENDOR}")
    # `cargo vendor` prints a possible source-replacement stanza to stdout; it
    # does not need or write repository Cargo configuration. Discard that
    # suggestion so vendoring cannot change a user's Cargo source policy.
    subprocess.run(
        [
            cargo,
            "vendor",
            "--locked",
            "--versioned-dirs",
            "--manifest-path",
            str(ROOT / "third-party/Cargo.toml"),
            str(VENDOR),
        ],
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
    )
    VENDOR.mkdir(exist_ok=True)
    RECEIPT.write_text(json.dumps(wanted, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    wanted = identity()
    if current(wanted):
        print(f"Buck2 vendor tree is current: {VENDOR}")
        return 0
    if args.check:
        print(f"Buck2 vendor tree is missing or stale: {VENDOR}")
        return 1
    install(wanted)
    if not current(wanted):
        raise SystemExit("vendored source tree failed its receipt check")
    print(f"installed Buck2 vendor tree: {VENDOR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
