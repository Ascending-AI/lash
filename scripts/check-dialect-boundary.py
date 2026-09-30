#!/usr/bin/env python3
"""The code-mode dialect seam stays a seam (ADR 0096).

A host selects its dialect by naming it where it constructs the RLM protocol,
and everything language-specific lives in that dialect's adapter. This check
fails when:

- the TypeScript adapter's concrete type is used anywhere as more than a value
  a host names (`TypescriptDialect::…`, `Arc<TypescriptDialect>`, a parameter
  or return typed as it), outside the adapter itself;
- the retired TypeScript tool-binding names come back anywhere
  (`TYPESCRIPT_TOOL_BINDING_KEY`, `required_tool_typescript_*`, the
  `typescript.tool` key, `StreamMessageKind::TypescriptCode`);
- TypeScript prompt text appears in shared code-mode production sources outside
  the TypeScript adapter;
- the test-only seam-proof dialect leaks out of the lash integration tests. Its
  only permitted mention elsewhere is ADR 0096's evidence line citing the test
  file.

Test code (test modules at the end of a file, `tests/`, `testing/` and
`*_tests.rs` files) is exempt from the first three rules: TypeScript tests
select TypeScript.
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
    r"TypescriptDialect\s*::"
    r"|<\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*TypescriptDialect\s*>"
    r"|(?:->|(?<!:):(?!:))\s*&?\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*TypescriptDialect\b"
    r"|\bfor\s+(?:[A-Za-z_][A-Za-z0-9_]*::)*TypescriptDialect\b"
    r"|\bstruct\s+TypescriptDialect\b"
)

RETIRED_BINDING_NAMES = re.compile(
    r"TYPESCRIPT_TOOL_BINDING_KEY|required_tool_typescript_|\"typescript\.tool\"|TypescriptCode\b"
)

# Shared code-mode production code: everything a second dialect runs through.
SHARED_CODE_MODE_ROOTS = (
    "crates/lash-protocol-rlm/src/",
    "crates/lash-lashlang-runtime/src/",
    "crates/lash-rlm-types/src/",
)
TYPESCRIPT_PROMPT_TEXT = re.compile(
    r"</?typescript>|console\.log|Promise<|HistoryItem\[\]|Promise\.race"
)

SEAM_PROOF = re.compile(r"SeamProof(?:Dialect|Frontend)|(?<![\w-])seam-proof(?![\w.-])|</?seam>")
SEAM_PROOF_HOME = "crates/lash/tests/"
SEAM_PROOF_EVIDENCE_ADR = "docs/adr/0096-"
SEAM_PROOF_EVIDENCE_PATH = "crates/lash/tests/seam_proof_dialect.rs"
CHECK_FILES = (
    "scripts/check-dialect-boundary.py",
    "scripts/test_check_dialect_boundary.py",
)

TEST_MODULE_TAIL = re.compile(r"^#\[cfg\(test\)\]\s*\n(?:#\[[^\n]*\]\s*\n)*mod\s", re.MULTILINE)


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


def violations(root: Path) -> list[str]:
    found: list[str] = []
    for path in tracked_files(root):
        file = root / path
        if not file.is_file():
            continue
        try:
            text = file.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue

        if path not in CHECK_FILES and not path.startswith(SEAM_PROOF_HOME):
            for number, line in enumerate(text.splitlines(), start=1):
                if not SEAM_PROOF.search(line):
                    continue
                if path.startswith(SEAM_PROOF_EVIDENCE_ADR) and SEAM_PROOF_EVIDENCE_PATH in line:
                    continue
                found.append(
                    f"{path}:{number}: the seam-proof test dialect lives only in {SEAM_PROOF_HOME}"
                )

        if not path.endswith(".rs") or TEST_PATH.search(path):
            continue
        production = production_text(text)
        for number, line in enumerate(production.splitlines(), start=1):
            if RETIRED_BINDING_NAMES.search(line):
                found.append(f"{path}:{number}: retired TypeScript binding name: {line.strip()}")
            if path == TYPESCRIPT_ADAPTER:
                continue
            if CONCRETE_TYPE_USE.search(line):
                found.append(
                    f"{path}:{number}: `TypescriptDialect` may only be named as a value a host "
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
            f"check-dialect-boundary: {len(found)} violation(s); see ADR 0096",
            file=sys.stderr,
        )
        return 1
    print("check-dialect-boundary: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
