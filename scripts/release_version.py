#!/usr/bin/env python3
"""Release version helper for the publish-time version-injection flow.

The working tree carries the honest ``0.0.0-dev`` placeholder in every
workspace manifest — there is no version-bump commit. At release time the
release workflow computes the next version from the release *channel* declared
in ``[workspace.metadata.release]`` plus the existing ``v*`` tags, then stamps
that version into the manifests of its ephemeral tag checkout before building
binaries and publishing crates. Nothing in ``main`` ever carries a released
version number.

Commands:
  print-next        Print the next release version for the declared channel.
  stamp <version>   Rewrite the workspace manifests + lockfile to <version>
                    (used by the release workflow's ephemeral checkout).
"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
CARGO_LOCK = ROOT / "Cargo.lock"
VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
# Pre-release versions (e.g. "0.1.0-alpha.1") and the "0.0.0-dev" working-tree
# placeholder both match this shape.
PRERELEASE_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)-[A-Za-z0-9.-]+$")


def main() -> int:
    if len(sys.argv) < 2:
        print_usage()
        return 1

    command = sys.argv[1]
    if command == "print-next":
        print(compute_next_version())
        return 0
    if command == "stamp":
        if len(sys.argv) != 3:
            print(f"{command} requires a version argument", file=sys.stderr)
            return 1
        value = sys.argv[2]
        if not (VERSION_RE.fullmatch(value) or PRERELEASE_RE.fullmatch(value)):
            print(f"unsupported version format: {value!r}", file=sys.stderr)
            return 1
        stamp_manifests(value)
        return 0

    print(f"unknown command: {command}", file=sys.stderr)
    print_usage()
    return 1


def print_usage() -> None:
    print(
        "Usage:\n"
        "  scripts/release_version.py print-next\n"
        "  scripts/release_version.py stamp <version>",
        file=sys.stderr,
    )


# --- next-version computation -------------------------------------------------


def read_release_channel() -> str:
    """The release channel declared in [workspace.metadata.release].

    The channel is the series CI advances — a pre-release series like
    ``0.1.0-alpha`` (advancing ``0.1.0-alpha.N``) or a clean ``X.Y.Z`` (advancing
    the patch). It replaces reading the version from the manifest, which now
    always holds the ``0.0.0-dev`` placeholder.
    """
    data = tomllib.loads(CARGO_TOML.read_text())
    try:
        channel = data["workspace"]["metadata"]["release"]["channel"]
    except (KeyError, TypeError):
        raise SystemExit("missing [workspace.metadata.release] channel in Cargo.toml")
    if not isinstance(channel, str) or not (
        VERSION_RE.fullmatch(channel) or PRERELEASE_RE.fullmatch(channel)
    ):
        raise SystemExit(f"unsupported release channel: {channel!r}")
    return channel


def compute_next_version() -> str:
    channel = read_release_channel()
    if PRERELEASE_RE.fullmatch(channel):
        return compute_next_prerelease_version(channel)
    return compute_next_stable_version(channel)


def compute_next_prerelease_version(channel: str) -> str:
    """Next ``<channel>.N`` for a pre-release channel like ``0.1.0-alpha``.

    N is one past the highest numeric suffix among ``v<channel>.<N>`` tags, or
    1 when the channel has no tagged releases yet.
    """
    prefix = f"{channel}."
    next_number = 1
    for tag in read_release_tags():
        value = tag.removeprefix("v")
        if not value.startswith(prefix):
            continue
        suffix = value[len(prefix):]
        if suffix.isdigit():
            next_number = max(next_number, int(suffix) + 1)
    return f"{prefix}{next_number}"


def compute_next_stable_version(channel: str) -> str:
    """Next patch for a clean ``X.Y.Z`` channel.

    The first release of a channel is the channel version itself; afterwards CI
    advances the patch past the highest tag sharing the channel's major.minor.
    """
    channel_tuple = parse_version(channel)
    highest = channel_tuple
    for tag in read_release_tags():
        value = tag.removeprefix("v")
        if not VERSION_RE.fullmatch(value):
            continue
        candidate = parse_version(value)
        if candidate[:2] == channel_tuple[:2] and candidate > highest:
            highest = candidate
    if highest == channel_tuple and not tag_exists(f"v{channel}"):
        return format_version(channel_tuple)
    return format_version((highest[0], highest[1], highest[2] + 1))


def read_release_tags() -> list[str]:
    result = subprocess.run(
        ["git", "tag", "--list", "v*", "--sort=-version:refname"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def tag_exists(tag: str) -> bool:
    return tag in set(read_release_tags())


def parse_version(value: str) -> tuple[int, int, int]:
    match = VERSION_RE.fullmatch(value)
    if not match:
        raise SystemExit(f"unsupported version format: {value!r}")
    return tuple(int(part) for part in match.groups())


def format_version(version: tuple[int, int, int]) -> str:
    return ".".join(str(part) for part in version)


# --- stamping -----------------------------------------------------------------


def stamp_manifests(version_text: str) -> None:
    """Stamp the workspace manifests + lockfile to an exact version.

    This is what the release workflow runs on its ephemeral tag checkout so the
    published crates and the built binaries carry the real release version.
    """
    update_workspace_version(version_text)
    update_workspace_dependency_versions(version_text)
    update_lockfile_versions(version_text)


def update_workspace_version(version: str) -> None:
    cargo_toml = CARGO_TOML.read_text()
    updated, count = re.subn(
        r"(?m)^version = \"[^\"]+\"$",
        f'version = "{version}"',
        cargo_toml,
        count=1,
    )
    if count != 1:
        raise SystemExit("failed to update workspace version in Cargo.toml")
    CARGO_TOML.write_text(updated)


def update_workspace_dependency_versions(version: str) -> None:
    """Stamp exact workspace pins, resolving Cargo aliases by `package =`."""
    workspace_packages = read_workspace_package_names()
    for manifest in sorted(ROOT.glob("**/Cargo.toml")):
        if "target" in manifest.relative_to(ROOT).parts:
            continue
        text = manifest.read_text()
        updated_lines = []
        for line in text.splitlines():
            stripped = line.lstrip()
            name_match = re.match(r"([A-Za-z0-9_-]+)\s*=", stripped)
            package_match = re.search(r'package\s*=\s*"([^"]+)"', stripped)
            package_name = (
                package_match.group(1)
                if package_match is not None
                else name_match.group(1)
                if name_match is not None
                else None
            )
            if (
                name_match
                and package_name in workspace_packages
                and "path =" in line
                and "version =" in line
            ):
                line = re.sub(
                    r'version = "=[^"]+"',
                    f'version = "={version}"',
                    line,
                )
            updated_lines.append(line)
        updated = "\n".join(updated_lines) + "\n"
        if updated != text:
            manifest.write_text(updated)


def update_lockfile_versions(version: str) -> None:
    workspace_packages = read_lockstep_workspace_package_names()
    lines = CARGO_LOCK.read_text().splitlines()
    current_name: str | None = None
    updated_lines: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped == "[[package]]":
            current_name = None
        elif stripped.startswith('name = "'):
            current_name = stripped[len('name = "') : -1]
        elif (
            current_name in workspace_packages
            and stripped.startswith('version = "')
        ):
            line = f'version = "{version}"'
        updated_lines.append(line)
    CARGO_LOCK.write_text("\n".join(updated_lines) + "\n")


def read_lockstep_workspace_package_names() -> set[str]:
    workspace_packages = read_workspace_package_names()
    lockstep_packages: set[str] = set()
    for manifest in sorted(ROOT.glob("**/Cargo.toml")):
        if "target" in manifest.relative_to(ROOT).parts:
            continue
        data = tomllib.loads(manifest.read_text())
        package = data.get("package")
        if not isinstance(package, dict):
            continue
        name = package.get("name")
        version = package.get("version")
        if (
            isinstance(name, str)
            and name in workspace_packages
            and isinstance(version, dict)
            and version.get("workspace") is True
        ):
            lockstep_packages.add(name)
    return lockstep_packages


def read_workspace_package_names() -> set[str]:
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
            str(CARGO_TOML),
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    metadata = json.loads(result.stdout)
    workspace_members = set(metadata["workspace_members"])
    return {
        package["name"]
        for package in metadata["packages"]
        if package["id"] in workspace_members
    }


if __name__ == "__main__":
    raise SystemExit(main())
