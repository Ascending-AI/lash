#!/usr/bin/env python3
"""Cheap determinism lint for authored tool-orchestration entrypoints."""

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
FORBIDDEN = {
    "SystemTime::now": "wall clock",
    "Instant::now": "wall clock",
    "Uuid::new_v4": "random identity",
    "rand::": "randomness",
    "thread_rng": "randomness",
    "HashMap::": "unordered iteration source",
    "HashSet::": "unordered iteration source",
    "tokio::spawn": "un-awaited spawned work",
}

ALIASED_FORBIDDEN = {
    "SystemTime": ("::now", "wall clock"),
    "Instant": ("::now", "wall clock"),
    "Uuid": ("::new_v4", "random identity"),
    "HashMap": ("::", "unordered iteration source"),
    "HashSet": ("::", "unordered iteration source"),
    "spawn": ("(", "un-awaited spawned work"),
    "thread_rng": ("(", "randomness"),
}

ENTRYPOINT = re.compile(r"\basync\s+fn\s+execute_orchestration(?:_by_id)?\s*\(")
EXPECTED_ENTRYPOINTS = {
    Path("crates/lash-protocol-standard/src/lib.rs"): 1,
    Path("crates/lash-subagents/src/rlm.rs"): 1,
}
USE = re.compile(r"\buse\s+([^;]+);")
ALIAS = re.compile(r"\b(SystemTime|Instant|Uuid|HashMap|HashSet|spawn|thread_rng)\s+as\s+([A-Za-z_][A-Za-z0-9_]*)")


def code_only(source: str) -> str:
    """Mask comments and string/character literals while preserving offsets."""
    chars = list(source)
    index = 0
    while index < len(chars):
        if source.startswith("//", index):
            end = source.find("\n", index)
            end = len(chars) if end == -1 else end
            chars[index:end] = " " * (end - index)
            index = end
        elif source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(chars) and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            for offset in range(index, end):
                if chars[offset] != "\n":
                    chars[offset] = " "
            index = end
        elif chars[index] == '"':
            end = index + 1
            while end < len(chars):
                if chars[end] == "\\":
                    end += 2
                elif chars[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            for offset in range(index, min(end, len(chars))):
                if chars[offset] != "\n":
                    chars[offset] = " "
            index = end
        else:
            index += 1
    return "".join(chars)


def import_aliases(source: str) -> dict[str, tuple[str, str]]:
    aliases = {}
    for statement in USE.findall(source):
        for original, alias in ALIAS.findall(statement):
            suffix, reason = ALIASED_FORBIDDEN[original]
            aliases[alias] = (suffix, reason)
    return aliases


def orchestration_bodies(source: str):
    source = code_only(source)
    cursor = 0
    while match := ENTRYPOINT.search(source, cursor):
        start = match.start()
        brace = source.find("{", start)
        if brace == -1:
            break
        depth = 1
        end = brace + 1
        while end < len(source) and depth:
            if source[end] == "{":
                depth += 1
            elif source[end] == "}":
                depth -= 1
            end += 1
        yield start, source[start:end]
        cursor = end


def main() -> int:
    failures = []
    matched_entrypoints = {}
    for path in sorted((ROOT / "crates").glob("**/*.rs")):
        source = path.read_text(encoding="utf-8")
        aliases = import_aliases(code_only(source))
        for start, body in orchestration_bodies(source):
            relative_path = path.relative_to(ROOT)
            matched_entrypoints[relative_path] = matched_entrypoints.get(relative_path, 0) + 1
            line = source.count("\n", 0, start) + 1
            for token, reason in FORBIDDEN.items():
                if token in body:
                    failures.append(f"{path.relative_to(ROOT)}:{line}: {reason}: {token}")
            for alias, (suffix, reason) in aliases.items():
                token = f"{alias}{suffix}"
                if token in body:
                    failures.append(
                        f"{path.relative_to(ROOT)}:{line}: {reason}: {token} "
                        f"(aliased import)"
                    )
    if matched_entrypoints != EXPECTED_ENTRYPOINTS:
        failures.append(
            "entrypoint coverage changed: "
            f"expected {EXPECTED_ENTRYPOINTS}, matched {matched_entrypoints}"
        )
    if failures:
        print("orchestrating-tool determinism lint failed:", file=sys.stderr)
        print("\n".join(f"- {failure}" for failure in failures), file=sys.stderr)
        return 1
    print("orchestrating-tool determinism lint passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
