#!/usr/bin/env python3
"""Cheap determinism lint for authored tool-orchestration entrypoints."""

from pathlib import Path
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


def orchestration_bodies(source: str):
    marker = "async fn execute_orchestration("
    cursor = 0
    while (start := source.find(marker, cursor)) != -1:
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
    for path in sorted((ROOT / "crates").glob("**/*.rs")):
        source = path.read_text(encoding="utf-8")
        for start, body in orchestration_bodies(source):
            line = source.count("\n", 0, start) + 1
            for token, reason in FORBIDDEN.items():
                if token in body:
                    failures.append(f"{path.relative_to(ROOT)}:{line}: {reason}: {token}")
    if failures:
        print("orchestrating-tool determinism lint failed:", file=sys.stderr)
        print("\n".join(f"- {failure}" for failure in failures), file=sys.stderr)
        return 1
    print("orchestrating-tool determinism lint passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
