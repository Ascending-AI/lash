#!/usr/bin/env python3
"""Refuse production VM execution or artifact decoding outside owned workers."""
from pathlib import Path
import importlib.util
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("vm_static", ROOT / "scripts/check-vm-static-state.py")
static = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = static
spec.loader.exec_module(static)
# These are the compiler/VM implementation and its explicitly hosted benchmark.
# They do not serve model cells, durable process bodies or process creation.
LIBRARIES = {"lashlang", "lash-typescript", "lash-vm-worker"}
# The conformance crate is a test harness: these constructors create fixed AST
# storage fixtures, and none is reachable from a model execution entry point.
TEST_HARNESSES = {"lash-conformance"}
REFERENCE_TOOLS = {"crates/lash-perf/src/string_scaling.rs"}
ENTRY = re.compile(r"(?:\b(?:lash_typescript|typescript|lashlang)::(?:parse(?:_[A-Za-z0-9_]+)?|link(?:_[A-Za-z0-9_]+)?|compile(?:_[A-Za-z0-9_]+)?)|\b(?:LinkedModule|ModuleArtifact)::(?:link|from_program|from_store_bytes)|\bVmInstance::(?:new|pristine)|\.(?:execute_program|execute_compiled|run_program|compile_program))\s*\(")
IMPORT = re.compile(r"\buse\s+(?:lash_typescript|lashlang)::[^;]*\b(?:parse(?:_[A-Za-z0-9_]+)?|link(?:_[A-Za-z0-9_]+)?|compile(?:_[A-Za-z0-9_]+)?|VmInstance)\b[^;]*;")
TEST_CFG = re.compile(r'#\s*\[\s*cfg\s*\((?:test|feature\s*=\s*"testing"|any\(\s*test\s*,\s*feature\s*=\s*"testing"\s*\))\)\s*\]')


def production(text: str) -> str:
    lines = static.strip_comments(text)
    clean = "\n".join(lines)
    # Remove an entire cfg(test/testing) item, including inline modules and impls.
    # Attributes are read from the source because string stripping hides testing.
    for match in reversed(list(TEST_CFG.finditer(text))):
        start = text[:match.start()].count("\n")
        depth = 0
        opened = False
        end = start
        for end in range(start,len(lines)):
            for char in lines[end]:
                if char == "{": depth += 1; opened = True
                elif char == "}": depth -= 1
                elif char == ";" and not opened: opened = True
            if opened and depth == 0: break
        for index in range(start,end+1): lines[index] = " " * len(lines[index])
    return "\n".join(lines)


def check(root: Path) -> list[str]:
    problems = []
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        relative = path.relative_to(root).as_posix()
        crate = path.relative_to(root / "crates").parts[0]
        if crate in LIBRARIES | TEST_HARNESSES or relative in REFERENCE_TOOLS:
            continue
        if any(part in {"testing", "tests", "lib_tests"} or part.endswith("_tests.rs") or part == "tests.rs" for part in path.parts):
            continue
        text = production(path.read_text())
        for match in [*ENTRY.finditer(text), *IMPORT.finditer(text)]:
            line = text[:match.start()].count("\n")+1
            problems.append(f"{relative}:{line}: {match.group().strip()} belongs in lash-vm-worker")
    return problems


def main() -> int:
    root = Path(sys.argv[1]).resolve() if len(sys.argv)>1 else ROOT
    problems = check(root)
    if problems:
        print("Parent VM path inventory failed:\n"+"\n".join(problems),file=sys.stderr)
        return 1
    print("Parent VM path inventory passed: model source and VM artifact entry points are worker owned")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
