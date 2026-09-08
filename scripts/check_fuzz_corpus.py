#!/usr/bin/env python3
"""Verify every fuzz target has a committed, non-empty seed corpus.

The fuzz smoke job runs each target only over its committed seeds; a target
whose corpus went missing or was emptied would silently smoke-test nothing.
This gate fails the run instead (FIG-878).
"""

from __future__ import annotations

from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]
FUZZ_MANIFEST = REPO_ROOT / "fuzz" / "Cargo.toml"
CORPUS_ROOT = REPO_ROOT / "fuzz" / "corpus"


def fuzz_targets(manifest_text: str) -> list[str]:
    """Returns the [[bin]] target names declared by the fuzz manifest."""
    targets = re.findall(
        r"^\[\[bin\]\]\s*\nname\s*=\s*\"([^\"]+)\"", manifest_text, flags=re.MULTILINE
    )
    if not targets:
        raise ValueError("fuzz/Cargo.toml declares no [[bin]] fuzz targets")
    return targets


def corpus_problems(targets: list[str], corpus_root: Path) -> list[str]:
    """Returns one problem line per target whose seed corpus is unusable."""
    problems: list[str] = []
    for target in targets:
        corpus_dir = corpus_root / target
        if not corpus_dir.is_dir():
            problems.append(f"{target}: missing corpus directory {corpus_dir}")
            continue
        seeds = [path for path in sorted(corpus_dir.iterdir()) if path.is_file()]
        if not seeds:
            problems.append(f"{target}: corpus directory has no seed files")
            continue
        empty = [path.name for path in seeds if path.stat().st_size == 0]
        if empty:
            problems.append(f"{target}: empty seed files: {', '.join(empty)}")
    return problems


def main() -> int:
    targets = fuzz_targets(FUZZ_MANIFEST.read_text(encoding="utf-8"))
    problems = corpus_problems(targets, CORPUS_ROOT)
    for problem in problems:
        print(f"fuzz corpus check: {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"fuzz corpus check passed: {len(targets)} targets seeded")
    return 0


if __name__ == "__main__":
    sys.exit(main())
