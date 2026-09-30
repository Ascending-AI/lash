#!/usr/bin/env python3
"""Run one stable, disjoint libtest shard in a single test process."""
import hashlib
import subprocess
import sys


def shard_assignments(tests: list[str], count: int) -> dict[str, int]:
    """Keep names that can collide under libtest substring skips together."""
    parents = list(range(len(tests)))

    def root(index: int) -> int:
        while parents[index] != index:
            parents[index] = parents[parents[index]]
            index = parents[index]
        return index

    def join(left: int, right: int) -> None:
        left_root = root(left)
        right_root = root(right)
        if left_root != right_root:
            parents[right_root] = left_root

    for left, left_name in enumerate(tests):
        for right in range(left + 1, len(tests)):
            right_name = tests[right]
            if left_name in right_name or right_name in left_name:
                join(left, right)

    groups: dict[int, list[str]] = {}
    for index, name in enumerate(tests):
        groups.setdefault(root(index), []).append(name)
    assignments = {}
    for names in groups.values():
        key = min(names)
        shard = int.from_bytes(hashlib.sha256(key.encode()).digest()[:8], "big") % count
        assignments.update((name, shard) for name in names)
    return assignments


def main() -> int:
    count = int(sys.argv[1])
    index = int(sys.argv[2])
    if count < 1 or index < 0 or index >= count:
        raise SystemExit("shard count must be positive and index must be in range")
    binary = sys.argv[3]
    arguments = sys.argv[4:]
    listed = subprocess.run(
        [binary, "--list", "--format", "terse"],
        check=True,
        capture_output=True,
        text=True,
    )
    tests = [line.rsplit(": ", 1)[0] for line in listed.stdout.splitlines() if line.endswith(": test")]
    assignments = shard_assignments(tests, count)
    skipped = [
        name
        for name in tests
        if assignments[name] != index
    ]
    command = [binary, *arguments]
    for name in skipped:
        command.extend(["--skip", name])
    return subprocess.run(command).returncode


if __name__ == "__main__":
    raise SystemExit(main())
