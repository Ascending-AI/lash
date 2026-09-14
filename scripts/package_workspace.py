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
just packaged instead of against crates.io — that is what makes a from-checkout
dry run possible for crates whose dependencies are not published yet. If that
combined run fails, the script re-packages crate by crate to attribute the
failure: a crate that cannot be packaged *only* because a path dependency is not
on crates.io yet is reported as a named exception (the publisher's layered
ordering publishes that dependency first), and any other failure is fatal.

Usage:
    python3 scripts/package_workspace.py [--version X.Y.Z] [--no-verify]
                                         [--digests digests.json]

`--version` stamps the real release version into the checkout first (the working
tree carries the `0.0.0-dev` placeholder), so packaging is exercised at exactly
the version the publisher will upload. `--digests` writes the sha256 of every
packaged `.crate` for the record.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
MISSING_PACKAGE_PATTERN = re.compile(r"no matching package named `([^`]+)` found")


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
    args = parser.parse_args()

    publish_workspace = load_publish_workspace()
    if args.version:
        publish_workspace.stamp_workspace(args.version)

    packages = publish_workspace.load_publishable_workspace_packages()
    names = sorted(package["name"] for package in packages.values())
    if not names:
        print("error: no publishable workspace crates found", file=sys.stderr)
        return 1
    print(f"dry-run packaging {len(names)} publishable crates (verify={not args.no_verify})")

    result = run_cargo_package(names, no_verify=args.no_verify)
    if result.returncode == 0:
        packaged = names
        exceptions: dict[str, str] = {}
    else:
        print(
            "combined packaging run failed; re-packaging crate by crate to attribute it",
            file=sys.stderr,
        )
        packaged, exceptions, failures = package_individually(
            names, packages, no_verify=args.no_verify
        )
        if failures:
            for name, reason in sorted(failures.items()):
                print(f"PACKAGING FAILED: {name}: {reason}", file=sys.stderr)
            return 1

    digests = crate_digests(target_directory(), packages)
    report(packaged, exceptions, digests)

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
    return 0


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


def package_individually(
    names: list[str], packages: dict[str, dict], no_verify: bool
) -> tuple[list[str], dict[str, str], dict[str, str]]:
    """Package one crate at a time and classify each crate that cannot be.

    A crate whose only problem is a workspace sibling that is not on crates.io
    yet is an expected exception from a checkout: the publisher publishes that
    sibling first. Anything else is a packaging defect and fails the release.
    """
    workspace_crate_names = {package["name"] for package in packages.values()}
    packaged: list[str] = []
    exceptions: dict[str, str] = {}
    failures: dict[str, str] = {}
    for name in names:
        result = run_cargo_package([name], no_verify=no_verify)
        if result.returncode == 0:
            packaged.append(name)
            continue
        output = result.stdout + result.stderr
        unpublished = sorted(
            {
                match
                for match in MISSING_PACKAGE_PATTERN.findall(output)
                if match in workspace_crate_names
            }
        )
        if unpublished:
            exceptions[name] = (
                "path dependencies not yet on crates.io: " + ", ".join(unpublished)
            )
        else:
            failures[name] = last_error_line(output)
    return packaged, exceptions, failures


def last_error_line(output: str) -> str:
    errors = [line.strip() for line in output.splitlines() if line.strip().startswith("error")]
    return errors[-1] if errors else "cargo package failed"


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


def report(packaged: list[str], exceptions: dict[str, str], digests: dict[str, str]) -> None:
    print(f"\npackaged {len(packaged)} crates:")
    for name in packaged:
        print(f"  {name} sha256={digests.get(name, 'missing')}")
    if exceptions:
        print(
            f"\n{len(exceptions)} crates could not be dry-packaged from this checkout; "
            "the publisher's layered ordering publishes their dependencies first:"
        )
        for name, reason in sorted(exceptions.items()):
            print(f"  {name}: {reason}")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - surface the failure and fail the job.
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
