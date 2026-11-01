#!/usr/bin/env python3
"""Dry-run package every publishable workspace crate before a release publishes.

`cargo publish` runs with `--no-verify` in the release publisher: the release is
cut from a CI-tested `main`, and re-verifying every crate inside the publish job
would double an already long, token-bearing job. What CI never exercises is the
*packaging* of that tested code — a missing `include`, a stale manifest field, a
path dependency without a version requirement, or a publish cycle only shows up
when cargo builds the `.crate` tarballs. This script is that proof, and it is a
required job ahead of the publisher, so `--no-verify` never publishes something
that was never packaged.

Packaging is done in one `cargo package -p <crate> ...` invocation covering every
publishable crate, so cargo resolves workspace siblings against the crates it
just packaged instead of against crates.io. Any packaging failure stops the
release; no unpublished-sibling exception can hide a missing artifact.

Usage:
    python3 scripts/package_workspace.py [--version X.Y.Z] [--no-verify]
                                         [--digests digests.json] [--verify-package CRATE]

`--version` stamps the real release version into the checkout first (the working
tree carries the `0.0.0-dev` placeholder), so packaging is exercised at exactly
the version the publisher will upload. `--digests` writes the sha256 of every
packaged `.crate` for the record. `--verify-package` builds a selected library
with its dialect features using only packaged workspace sources.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent


def load_publish_workspace():
    module_path = SCRIPT_DIR / "publish_workspace.py"
    spec = importlib.util.spec_from_file_location("publish_workspace", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--version",
        default=None,
        help="stamp this release version into the workspace before packaging",
    )
    parser.add_argument(
        "--no-verify",
        action="store_true",
        help="skip the per-crate verify build (packaging itself is still proven)",
    )
    parser.add_argument(
        "--digests",
        default=None,
        help="write the sha256 of every packaged .crate to this JSON file",
    )
    parser.add_argument(
        "--verify-package", action="append", default=[], metavar="CRATE",
        help="build this standalone crate from packaged workspace dependencies, including its dialect features",
    )
    args = parser.parse_args()

    publish_workspace = load_publish_workspace()
    if args.version:
        publish_workspace.stamp_workspace(args.version)

    from generate_notices import generate_published_notices

    generate_published_notices(Path.cwd(), "x86_64-unknown-linux-gnu")
    packages = publish_workspace.load_publishable_workspace_packages()
    names = sorted(package["name"] for package in packages.values())
    if not names:
        print("error: no publishable workspace crates found", file=sys.stderr)
        return 1
    print(f"dry-run packaging {len(names)} publishable crates (verify={not args.no_verify})")

    result = run_cargo_package(names, no_verify=args.no_verify)
    if result.returncode != 0:
        return result.returncode
    packaged = names

    digests = crate_digests(target_directory(), packages)
    report(packaged, digests)

    if args.digests:
        Path(args.digests).write_text(
            json.dumps(digests, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"wrote packaged crate digests to {args.digests}")

    missing = sorted(set(packaged) - set(digests))
    if missing:
        print(
            "error: cargo reported success but produced no .crate for: "
            + ", ".join(missing),
            file=sys.stderr,
        )
        return 1
    for name in args.verify_package:
        verify_packaged_crate(name, target_directory(), packages)
    return 0


def verify_packaged_crate(name: str, target_dir: Path, packages: dict[str, dict]) -> None:
    """Use only normalized .crate contents, with no workspace path dependencies."""
    by_name = {package["name"]: package for package in packages.values()}
    if name not in by_name:
        raise ValueError(f"standalone verification requires a publishable crate: {name}")
    with tempfile.TemporaryDirectory(prefix="lash-packaged-") as directory:
        root = Path(directory)
        shutil.copyfile(Path.cwd() / "rust-toolchain.toml", root / "rust-toolchain.toml")
        config = []
        for package in packages.values():
            stem = f"{package['name']}-{package['version']}"
            with tarfile.open(target_dir / "package" / f"{stem}.crate") as archive:
                archive.extractall(root, filter="data")
            config.extend(["--config", f'patch.crates-io.{package["name"]}.path={json.dumps(str(root / stem))}'])
        package = by_name[name]
        manifest = root / f"{name}-{package['version']}" / "Cargo.toml"
        # Cargo emitted these manifests with registry dependencies. Patches
        # stand in for this release's not-yet-uploaded packages, never sources
        # from the checkout. Cargo still enforces their exact version pins.
        features = sorted(feature for feature in tomllib.loads(manifest.read_text()).get("features", {})
                          if feature.startswith("dialect-"))
        command = ["cargo", "build", "--manifest-path", str(manifest), "--lib", *config]
        if features:
            command.extend(["--features", ",".join(features)])
        subprocess.run(command, cwd=root, check=True)
        print(f"standalone packaged build passed: {name}; dialect features {features}")


def run_cargo_package(names: list[str], no_verify: bool) -> subprocess.CompletedProcess:
    # --allow-dirty: --version stamping rewrites the manifests on purpose, so the
    # checkout is dirty exactly as it is when the publisher packages it.
    command = ["cargo", "package", "--locked", "--allow-dirty"]
    if no_verify:
        command.append("--no-verify")
    for name in names:
        command.extend(["-p", name])
    result = subprocess.run(command, text=True, capture_output=True, check=False)
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    return result


def target_directory() -> Path:
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"], text=True
        )
    )
    return Path(metadata["target_directory"])


def crate_digests(target_dir: Path, packages: dict[str, dict]) -> dict[str, str]:
    digests: dict[str, str] = {}
    for package in packages.values():
        crate_path = target_dir / "package" / f"{package['name']}-{package['version']}.crate"
        if crate_path.is_file():
            digests[package["name"]] = sha256_file(crate_path)
    return digests


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def report(packaged: list[str], digests: dict[str, str]) -> None:
    print(f"\npackaged {len(packaged)} crates:")
    for name in packaged:
        print(f"  {name} sha256={digests.get(name, 'missing')}")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - surface the failure and fail the job.
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
