#!/usr/bin/env python3
"""Reconcile the isolated runtime OFF graph with Cargo's package-only graph."""

from __future__ import annotations

import argparse
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
LOCK = ROOT / "tools/bazel/runtime-off.Cargo.lock"
# MODULE.bazel replaces aws-lc-sys's Cargo build script with the existing
# native aws-lc target. These are exclusively that script's build helpers.


def cargo(*args: str) -> str:
    result = subprocess.run(["cargo", "--color", "never", *args],
        cwd=ROOT,
        text=True,
        capture_output=True,)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return result.stdout


def tree(manifest: Path, *, original: bool) -> tuple[set, set]:
    args = [
        "tree",
        "--manifest-path",
        str(manifest),
        "--locked",
        "--target",
        "x86_64-unknown-linux-gnu",
        "--edges",
        "normal,build",
        "--prefix",
        "depth",
        "--format",
        "{p}|{f}",
    ]
    if original:
        args += ["-p", "lash-runtime", "--no-default-features"]
    nodes, edges, stack = set(), set(), []
    for line in cargo(*args).splitlines():
        header, features = line.split("|")
        match = re.match(r"(\d+)(\S+) v(\S+)", header)
        if match is None:
            raise ValueError(f"unrecognized Cargo tree row: {line}")
        node = (
            match[2],
            match[3],
            tuple(sorted(filter(None, features.removesuffix(" (*)").split(",")))),
        )
        depth = int(match[1])
        stack = stack[:depth]
        if node[0] != "lash-runtime-off-witness":
            nodes.add(node)
            if stack and stack[-1][0] != "lash-runtime-off-witness":
                edges.add((stack[-1], node))
        stack.append(node)
    return nodes, edges


def bazel_graph(path: Path) -> tuple[set, set]:
    graph = json.loads(path.read_text())
    labels = {target["id"]: target["label"] for target in graph["targets"]}
    nodes, edges, artifacts, units = set(), set(), {}, []
    for action in graph["actions"]:
        label = labels[action["targetId"]]
        match = re.search(r"runtime_off_crates__(.*?)-(\d+\.\d+\.\d+[^/]*)//:", label)
        if match is None:
            if not (
                label.startswith("@@rules_rs++rules_rust+rules_rust//")
                or label.startswith("@@rules_rs++http_archive+rules_rust_tinyjson//")
            ):
                raise ValueError(f"OFF graph contains a foreign Rust target: {label}")
            continue
        arguments = action.get("arguments", [])
        features = tuple(
            sorted(
                argument[len('feature="') : -1]
                for argument in arguments
                if argument.startswith('feature="')
            )
        )
        node = (match[1], match[2], features)
        units.append((node, arguments))
        if label.endswith("//:_bs_"):
            continue
        nodes.add(node)
        for artifact in action.get("outputIds", []):
            artifacts[artifact] = node
    fragments = {part["id"]: part for part in graph.get("pathFragments", [])}

    def artifact_path(fragment: int) -> str:
        parts = []
        while fragment:
            part = fragments[fragment]
            parts.append(part["label"])
            fragment = part.get("parentId", 0)
        return "/".join(reversed(parts))

    paths = {
        artifact_path(artifact["pathFragmentId"]): artifacts[artifact["id"]]
        for artifact in graph.get("artifacts", [])
        if artifact["id"] in artifacts
    }
    for owner, arguments in units:
        for index, argument in enumerate(arguments):
            extern = (
                arguments[index + 1]
                if argument == "--extern"
                else argument.removeprefix("--extern=")
                if argument.startswith("--extern=")
                else None
            )
            if extern and "=" in extern:
                dependency = paths.get(extern.split("=", 1)[1])
                if dependency is None:
                    raise ValueError(
                        f"unmapped OFF Rust dependency for {owner}: {extern}"
                    )
                edges.add((owner, dependency))
    return nodes, edges


def compare(expected: set, actual: set, label: str) -> None:
    if expected != actual:
        raise ValueError(
            f"{label} differ\nmissing: {sorted(expected - actual)}\nextra: {sorted(actual - expected)}"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sync", action="store_true")
    parser.add_argument("--actions", type=Path)
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location(
        "runtime_off_workspace", ROOT / "tools/bazel/runtime_off_workspace.py"
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix="lash-runtime-off-") as temporary:
        output = Path(temporary)
        module.materialize(ROOT, output)
        manifest = output / "witness/Cargo.toml"
        if args.sync:
            manifest.with_name("Cargo.lock").write_text(
                (ROOT / "Cargo.lock").read_text()
                + '\n[[package]]\nname = "lash-runtime-off-witness"\nversion = "0.0.0"\ndependencies = ["lash-runtime"]\n'
            )
        cargo("metadata", "--manifest-path", str(manifest), "--format-version", "1")
        resolved_lock = manifest.with_name("Cargo.lock")
        if args.sync:
            LOCK.write_bytes(resolved_lock.read_bytes())
        else:
            compare(
                set(
                    json.dumps(row, sort_keys=True)
                    for row in tomllib.loads(LOCK.read_text())["package"]
                ),
                set(
                    json.dumps(row, sort_keys=True)
                    for row in tomllib.loads(resolved_lock.read_text())["package"]
                ),
                "isolated lock packages; run kiln sync",
            )
        expected, expected_edges = tree(ROOT / "Cargo.toml", original=True)
        actual, actual_edges = tree(manifest, original=False)
        compare(expected, actual, "Cargo package features")
        compare(expected_edges, actual_edges, "Cargo dependency edges")
        if args.actions:
            actual, actual_edges = bazel_graph(args.actions)
            expected_edges = {
                edge for edge in expected_edges if edge[0][0] != "aws-lc-sys"
            }
            reachable = {node for node in expected if node[0] == "lash-runtime"}
            while True:
                expanded = reachable | {
                    child for parent, child in expected_edges if parent in reachable
                }
                if expanded == reachable:
                    break
                reachable = expanded
            expected = reachable
            expected_edges = {edge for edge in expected_edges if edge[0] in reachable}
            compare(expected, actual, "Bazel package features")
            compare(expected_edges, actual_edges, "Bazel dependency edges")
        print(
            f"runtime OFF graph: {len(expected)} package-feature units and {len(expected_edges)} dependency edges match"
        )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, ValueError) as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
