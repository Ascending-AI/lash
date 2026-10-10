#!/usr/bin/env python3
"""The code-mode dialect seam stays a seam (ADR 0139).

A host selects its dialect by naming it where it constructs the RLM protocol
(`CellDialect::typescript()`), and everything that words prompts in a language
lives in that dialect's prompt adapter. This check fails when:

- the TypeScript prompt adapter's concrete type is used anywhere as more than
  a value (`TypescriptPrompts::…`, `Arc<TypescriptPrompts>`, a parameter or
  return typed as it), outside the adapter itself;
- the retired TypeScript tool-binding names come back anywhere
  (`TYPESCRIPT_TOOL_BINDING_KEY`, `required_tool_typescript_*`, the
  `typescript.tool` key, `StreamMessageKind::TypescriptCode`);
- TypeScript prompt text appears in shared code-mode production sources outside
  the TypeScript adapter.

Test code (test modules at the end of a file, `tests/`, `testing/` and
`*_tests.rs` files, and a file its parent declares as a `#[cfg(test)]`
module) is exempt: TypeScript tests select TypeScript.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

TYPESCRIPT_ADAPTER = "crates/lash-protocol-rlm/src/dialect/typescript.rs"

TEST_PATH = re.compile(r"(^|/)(tests?|testing|[a-z_]*_tests)(/|\.rs$)|_tests\.rs$")

# The adapter's concrete type used as a type or a namespace, not as a value a
# host names.
CONCRETE_TYPE_USE = re.compile(
    r"TypescriptPrompts\s*::"
    r"|<\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*TypescriptPrompts\s*>"
    r"|(?:->|(?<!:):(?!:))\s*&?\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*TypescriptPrompts\b"
    r"|\bfor\s+(?:[A-Za-z_][A-Za-z0-9_]*::)*TypescriptPrompts\b"
    r"|\bstruct\s+TypescriptPrompts\b"
)

RETIRED_BINDING_NAMES = re.compile(
    r"TYPESCRIPT_TOOL_BINDING_KEY|required_tool_typescript_|\"typescript\.tool\"|TypescriptCode\b"
)

# Shared code-mode production code: everything a second dialect runs through.
SHARED_CODE_MODE_ROOTS = (
    "crates/lash-protocol-rlm/src/",
    "crates/lash-vm-runtime/src/",
    "crates/lash-rlm-types/src/",
)
TYPESCRIPT_PROMPT_TEXT = re.compile(
    r"</?typescript>|console\.log|Promise<|HistoryItem\[\]|Promise\.race"
)

TEST_MODULE_TAIL = re.compile(r"^#\[cfg\(test\)\]\s*\n(?:#\[[^\n]*\]\s*\n)*mod\s", re.MULTILINE)

# `#[cfg(test)] mod name;`: the module's file is test code.
TEST_MODULE_DECLARATION = re.compile(
    r"^\s*#\[cfg\(test\)\]\s*\n(?:\s*#\[[^\n]*\]\s*\n)*\s*mod\s+([a-z_][a-z0-9_]*)\s*;",
    re.MULTILINE,
)


def tracked_files(root: Path) -> list[str]:
    output = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return [path for path in output.split("\0") if path]


def production_text(text: str) -> str:
    """The file up to its trailing test module, where one closes the file."""
    match = TEST_MODULE_TAIL.search(text)
    return text[: match.start()] if match else text


def test_module_files(root: Path, paths: list[str]) -> set[str]:
    """The files that a parent module declares under `#[cfg(test)]`."""
    files: set[str] = set()
    for path in paths:
        if not path.endswith(".rs"):
            continue
        try:
            text = (root / path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        parent = Path(path).parent
        stem = Path(path).stem
        children = parent if stem in ("mod", "lib", "main") else parent / stem
        for name in TEST_MODULE_DECLARATION.findall(text):
            files.add(str(children / f"{name}.rs"))
            files.add(str(children / name / "mod.rs"))
    return files


def violations(root: Path) -> list[str]:
    found: list[str] = []
    paths = tracked_files(root)
    test_files = test_module_files(root, paths)
    for path in paths:
        file = root / path
        if not file.is_file():
            continue
        try:
            text = file.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue

        if not path.endswith(".rs") or TEST_PATH.search(path) or path in test_files:
            continue
        production = production_text(text)
        for number, line in enumerate(production.splitlines(), start=1):
            if RETIRED_BINDING_NAMES.search(line):
                found.append(f"{path}:{number}: retired TypeScript binding name: {line.strip()}")
            if path == TYPESCRIPT_ADAPTER:
                continue
            if CONCRETE_TYPE_USE.search(line):
                found.append(
                    f"{path}:{number}: `TypescriptPrompts` may only be named as a value a host "
                    f"selects: {line.strip()}"
                )
            if path.startswith(SHARED_CODE_MODE_ROOTS) and TYPESCRIPT_PROMPT_TEXT.search(line):
                found.append(
                    f"{path}:{number}: TypeScript prompt text belongs in {TYPESCRIPT_ADAPTER}: "
                    f"{line.strip()}"
                )
    return found


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    found = violations(root)
    for violation in found:
        print(violation, file=sys.stderr)
    if found:
        print(
            f"check-dialect-boundary: {len(found)} violation(s); see ADR 0139",
            file=sys.stderr,
        )
        return 1
    print("check-dialect-boundary: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
