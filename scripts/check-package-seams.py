#!/usr/bin/env python3
"""L-SEAMS: verify the enabled production package graph, including target edges.

Run through `kiln gate lash <fork> -- python3 scripts/check-package-seams.py`.
Cargo resolves all supported features without filtering a target platform, so
optional and target-specific dependencies participate. Development edges are
reported separately and never justify a production exception. Renamed dependency
keys are resolved by package ID, rather than mistaken for package identities.
"""
from __future__ import annotations

import argparse
from collections import deque
import json
from pathlib import Path
import subprocess
import sys
import unittest


def package_name(package: dict) -> str:
    return package["name"].replace("lash-internal-", "lash-", 1)


def path_to(nodes: dict, start: str, forbidden: set[str], development: bool) -> list[str] | None:
    queue = deque([(start, [start])])
    seen = {start}
    while queue:
        current, path = queue.popleft()
        if current != start and current in forbidden:
            return path
        for dependency in nodes[current]["deps"]:
            kinds = dependency["dep_kinds"]
            if not development and not any(kind["kind"] != "dev" for kind in kinds):
                continue
            target = dependency["pkg"]
            if target not in seen:
                seen.add(target)
                queue.append((target, [*path, target]))
    return None


def check(metadata: dict) -> tuple[list[str], list[str]]:
    packages = {package["id"]: package for package in metadata["packages"]}
    names = {identity: package_name(package) for identity, package in packages.items()}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    runtime = {identity for identity, name in names.items() if name.startswith(("lash-kernel-", "lash-vm-", "lash-dialect-"))}
    protocols = {identity for identity, name in names.items() if name in {"lash-protocol-standard", "lash-protocol-rlm"}}
    failures, development = [], []
    roots = {"lash-core", "lash-protocol-standard"}
    found = set()
    for identity, name in names.items():
        if name in roots:
            found.add(name)
            forbidden = runtime
        elif name.startswith("lash-plugin-"):
            forbidden = protocols
        else:
            continue
        production = path_to(nodes, identity, forbidden, False)
        render = lambda path: " -> ".join(names[part] for part in path)
        if production:
            failures.append(f"L-SEAMS: {render(production)}")
        else:
            dev = path_to(nodes, identity, forbidden, True)
            if dev:
                development.append(f"development only: {render(dev)}")
    for missing in sorted(roots - found):
        failures.append(f"L-SEAMS: missing graph root {missing}")
    return failures, development


class SeamsLaw(unittest.TestCase):
    """L-SEAMS traverses resolved IDs and enabled production edges."""

    def test_shared_optional_target_edge_is_production_and_dev_is_separate(self):
        names = ["lash-internal-core", "lash-internal-protocol-standard", "shared", "lash-internal-vm-runtime", "lash-internal-plugin-example"]
        packages = [{"id": str(index), "name": name, "manifest_path": f"/repo/crates/{name}/Cargo.toml"} for index, name in enumerate(names)]
        def edge(target, kind=None, platform=None):
            return {"name": "renamed_dependency", "pkg": str(target), "dep_kinds": [{"kind": kind, "target": platform}]}
        nodes = [{"id": str(index), "deps": []} for index in range(len(names))]
        nodes[0]["deps"] = [edge(2)]
        nodes[2]["deps"] = [edge(3, platform='cfg(unix)')]
        nodes[4]["deps"] = [edge(3)]  # plugins may depend on runtime
        metadata = {"packages": packages, "resolve": {"nodes": nodes}}
        failures, dev = check(metadata)
        self.assertEqual(failures, ["L-SEAMS: lash-core -> shared -> lash-vm-runtime"])
        self.assertEqual(dev, [])
        nodes[2]["deps"] = [edge(3, "dev")]
        failures, dev = check(metadata)
        self.assertEqual(failures, [])
        self.assertEqual(dev, ["development only: lash-core -> shared -> lash-vm-runtime"])
        nodes[4]["deps"].append(edge(1))
        failures, _ = check(metadata)
        self.assertEqual(failures, ["L-SEAMS: lash-plugin-example -> lash-protocol-standard"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", type=Path, help="existing full Cargo metadata graph")
    parser.add_argument("--self-test", action="store_true")
    options = parser.parse_args()
    if options.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(SeamsLaw))
        return 0 if result.wasSuccessful() else 1
    if options.metadata:
        metadata = json.loads(options.metadata.read_text())
    else:
        result = subprocess.run(["cargo", "metadata", "--format-version", "1", "--all-features", "--locked"], check=True, capture_output=True, text=True)
        metadata = json.loads(result.stdout)
    failures, development = check(metadata)
    for line in [*failures, *development]:
        print(line)
    print(f"L-SEAMS: {'failed' if failures else 'passed'}; {len(development)} development-only paths")
    return bool(failures)


if __name__ == "__main__":
    sys.exit(main())
