#!/usr/bin/env python3
"""Require shared lane constructors in laws, including newly added test modules.

Single-pass fixtures use law_tick_lanes. Laws of the deployment schedule use
persistent deployment_tick_lanes, which deliberately retain the bounded wait.
Test ownership comes from the shared law module, paths, test attributes and
cfg(test) module edges;
there is no catalogue of laws or exempt law files.
"""

from pathlib import Path
import re
import sys
import tomllib

from check_feature_coverage import masked_rust, attached_brace_region

ROOT = Path(__file__).resolve().parents[1]
HELPERS = Path("crates/lash-conformance/src/conformance/helpers.rs")
TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*(?:cfg\s*\(\s*test\b|(?:\w+\s*::\s*)?test\b)")
TEST_MODULE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")


def violations(root: Path) -> list[str]:
    sources = {}
    manifest = root / "Cargo.toml"
    members = tomllib.loads(manifest.read_text()).get("workspace", {}).get("members", ["."]) if manifest.exists() else ["."]
    for member in members:
        for package in ([root] if member == "." else root.glob(member)):
            for tree in (package / "src", package / "tests"):
                if tree.is_dir():
                    sources.update((path, path.read_text()) for path in tree.rglob("*.rs"))
    laws = {path for path, code in sources.items()
            if "tests" in path.relative_to(root).parts
            or path.relative_to(root).is_relative_to(HELPERS.parent)
            or TEST_ATTRIBUTE.search(code)}
    test_files = set()
    test_directories = set()
    for path, code in sources.items():
        if not TEST_MODULE.search(code):
            continue
        for module in TEST_MODULE.finditer(masked_rust(code)):
            directory = path.parent if path.name in {"lib.rs", "main.rs", "mod.rs"} else path.with_suffix("")
            child = directory / module[1]
            test_files.add(child.with_suffix(".rs"))
            test_directories.add(child)
    laws.update(path for path in sources
                if path in test_files or any(parent in test_directories for parent in path.parents))
    failures = []
    for path in sorted(laws):
        if "RelayLanes" not in sources[path]:
            continue
        code = masked_rust(sources[path])
        shared = []
        if path.relative_to(root) == HELPERS:
            for function in re.finditer(r"\bfn\s+(?:law_tick_lanes|deployment_tick_lanes)\b", code):
                region = attached_brace_region(code, function.end())
                if region:
                    shared.append(region)
        names = {"RelayLanes"}
        names.update(re.findall(r"\bRelayLanes\s+as\s+(\w+)", code))
        constructor = re.compile(r"\b(?:" + "|".join(sorted(names)) + r")\s*::\s*(?:new|default)\s*\(")
        for call in constructor.finditer(code):
            if not any(start < call.start() < end for start, end in shared):
                line = code.count("\n", 0, call.start()) + 1
                failures.append(f"{path.relative_to(root)}:{line}")
    return failures


if __name__ == "__main__":
    failures = violations(ROOT)
    for failure in failures:
        print(f"{failure}: use law_tick_lanes, or deployment_tick_lanes for a persistent schedule")
    if not failures:
        print("law tick lane constructors checked")
    sys.exit(bool(failures))
