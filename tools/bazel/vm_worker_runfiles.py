"""Runtime helper requirements of the emitted first-party dependency graph."""

from __future__ import annotations

import ast
import json
import pathlib
from dataclasses import dataclass


SPAWNER_PACKAGE = "lash-internal-vm-client"
WORKER_LABEL = "//crates/lash-vm-worker:lash-vm-worker__bin"
WORKER_ENV = f"$(rootpath {WORKER_LABEL})"
TEST_MACROS = {"lash_rust_unit_test", "lash_rust_integration_test", "lash_rust_feature_test"}
LIBRARY_MACROS = {"lash_rust_library", "lash_rust_feature_library"}
BINARY_MACROS = {"lash_rust_binary", "lash_rust_feature_binary"}


@dataclass
class Target:
    path: pathlib.Path
    call: ast.Call
    macro: str
    directory: str
    args: dict[str, ast.expr]

    def value(self, name: str, default=None):
        return ast.literal_eval(self.args[name]) if name in self.args else default

    def runfiles(self) -> set[str]:
        return {
            Graph.absolute(label, self.directory)
            for name in ("extra_data", "extra_compile_data")
            for label in self.value(name, [])
        }


class Graph:
    """Mirror the wrappers' normal/dev edges, library links and variant swaps.

    Cargo's resolved package IDs handle renames and inactive optional edges.
    A variant adds and replaces labels exactly as lash_rust.bzl does. Build
    script edges and runtime data are not linked spawner dependencies.
    """

    def __init__(self, metadata: dict, outputs: dict[pathlib.Path, str], root: pathlib.Path):
        members = set(metadata["workspace_members"])
        self.directories = {
            package["id"]: pathlib.Path(package["manifest_path"]).parent.relative_to(root).as_posix()
            for package in metadata["packages"] if package["id"] in members
        }
        self.spawners = {
            self.directories[package["id"]]
            for package in metadata["packages"]
            if package["id"] in members and package["name"] == SPAWNER_PACKAGE
        }
        self.resolved = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        self.targets = {}
        self.libraries = {}
        for path, content in outputs.items():
            if path.name != "BUILD.bazel":
                continue
            directory = path.parent.relative_to(root).as_posix()
            for node in ast.parse(content).body:
                if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                        and isinstance(node.value.func, ast.Name)):
                    continue
                call = node.value
                macro = call.func.id
                if macro not in TEST_MACROS | LIBRARY_MACROS | BINARY_MACROS | {"lash_batch_test"}:
                    continue
                target = Target(path, call, macro, directory, {arg.arg: arg.value for arg in call.keywords})
                label = f"//{directory}:{target.value('name')}"
                self.targets[label] = target
                if macro == "lash_rust_library":
                    self.libraries[directory] = label
        self.edges = {}
        by_directory = {directory: key for key, directory in self.directories.items()}
        for label, target in self.targets.items():
            if target.macro == "lash_batch_test":
                self.edges[label] = {self.absolute(member, target.directory) for member in target.value("tests", [])}
                continue
            with_dev = target.macro in TEST_MACROS or target.value("include_dev_deps", False)
            allowed = {None, "dev"} if with_dev else {None}
            dependencies = set()
            package_id = by_directory[target.directory]
            swaps = target.value("variant_deps", {})
            for dependency in self.resolved.get(package_id, {}).get("deps", []):
                if not any(kind["kind"] in allowed for kind in dependency["dep_kinds"]):
                    continue
                directory = self.directories.get(dependency["pkg"])
                if directory in self.libraries:
                    dependencies.add(swaps.get(f"//{directory}", self.libraries[directory]))
            dependencies.update(target.value("extra_deps", {}))
            if target.value("library"):
                dependencies.add(self.absolute(target.value("library"), target.directory))
            self.edges[label] = dependencies

    @staticmethod
    def absolute(label: str, directory: str) -> str:
        return f"//{directory}{label}" if label.startswith(":") else label

    def requires(self, label: str) -> bool:
        pending = [label]
        visited = set()
        while pending:
            current = pending.pop()
            if current in visited:
                continue
            visited.add(current)
            target = self.targets.get(current)
            if target and target.directory in self.spawners:
                return True
            pending.extend(self.edges.get(current, ()))
        return False

    def test_packages(self) -> set[str]:
        directories = {
            target.directory for label, target in self.targets.items()
            if target.macro in TEST_MACROS and self.requires(label)
        }
        return {package_id for package_id, directory in self.directories.items() if directory in directories}


def add(metadata: dict, outputs: dict[pathlib.Path, str], root: pathlib.Path) -> None:
    """Attach support to every emitted test that links the spawner."""
    graph = Graph(metadata, outputs, root)
    edits = {}
    for label, target in graph.targets.items():
        if target.macro not in TEST_MACROS or not graph.requires(label):
            continue
        data = target.value("extra_data", [])
        if WORKER_LABEL not in target.runfiles():
            data.append(WORKER_LABEL)
        env = target.value("test_env", {}) | {"LASH_VM_WORKER": WORKER_ENV}
        for name, value in (("extra_data", data), ("test_env", env)):
            if name == "extra_data" and not value:
                continue
            replacement = (
                f"    {name} = [\n"
                + "".join(f"        {json.dumps(item)},\n" for item in value)
                + "    ],\n"
                if isinstance(value, list)
                else f"    {name} = {json.dumps(value, sort_keys=True)},\n"
            )
            if name in target.args:
                node = target.args[name]
                edit = (node.lineno - 1, node.end_lineno, replacement)
            else:
                line = target.call.end_lineno - 1
                if name == "extra_data" and "extra_compile_data" in target.args:
                    line = target.args["extra_compile_data"].end_lineno
                elif name == "test_env":
                    anchor = (
                        "tags" if "srcs_patterns" in target.args
                        else "library" if "library" in target.args
                        else "tags"
                    )
                    if anchor in target.args:
                        line = target.args[anchor].lineno - 1
                edit = (line, line, replacement)
            edits.setdefault(target.path, []).append(edit)
    for path, changes in edits.items():
        lines = outputs[path].splitlines(keepends=True)
        for start, end, replacement in sorted(changes, reverse=True):
            lines[start:end] = [replacement]
        outputs[path] = "".join(lines)


def check(metadata: dict, outputs: dict[pathlib.Path, str], root: pathlib.Path) -> list[str]:
    """Reject missing helper files, unresolved paths and batched helper users."""
    graph = Graph(metadata, outputs, root)
    failures = []
    for label, target in sorted(graph.targets.items()):
        if not graph.requires(label):
            continue
        if target.macro == "lash_batch_test":
            failures.append(f"{label}: worker-dependent tests cannot be batch members because their environment would be lost")
        elif target.macro in TEST_MACROS:
            if WORKER_LABEL not in target.runfiles():
                failures.append(f"{label}: missing VM worker runfile {WORKER_LABEL}")
            if target.value("test_env", {}).get("LASH_VM_WORKER") != WORKER_ENV:
                failures.append(f"{label}: LASH_VM_WORKER must resolve the VM worker runfile")
    return failures
