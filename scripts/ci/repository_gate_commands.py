#!/usr/bin/env python3
"""Discover repository self-tests for the local and CI command runners."""

from pathlib import Path
import shlex


ROOT = Path(__file__).resolve().parents[2]
DIRECTORIES = ("scripts", "tools/buck2", "tools/buck2/tests")
PATTERNS = ("test_*.py", "test-*.sh")

# These production entrypoints happen to share the self-test naming convention.
EXCLUSIONS = {
    "tools/buck2/test_runner.py": "Production Buck2 external test executor, not a self-test.",
    "tools/buck2/test_selection.py": "Production libtest selection module, not a self-test.",
    "tools/buck2/test_shard.py": "Production libtest shard wrapper, not a self-test.",
    "tools/buck2/test_timeout.py": "Production test timeout wrapper, not a self-test.",
}


def self_test_paths(root: Path = ROOT) -> list[str]:
    return sorted(
        path.relative_to(root).as_posix()
        for directory in DIRECTORIES
        for pattern in PATTERNS
        for path in (root / directory).glob(pattern)
        if path.is_file() and path.relative_to(root).as_posix() not in EXCLUSIONS
    )


def commands(root: Path = ROOT) -> list[str]:
    return [
        f"{'python3' if path.endswith('.py') else 'bash'} {shlex.quote(path)}"
        for path in self_test_paths(root)
    ]


if __name__ == "__main__":
    print("\n".join(commands()))
