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
    # Live service proofs belong in their named integration gates.
    "scripts/test-gate-worktree-concurrency.sh": "Needs Docker services and a peer worktree argument.",
    "scripts/test-mcp-catalog.sh": "Builds Rust law witnesses and runs PostgreSQL and live Restate.",
    "scripts/test-restate-workers-trace-scrub.sh": "Creates real Docker containers and a trace volume.",
    # A lander-environment proof: it needs `kiln` on PATH and runs with
    # scripts/ci/landing-gates.sh, not on a CI runner without Kiln.
    "scripts/test_landing_gates.py": "Lander-only proof: requires `kiln` on PATH.",
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
