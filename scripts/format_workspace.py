#!/usr/bin/env python3
"""Format every workspace member with the workspace Cargo configuration."""

from __future__ import annotations

from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    return subprocess.call(["cargo", "fmt", "--all", *sys.argv[1:]], cwd=ROOT)


if __name__ == "__main__":
    raise SystemExit(main())
