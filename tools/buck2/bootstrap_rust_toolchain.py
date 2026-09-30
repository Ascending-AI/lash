#!/usr/bin/env python3
"""Materialize Lash's pinned Buck2 Rust toolchain without installing it.

The output is ignored local state. Buck2 declares the complete directory as a
toolchain input, so remote workers never depend on an ambient Rust install.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import urllib.request


LOCK = pathlib.Path(__file__).with_name("toolchain-lock.json")
DEFAULT_OUTPUT = pathlib.Path(__file__).with_name("toolchains") / "rust-files"
REQUIRED = (
    "bin/rustc",
    "bin/rustdoc",
    "bin/clippy-driver",
    "lib/rustlib/x86_64-unknown-linux-gnu/lib",
)


def digest(path: pathlib.Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def load_lock() -> dict:
    data = json.loads(LOCK.read_text(encoding="utf-8"))
    if data.get("schema") != 1:
        raise SystemExit(f"unsupported toolchain lock schema: {data.get('schema')!r}")
    if data.get("host") != "x86_64-unknown-linux-gnu":
        raise SystemExit(f"unsupported toolchain host: {data.get('host')!r}")
    if not data.get("components"):
        raise SystemExit("toolchain lock has no components")
    return data


def check(output: pathlib.Path, lock: dict) -> bool:
    manifest = output / ".lash-toolchain.json"
    if not manifest.is_file():
        return False
    try:
        installed = json.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False
    if installed != lock:
        return False
    return all((output / path).exists() for path in REQUIRED)


def download(component: dict, destination: pathlib.Path) -> None:
    url = component["url"]
    with urllib.request.urlopen(url) as response, destination.open("wb") as out:
        shutil.copyfileobj(response, out)
    actual = digest(destination)
    if actual != component["sha256"]:
        raise SystemExit(
            f"{component['name']} checksum mismatch: expected "
            f"{component['sha256']}, got {actual}"
        )


def extract(component: dict, archive: pathlib.Path, destination: pathlib.Path) -> None:
    prefix = component["archive_prefix"]
    members = subprocess.run(
        ["tar", "--use-compress-program=unzstd", "-tf", archive],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    if any(
        pathlib.PurePosixPath(member).is_absolute()
        or ".." in pathlib.PurePosixPath(member).parts
        for member in members
    ):
        raise SystemExit(f"{component['name']} archive contains an unsafe path")
    rooted = prefix + "/"
    if not any(member == prefix or member.startswith(rooted) for member in members):
        raise SystemExit(f"{component['name']} archive omits {prefix!r}")
    strip = len(pathlib.PurePosixPath(prefix).parts)
    subprocess.run(
        [
            "tar",
            "--use-compress-program=unzstd",
            "--wildcards",
            "-xf",
            archive,
            "--strip-components",
            str(strip),
            "-C",
            destination,
            rooted + "*",
        ],
        check=True,
    )


def install(output: pathlib.Path, lock: dict) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="lash-buck2-rust-") as raw:
        temporary = pathlib.Path(raw)
        stage = temporary / "rust-files"
        stage.mkdir()
        for component in lock["components"]:
            archive = temporary / f"{component['name']}.tar.zst"
            print(f"download {component['name']} {component['url']}", file=sys.stderr)
            download(component, archive)
            extract(component, archive, stage)
        (stage / ".lash-toolchain.json").write_text(
            json.dumps(lock, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        if not all((stage / path).exists() for path in REQUIRED):
            missing = [path for path in REQUIRED if not (stage / path).exists()]
            raise SystemExit("toolchain archives omitted required paths: " + ", ".join(missing))
        old = output.with_name(output.name + ".old")
        if old.exists():
            shutil.rmtree(old)
        if output.exists():
            output.rename(old)
        stage.rename(output)
        if old.exists():
            shutil.rmtree(old)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--output", type=pathlib.Path, default=DEFAULT_OUTPUT)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    output = args.output.resolve()
    lock = load_lock()
    if check(output, lock):
        print(f"Buck2 Rust {lock['rust_version']} toolchain is current: {output}")
        return 0
    if args.check:
        print(f"Buck2 Rust toolchain is missing or stale: {output}", file=sys.stderr)
        return 1
    install(output, lock)
    if not check(output, lock):
        raise SystemExit("installed toolchain did not pass its manifest check")
    rustc = subprocess.run(
        [output / "bin/rustc", "--version"],
        check=True,
        capture_output=True,
        text=True,
        env={"PATH": "/usr/bin:/bin"},
    ).stdout.strip()
    if not rustc.startswith(f"rustc {lock['rust_version']} "):
        raise SystemExit(f"unexpected compiler after install: {rustc}")
    print(f"installed {rustc}: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
