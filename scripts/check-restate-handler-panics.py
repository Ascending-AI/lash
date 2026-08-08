#!/usr/bin/env python3
"""Reject panic-capable operations from production Restate handler code."""

from __future__ import annotations

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOTS = sorted(ROOT.glob("crates/lash-restate*/src"))
CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
PANIC_CAPABLE = re.compile(
    r"\b(?:panic|unreachable|todo|unimplemented|assert|assert_eq|assert_ne|debug_assert(?:_eq|_ne)?)\s*!\s*\("
    r"|\.\s*(?:expect|expect_err|unwrap)\s*\("
)


def mask_non_code(source: str) -> str:
    """Mask comments and literals while preserving byte offsets and newlines."""
    chars = list(source)
    index = 0
    block_depth = 0
    while index < len(chars):
        if block_depth:
            if source.startswith("/*", index):
                chars[index : index + 2] = "  "
                block_depth += 1
                index += 2
            elif source.startswith("*/", index):
                chars[index : index + 2] = "  "
                block_depth -= 1
                index += 2
            else:
                if chars[index] != "\n":
                    chars[index] = " "
                index += 1
            continue

        if source.startswith("//", index):
            end = source.find("\n", index)
            end = len(chars) if end < 0 else end
            chars[index:end] = " " * (end - index)
            index = end
            continue
        if source.startswith("/*", index):
            chars[index : index + 2] = "  "
            block_depth = 1
            index += 2
            continue

        raw = re.match(r'(?:b)?r(?P<hashes>#{0,255})"', source[index:])
        if raw:
            delimiter = '"' + raw.group("hashes")
            end = source.find(delimiter, index + raw.end())
            end = len(chars) if end < 0 else end + len(delimiter)
            for position in range(index, end):
                if chars[position] != "\n":
                    chars[position] = " "
            index = end
            continue

        prefix_length = 2 if source.startswith('b"', index) else 1
        if source[index : index + prefix_length].endswith('"'):
            end = index + prefix_length
            escaped = False
            while end < len(chars):
                char = source[end]
                end += 1
                if char == '"' and not escaped:
                    break
                escaped = char == "\\" and not escaped
                if char != "\\":
                    escaped = False
            for position in range(index, end):
                if chars[position] != "\n":
                    chars[position] = " "
            index = end
            continue

        index += 1
    return "".join(chars)


def matching_delimiter(source: str, start: int, opening: str, closing: str) -> int:
    depth = 0
    for index in range(start, len(source)):
        if source[index] == opening:
            depth += 1
        elif source[index] == closing:
            depth -= 1
            if depth == 0:
                return index + 1
    return len(source)


def cfg_test_item_end(code: str, start: int) -> int:
    """Return the end of the item, field, or statement guarded by cfg(test)."""
    index = start
    while index < len(code):
        while index < len(code) and code[index].isspace():
            index += 1
        if not code.startswith("#[", index):
            break
        index = matching_delimiter(code, index + 1, "[", "]")

    parens = 0
    brackets = 0
    while index < len(code):
        char = code[index]
        if char == "(":
            parens += 1
        elif char == ")":
            parens -= 1
        elif char == "[":
            brackets += 1
        elif char == "]":
            brackets -= 1
        elif parens == 0 and brackets == 0:
            if char == "{":
                return matching_delimiter(code, index, "{", "}")
            if char in ";,":
                return index + 1
        index += 1
    return len(code)


def mask_cfg_test_items(code: str) -> str:
    chars = list(code)
    spans = [
        (match.start(), cfg_test_item_end(code, match.end()))
        for match in CFG_TEST.finditer(code)
    ]
    for start, end in reversed(spans):
        for position in range(start, end):
            if chars[position] != "\n":
                chars[position] = " "
    return "".join(chars)


def production_sources() -> list[Path]:
    return [
        path
        for source_root in SOURCE_ROOTS
        for path in sorted(source_root.rglob("*.rs"))
        if path.name != "tests.rs"
        and "tests" not in path.relative_to(source_root).parts
    ]


def main() -> int:
    findings: list[str] = []
    for path in production_sources():
        source = path.read_text(encoding="utf-8")
        code = mask_cfg_test_items(mask_non_code(source))
        for match in PANIC_CAPABLE.finditer(code):
            line = code.count("\n", 0, match.start()) + 1
            relative = path.relative_to(ROOT)
            findings.append(f"{relative}:{line}: {source.splitlines()[line - 1].strip()}")

    if findings:
        print(
            "Restate handler panic boundary failed; use typed terminal errors or make the state unrepresentable:",
            file=sys.stderr,
        )
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        return 1

    print("Restate handler panic boundary: clean")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
