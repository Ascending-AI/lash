#!/usr/bin/env python3
"""Prove no unvetted entropy source can produce a value in durable reach.

FIG-1278 doctrine: entropy may never produce a value that lands in a durable
record. A replay or redrive that recomputes a minted value diverges from the
journaled one, so durable-reaching identity must be derived from replayable
inputs or required from the host. This check enforces the rule at the source
level: every entropy call in a swept crate must be either

* inside test code (a `#[cfg(test)]` item, a `#[test]`/`#[tokio::test]`
  function, or a `*test*`/`testing` path), or
* annotated with a `durable-entropy:` comment on the same line or within
  three lines above it, stating why the minted value cannot reach a durable
  record (for example a lease/incarnation fencing nonce, a live-stream
  correlation id, or a journaled effect outcome).

Comments and string literals are blanked before the scan, so documentation
that merely *mentions* an entropy call does not trip the gate.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]

# Crates whose production code may mint values that land in durable records.
SWEPT_DIRS = (
    "crates/lash-core/src",
    "crates/lash-core-execution/src",
    "crates/lash-core-llm/src",
    "crates/lash-lashlang-runtime/src",
    "crates/lash-protocol-rlm/src",
)

# Calls that mint nondeterministic values. `SystemTime::now` and
# `std::process::id` are included because time-as-identity and host-local
# identity are the same defect class as a minted uuid.
ENTROPY = re.compile(
    r"Uuid::new_v4|Uuid::new_v6|Uuid::new_v7|Uuid::new_v8|Uuid::now_v7"
    r"|rand::|thread_rng|fastrand|getrandom"
    r"|SystemTime::now"
    r"|std::process::id\b|process::id\(\)"
    r"|thread::current\(\)"
)

ANNOTATION = re.compile(r"durable-entropy:")

# Attributes that gate the following item to test builds or mark a test case.
TEST_ATTR = re.compile(
    r"#\s*\[\s*(?:cfg\s*\([^)]*\btest\b[^)]*\)|test|tokio::test|test_case)"
)

# How far above a hit an annotation comment may sit. Three lines covers a
# `let x = ...` statement whose justification comment precedes it.
ANNOTATION_LOOKBACK = 3


def is_test_path(relpath: str) -> bool:
    parts = Path(relpath).parts
    return any(
        "test" in part or part == "testing" for part in parts[:-1]
    ) or "test" in parts[-1]


def code_skeleton(text: str) -> str:
    """Blank comments and string/char literals, preserving line numbers."""
    out = list(text)
    i, n = 0, len(text)
    while i < n:
        ch = text[i]
        pair = text[i : i + 2]
        if pair == "//":
            while i < n and text[i] != "\n":
                out[i] = " "
                i += 1
        elif pair == "/*":
            depth = 1
            out[i] = out[i + 1] = " "
            i += 2
            while i < n and depth:
                if text[i : i + 2] == "/*":
                    depth += 1
                    out[i] = out[i + 1] = " "
                    i += 2
                elif text[i : i + 2] == "*/":
                    depth -= 1
                    out[i] = out[i + 1] = " "
                    i += 2
                else:
                    if text[i] != "\n":
                        out[i] = " "
                    i += 1
        elif ch == '"':
            out[i] = " "
            i += 1
            while i < n and text[i] != '"':
                if text[i] == "\\":
                    out[i] = " "
                    i += 1
                    if i < n:
                        out[i] = " " if text[i] != "\n" else "\n"
                        i += 1
                else:
                    if text[i] != "\n":
                        out[i] = " "
                    i += 1
            if i < n:
                out[i] = " "
                i += 1
        elif pair == "r#" and text[i + 2 : i + 3] == '"':
            out[i] = out[i + 1] = out[i + 2] = " "
            i += 3
            while i < n and text[i : i + 2] != '"#':
                if text[i] != "\n":
                    out[i] = " "
                i += 1
            if i < n:
                out[i] = out[i + 1] = " "
                i += 2
        elif ch == "'" and re.match(r"'\\?.'", text[i : i + 4]):
            for j in range(i, min(i + 3, n)):
                out[j] = " "
            i += 3
        else:
            i += 1
    return "".join(out)


def test_item_ranges(lines: list[str]) -> list[tuple[int, int]]:
    """Line ranges (0-based, inclusive) of test-gated items.

    After a `#[cfg(test)]`/`#[test]`-style attribute, the gated item's body is
    the brace-balanced block that follows it. Items without a body (a `const`
    or a `;` item) contribute no range.
    """
    ranges: list[tuple[int, int]] = []
    text = "\n".join(lines)
    offsets = []
    pos = 0
    for line in lines:
        offsets.append(pos)
        pos += len(line) + 1
    for idx, line in enumerate(lines):
        if not TEST_ATTR.search(line):
            continue
        # Find the first `{` at or after the attribute line.
        cursor = offsets[idx]
        brace_at = text.find("{", cursor)
        semi_at = text.find(";", cursor)
        nl_at = text.find("\n", cursor)
        if brace_at == -1 or (semi_at != -1 and semi_at < brace_at):
            continue
        # The `{` must belong to this item, not a later one: only attributes,
        # signatures, and whitespace may intervene. Approximate by refusing a
        # `}` or a newline-separated `;` before the brace — a `;` above already
        # returned, and a `}` means a prior item's close on this path.
        segment = text[cursor:brace_at]
        if "}" in segment:
            continue
        depth = 0
        j = brace_at
        while j < len(text):
            if text[j] == "{":
                depth += 1
            elif text[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        end_line = text.count("\n", 0, j)
        ranges.append((idx, end_line))
    return ranges


def in_ranges(line_no: int, ranges: list[tuple[int, int]]) -> bool:
    return any(start <= line_no <= end for start, end in ranges)


def scan_source(relpath: str, text: str) -> list[str]:
    """Return violation descriptions for one swept source file."""
    if is_test_path(relpath):
        return []
    skeleton = code_skeleton(text)
    skeleton_lines = skeleton.split("\n")
    original_lines = text.split("\n")
    ranges = test_item_ranges(skeleton_lines)
    violations: list[str] = []
    for idx, line in enumerate(skeleton_lines):
        if not ENTROPY.search(line):
            continue
        if in_ranges(idx, ranges):
            continue
        window = original_lines[max(0, idx - ANNOTATION_LOOKBACK) : idx + 1]
        if any(ANNOTATION.search(above) for above in window):
            continue
        violations.append(f"{relpath}:{idx + 1}: {original_lines[idx].strip()}")
    return violations


def swept_files(root: Path) -> list[Path]:
    files: list[Path] = []
    for directory in SWEPT_DIRS:
        base = root / directory
        assert base.is_dir(), f"swept directory moved: {directory}"
        files.extend(sorted(base.rglob("*.rs")))
    return files


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    violations: list[str] = []
    for path in swept_files(args.root):
        violations.extend(
            scan_source(path.relative_to(args.root).as_posix(), path.read_text())
        )
    if violations:
        print("durable-entropy violations:", file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        print(
            "entropy must never produce a value that lands in a durable record "
            "(FIG-1278): derive the value from replayable inputs, require it "
            "from the host, or justify the exception inline with a "
            "`durable-entropy:` comment",
            file=sys.stderr,
        )
        return 1
    print("durable-entropy check: all swept entropy sites are test-only or justified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
