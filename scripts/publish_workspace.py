#!/usr/bin/env python3
"""Publish the Cargo workspace to crates.io in topological batches.

`cargo publish --workspace` can fail after partial progress when crates.io index
propagation lags behind newly uploaded workspace dependencies. This helper is
idempotent and resumable: it skips versions already visible on crates.io,
publishes crates in dependency *layers* (every crate whose workspace deps are
already visible forms a layer), publishes the crates within a layer
concurrently under a conservative cap, and waits for crates.io visibility once
per layer before starting the next.

Publish-time version injection: the working tree carries the `0.0.0-dev`
placeholder. Pass `--version <version>` and the publisher stamps the real
release version into the workspace manifests + lockfile (via
`scripts/release_version.py stamp`) before reading the graph and publishing, so
`main` never carries a released version and published crates still pin the real
one.

`--plan` prints the computed version + publish layers and exits without touching
crates.io — a safe dry run of the stamping target and layering.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import email.utils
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


USER_AGENT = "lash-release-publisher/1.0"
TRANSIENT_PUBLISH_ERRORS = (
    "no matching package named",
    "failed to select a version",
    "perhaps a crate was updated and forgotten to be re-vendored",
    "too many requests",
    "published too many new crates",
    "try again after",
    "download of config.json failed",
    "failed to download",
    "failed to update registry",
    "curl failed",
    "http2 framing layer",
    "timeout was reached",
    "connection reset",
    "could not resolve host",
)
ALREADY_PUBLISHED_ERRORS = (
    "already uploaded",
    "already exists",
    "previously uploaded",
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--publish-timeout-seconds", type=int, default=600)
    parser.add_argument("--publish-attempts", type=int, default=6)
    parser.add_argument("--retry-delay-seconds", type=int, default=15)
    parser.add_argument("--visibility-timeout-seconds", type=int, default=600)
    parser.add_argument("--visibility-delay-seconds", type=int, default=5)
    # crates.io rate-limits bursts of new crates/versions; keep in-layer
    # concurrency conservative. The transient "try again after" backoff still
    # covers any 429 that slips through.
    parser.add_argument("--layer-concurrency", type=int, default=4)
    parser.add_argument(
        "--version",
        dest="version",
        default=None,
        help="stamp this release version into the workspace before publishing",
    )
    parser.add_argument(
        "--plan",
        action="store_true",
        help="print the computed version + publish layers and exit (no crates.io calls)",
    )
    args = parser.parse_args()

    if args.version and not args.plan:
        stamp_workspace(args.version)

    packages = load_publishable_workspace_packages()

    if args.plan:
        print_plan(packages, args.version)
        return 0

    completed: set[str] = set()
    for package_id, package in packages.items():
        if crate_version_visible(package["name"], package["version"]):
            print(f"already published: {package['name']} {package['version']}")
            completed.add(package_id)

    layers = compute_layers(packages, completed)
    for index, layer in enumerate(layers, start=1):
        names = ", ".join(packages[package_id]["name"] for package_id in layer)
        print(f"\n== publish layer {index}/{len(layers)} ({len(layer)} crates): {names}")
        publish_layer(layer, packages, args)

    print(f"published or confirmed {len(packages)} workspace crates")
    return 0


def stamp_workspace(version: str) -> None:
    """Stamp the real release version into the workspace being published.

    Resolves the workspace root from cargo metadata and runs that checkout's own
    `scripts/release_version.py stamp`, so manifests + lockfile carry the real
    version regardless of where this publisher script itself lives (release.yml
    runs a pinned copy from `.release-tools`).
    """
    metadata = run_json(["cargo", "metadata", "--format-version", "1", "--no-deps"])
    workspace_root = Path(metadata["workspace_root"])
    stamp_script = workspace_root / "scripts" / "release_version.py"
    print(f"stamping workspace at {workspace_root} to version {version}")
    subprocess.run(
        [sys.executable, str(stamp_script), "stamp", version],
        check=True,
        cwd=workspace_root,
    )


def load_publishable_workspace_packages() -> dict[str, dict]:
    metadata = run_json(["cargo", "metadata", "--format-version", "1", "--locked", "--no-deps"])
    workspace_members = set(metadata["workspace_members"])
    package_by_id = {
        package["id"]: package
        for package in metadata["packages"]
        if package["id"] in workspace_members and package.get("publish") != []
    }
    package_id_by_dir = {
        Path(package["manifest_path"]).parent.resolve(): package_id
        for package_id, package in package_by_id.items()
    }

    result = {}
    for package_id, package in package_by_id.items():
        workspace_dependencies = set()
        for dependency in package.get("dependencies", []):
            # Version-less path dev-dependencies (req "*") are stripped from
            # the published package, so they never gate publish ordering —
            # counting them would deadlock the planner on self-referential
            # dev-deps (a crate enabling its own feature for integration
            # tests). Dev-dependencies that carry a version requirement
            # survive into the published manifest and must resolve on the
            # index when cargo packages the crate, so they ARE ordering edges.
            if dependency.get("kind") == "dev" and dependency.get("req", "*") == "*":
                continue
            dependency_path = dependency.get("path")
            if not dependency_path:
                continue
            dependency_id = package_id_by_dir.get(Path(dependency_path).resolve())
            if dependency_id == package_id:
                continue
            if dependency_id in package_by_id:
                workspace_dependencies.add(dependency_id)
        result[package_id] = {
            "id": package_id,
            "name": package["name"],
            "version": package["version"],
            "workspace_dependencies": workspace_dependencies,
        }
    return result


def compute_layers(
    packages: dict[str, dict], already_completed: set[str] | None = None
) -> list[list[str]]:
    """Topological publish layers ordered by dependency depth.

    Each layer is the set of not-yet-published crates whose workspace
    dependencies are all satisfied by earlier layers (plus anything in
    `already_completed`, e.g. versions already visible on crates.io — that keeps
    a resumed run's layering correct). Crates within a layer have no ordering
    constraint between them, so they publish concurrently. Layers are sorted by
    crate name for deterministic output.
    """
    completed = set(already_completed or ())
    remaining = {package_id for package_id in packages if package_id not in completed}
    layers: list[list[str]] = []
    while remaining:
        ready = sorted(
            (
                package_id
                for package_id in remaining
                if packages[package_id]["workspace_dependencies"] <= completed
            ),
            key=lambda package_id: packages[package_id]["name"],
        )
        if not ready:
            names = ", ".join(
                sorted(packages[package_id]["name"] for package_id in remaining)
            )
            raise RuntimeError(f"publish dependency cycle or unresolved publish set: {names}")
        layers.append(ready)
        completed.update(ready)
        remaining.difference_update(ready)
    validate_publish_order(
        packages,
        [package_id for layer in layers for package_id in layer],
        already_completed,
    )
    return layers


def validate_publish_order(
    packages: dict[str, dict],
    ordered_package_ids: list[str],
    already_completed: set[str] | None = None,
) -> None:
    """Reject an incomplete, duplicated, or dependency-inverted publish order."""
    completed = set(already_completed or ())
    expected = set(packages) - completed
    actual = set(ordered_package_ids)
    if len(ordered_package_ids) != len(actual) or actual != expected:
        raise RuntimeError("publish order does not contain each pending package exactly once")

    for package_id in ordered_package_ids:
        missing = packages[package_id]["workspace_dependencies"] - completed
        if missing:
            package_name = packages[package_id]["name"]
            dependency_names = ", ".join(
                sorted(packages[dependency_id]["name"] for dependency_id in missing)
            )
            raise RuntimeError(
                f"publish order places {package_name} before: {dependency_names}"
            )
        completed.add(package_id)


def print_plan(packages: dict[str, dict], version: str | None) -> None:
    version_label = version or next(iter(packages.values()))["version"] if packages else "n/a"
    layers = compute_layers(packages)
    print(f"publish plan: {len(packages)} crates, {len(layers)} layers")
    print(f"stamp version: {version_label}")
    for index, layer in enumerate(layers, start=1):
        print(f"  layer {index} ({len(layer)} crates):")
        for package_id in layer:
            package = packages[package_id]
            deps = ", ".join(
                sorted(packages[dep]["name"] for dep in package["workspace_dependencies"])
            )
            suffix = f"  <- {deps}" if deps else ""
            print(f"    {package['name']}{suffix}")


def publish_layer(layer: list[str], packages: dict[str, dict], args: argparse.Namespace) -> None:
    """Publish every crate in a layer concurrently, then confirm all visible.

    Each worker uploads its crate (with the transient-failure retry/backoff) and
    waits for its own crates.io visibility, so the layer completes only once
    every crate in it is visible — the "wait once per layer" the next layer
    depends on. Concurrency is capped so a burst of new crates does not trip
    crates.io rate limits.
    """
    max_workers = max(1, min(args.layer_concurrency, len(layer)))
    if max_workers == 1:
        for package_id in layer:
            publish_package(packages[package_id], args)
        return

    with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as executor:
        futures = {
            executor.submit(publish_package, packages[package_id], args): package_id
            for package_id in layer
        }
        errors: list[str] = []
        for future in concurrent.futures.as_completed(futures):
            try:
                future.result()
            except Exception as error:  # noqa: BLE001 - surface every crate's failure.
                errors.append(f"{packages[futures[future]]['name']}: {error}")
        if errors:
            raise RuntimeError("layer publish failed:\n  " + "\n  ".join(errors))


def publish_package(package: dict, args: argparse.Namespace) -> None:
    name = package["name"]
    version = package["version"]
    if crate_version_visible(name, version):
        print(f"already published: {name} {version}")
        return

    # --allow-dirty: publish-time version stamping rewrites the manifests in
    # the checkout on purpose, so the tree is always dirty here. The packaged
    # .cargo_vcs_info.json keeps the release tag's sha plus a dirty marker,
    # which is the honest provenance for a stamped tree.
    command = ["cargo", "publish", "-p", name, "--no-verify", "--locked", "--allow-dirty"]
    deadline = time.monotonic() + args.publish_timeout_seconds
    last_output = ""

    for attempt in range(1, args.publish_attempts + 1):
        print(f"publishing {name} {version} (attempt {attempt}/{args.publish_attempts})")
        result = subprocess.run(command, text=True, capture_output=True, check=False)
        output = result.stdout + result.stderr
        last_output = output
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)

        if result.returncode == 0:
            wait_for_crate_version(name, version, args)
            return

        normalized = output.lower()
        if any(marker in normalized for marker in ALREADY_PUBLISHED_ERRORS):
            wait_for_crate_version(name, version, args)
            return

        if not any(marker in normalized for marker in TRANSIENT_PUBLISH_ERRORS):
            raise RuntimeError(f"cargo publish failed for {name} {version}")

        if attempt == args.publish_attempts or time.monotonic() >= deadline:
            break
        delay = retry_delay_seconds(output, args.retry_delay_seconds)
        print(f"waiting {delay}s before retrying {name}")
        time.sleep(delay)

    raise RuntimeError(f"cargo publish did not complete for {name} {version}\n{last_output}")


def wait_for_crate_version(name: str, version: str, args: argparse.Namespace) -> None:
    deadline = time.monotonic() + args.visibility_timeout_seconds
    while time.monotonic() < deadline:
        if crate_version_visible(name, version):
            print(f"visible on crates.io: {name} {version}")
            return
        print(f"waiting for crates.io visibility: {name} {version}")
        time.sleep(args.visibility_delay_seconds)
    raise RuntimeError(f"timed out waiting for crates.io visibility: {name} {version}")


def retry_delay_seconds(output: str, default_delay_seconds: int) -> int:
    match = re.search(r"try again after ([^\n.]+GMT)", output, flags=re.IGNORECASE)
    if not match:
        return default_delay_seconds

    try:
        retry_at = email.utils.parsedate_to_datetime(match.group(1))
    except (TypeError, ValueError):
        return default_delay_seconds

    if retry_at.tzinfo is None:
        retry_at = retry_at.replace(tzinfo=datetime.timezone.utc)
    now = datetime.datetime.now(datetime.timezone.utc)
    seconds_until_retry = int((retry_at - now).total_seconds()) + 5
    return max(default_delay_seconds, seconds_until_retry)


def crate_version_visible(name: str, version: str) -> bool:
    encoded_name = urllib.parse.quote(name, safe="")
    encoded_version = urllib.parse.quote(version, safe="")
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{encoded_name}/{encoded_version}",
        headers={"User-Agent": USER_AGENT},
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status == 200
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        print(f"crates.io lookup failed for {name} {version}: HTTP {error.code}", file=sys.stderr)
        return False
    except OSError as error:
        print(f"crates.io lookup failed for {name} {version}: {error}", file=sys.stderr)
        return False


def run_json(command: list[str]) -> dict:
    result = subprocess.run(command, text=True, capture_output=True, check=False)
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise RuntimeError(f"command failed: {' '.join(command)}")
    return json.loads(result.stdout)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
