#!/usr/bin/env python3
"""Generate worker notices from Cargo.lock, enforcing deny.toml's license policy."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

WORKER_MANIFEST = "crates/lash-vm-worker/Cargo.toml"
WORKER_FEATURES = "dialect-typescript,dialect-python"
LICENSE_NAMES = ("LICENSE", "LICENCE", "COPYING", "UNLICENSE", "COPYRIGHT")


def license_paths(directory: Path, *, include_tests: bool = False) -> list[Path]:
    return sorted(path for path in directory.rglob("*")
                  if path.is_file() and path.name.upper().startswith(LICENSE_NAMES + ("NOTICE",))
                  and not any(part in {".git", "target", "node_modules"} for part in path.relative_to(directory).parts)
                  and (include_tests or not any(part in {"tests", "benches", "examples"}
                                               for part in path.relative_to(directory).parts)))


def original_files(source: Path, package: dict) -> list[tuple[str, str]]:
    directory = Path(package["manifest_path"]).parent
    paths = license_paths(directory)
    if package["source"] is None and not any(path.name.upper().startswith(LICENSE_NAMES) for path in paths):
        manifest = tomllib.loads(Path(package["manifest_path"]).read_text())
        if manifest["package"].get("license") == {"workspace": True}:
            paths.append(source / "LICENSE")
    if package.get("license_file"):
        paths = sorted(set(paths + [directory / package["license_file"]]))
    return [(str(path.relative_to(directory)) if path.is_relative_to(directory) else path.name,
             path.read_text()) for path in paths]


def about_config(source: Path) -> str:
    policy = tomllib.loads((source / "deny.toml").read_text())["licenses"]
    lines = [
        "accepted = " + json.dumps(policy["allow"]),
        "ignore-build-dependencies = false",
        "ignore-dev-dependencies = false",
    ]
    for exception in policy.get("exceptions", []):
        name = exception["crate"]
        if any(char in name for char in "@<>=*"):
            raise ValueError(f"cargo-about needs an exact crate name for exception {name}")
        lines += [f"[{json.dumps(name)}]", "accepted = " + json.dumps(exception["allow"])]
    return "\n".join(lines) + "\n"


def license_catalog(source: Path, target: str) -> dict:
    tool = subprocess.check_output(
        [sys.executable, str(source / "tools/cargo-about/bootstrap.py")], text=True
    ).strip()
    with tempfile.TemporaryDirectory(prefix="lash-notices-") as directory:
        config = Path(directory) / "about.toml"
        config.write_text(about_config(source))
        policy = tomllib.loads((source / "deny.toml").read_text())["licenses"]
        data = json.loads(subprocess.check_output([
            tool, "generate", "--locked", "--fail", "--format", "json",
            "--threshold", str(policy["confidence-threshold"]),
            "--manifest-path", str(source / "Cargo.toml"), "--workspace", "--all-features",
            "--target", target, "--config", str(config),
        ], cwd=source, text=True))
    return data


def generate_notices(source: Path, target: str, *, features: str = WORKER_FEATURES) -> bytes:
    source = source.resolve()
    tree = subprocess.check_output([
        "cargo", "tree", "--locked", "--manifest-path", str(source / WORKER_MANIFEST),
        "--no-default-features", "--features", features, "--target", target,
        "--edges", "normal", "--prefix", "none", "--format", "{p}",
    ], cwd=source, text=True)
    components = {(row.split()[0], row.split()[1].removeprefix("v")) for row in tree.splitlines()}
    data = license_catalog(source, target)
    # Gather license records broadly, then select Cargo's actual normal closure.
    # cargo-about's graph filtering can lose feature edges used by cargo tree.
    crates = [item for item in data["crates"]
              if (item["package"]["name"], item["package"]["version"]) in components]
    for license in data["licenses"]:
        license["used_by"] = [item for item in license["used_by"]
                              if (item["crate"]["name"], item["crate"]["version"]) in components]
    data["licenses"] = [license for license in data["licenses"] if license["used_by"]]
    actual = {(item["package"]["name"], item["package"]["version"]) for item in crates}
    if actual != components:
        raise ValueError(f"notice closure differs from cargo tree: missing {components - actual}; extra {actual - components}")
    if any(not license["text"].strip() for license in data["licenses"]):
        raise ValueError("a selected license has no text")
    covered = {item["crate"]["id"] for license in data["licenses"] for item in license["used_by"]}
    if not crates or covered != {item["package"]["id"] for item in crates}:
        raise ValueError("not every dependency has a license text")
    external = sum(item["package"]["source"] is not None for item in crates)
    lines = [
        "THIRD-PARTY-NOTICES",
        f"lash-vm-worker; target {target}; features {features}",
        f"Locked normal dependency closure: {len(crates)} crates ({external} registry/git crates).",
        "Generated by cargo-about; accepted licenses come from deny.toml.",
        "", "Components:",
    ]
    for item in crates:
        package = item["package"]
        lines.append(f"{package['name']} {package['version']}: {item['license']}")
        lines.append("  Authors: " + "; ".join(package["authors"]))
        if package.get("repository"):
            lines.append("  Source: " + package["repository"])
    for license in data["licenses"]:
        lines += ["", "=" * 72, license["id"], "Used by:"]
        lines += [f"  {item['crate']['name']} {item['crate']['version']}" for item in license["used_by"]]
        lines += ["", license["text"].strip()]
    for item in crates:
        package = item["package"]
        originals = original_files(source, package)
        if originals:
            lines += ["", "=" * 72, f"Original notices: {package['name']} {package['version']}"]
        for name, text in originals:
            lines += ["", name, text.strip()]
    return ("\n".join(lines) + "\n").encode()


def generate_published_notices(source: Path, target: str) -> list[Path]:
    """Crate dependencies ship by reference; only bundled sources need notices."""
    data = license_catalog(source.resolve(), target)
    outputs = []
    for item in data["crates"]:
        package = item["package"]
        if package["source"] is not None or package.get("publish") == []:
            continue
        directory = Path(package["manifest_path"]).parent
        originals = license_paths(directory, include_tests=True)
        if not originals:
            continue
        lines = [f"Bundled source notices for {package['name']}",
                 "Generated from the license files included in this crate.",
                 "Dependencies are distributed separately by Cargo."]
        for path in originals:
            text = path.read_text().strip()
            if not text:
                raise ValueError(f"missing license text in {path}")
            lines += ["", "=" * 72, str(path.relative_to(directory)), text]
        output = directory / "THIRD-PARTY-NOTICES"
        output.write_text("\n".join(lines) + "\n")
        outputs.append(output)
    return outputs


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path.cwd())
    parser.add_argument("--target", default="x86_64-unknown-linux-gnu")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--published", action="store_true", help="also generate notices for bundled sources in published crates")
    args = parser.parse_args()
    if not args.out and not args.published:
        parser.error("supply --out for worker notices or --published for crate notices")
    if args.out:
        args.out.write_bytes(generate_notices(args.source, args.target))
        print(f"Generated {args.out}")
    if args.published:
        for output in generate_published_notices(args.source, args.target):
            print(f"Generated {output}")


if __name__ == "__main__":
    main()
