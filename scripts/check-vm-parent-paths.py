#!/usr/bin/env python3
"""Model code runs and lowers only in owned workers (ADR 0123).

A host never starts or imports a kernel run, and never parses or lowers model
source, in its own process: the worker does both, and the parent drives it
over the worker protocol. This check refuses those entry points in the
production code of every crate outside the worker and the kernel set:
`KernelMachine::start`/`import` (or through the `Machine` trait), and a
dialect's `parse`, `lower`, `lower_*` and `Parser`. Test items and test files
are exempt.
"""
from pathlib import Path
import importlib.util
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("vm_static", ROOT / "scripts/check-vm-static-state.py")
static = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = static
spec.loader.exec_module(static)
# The worker hosts runs and lowering; the kernel set is the machine and the
# dialects themselves.
WORKER = "lash-vm-worker"
KERNEL_SET = re.compile(r"^lash-(kernel|dialect|ext)-")
DIALECT_ENTRY = r"(?:lower(?:_[a-z_]+)?|parse|Parser)"
ENTRY = re.compile(
    rf"\b(?:KernelMachine|Machine)::(?:start|import)\s*\(|\blash_dialect_[a-z]+::{DIALECT_ENTRY}\b"
)
IMPORT = re.compile(
    rf"\buse\s+lash_dialect_[a-z]+::[^;]*\b{DIALECT_ENTRY}\b[^;]*;"
    r"|\buse\s+lash_kernel_vm::[^;]*\bKernelMachine\b[^;]*;"
)
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
        if crate == WORKER or KERNEL_SET.match(crate):
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
    print("Parent VM path inventory passed: kernel runs and model-source lowering are worker owned")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
