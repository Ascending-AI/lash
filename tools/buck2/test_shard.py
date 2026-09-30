#!/usr/bin/env python3
"""Run one stable, disjoint libtest shard in a single test process."""
import hashlib
import subprocess
import sys


def main() -> int:
    count = int(sys.argv[1])
    index = int(sys.argv[2])
    binary = sys.argv[3]
    arguments = sys.argv[4:]
    listed = subprocess.run(
        [binary, "--list", "--format", "terse"],
        check=True,
        capture_output=True,
        text=True,
    )
    tests = [line.rsplit(": ", 1)[0] for line in listed.stdout.splitlines() if line.endswith(": test")]
    skipped = [
        name
        for name in tests
        if int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], "big") % count != index
    ]
    command = [binary, *arguments]
    for name in skipped:
        command.extend(["--skip", name])
    return subprocess.run(command).returncode


if __name__ == "__main__":
    raise SystemExit(main())
