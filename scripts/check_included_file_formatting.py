#!/usr/bin/env python3
"""Format-check the files that `include!` pulls in.

`cargo fmt` walks modules, and an `include!`d file is not a module: it is text
spliced into whichever file names it. Those files are invisible to
`cargo fmt --all` and to `cargo fmt --all --check`, so they drift silently.

The list used to be maintained by hand in CI, which meant the check covered one
file out of sixty. This finds every `include!` target instead, resolving each
path the way Rust does — relative to the directory of the file doing the
including — so a new one is covered the moment it exists.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

INCLUDE = re.compile(r'include!\("([^"]+)"\)')

# Scopes whose include!d files are formatted and stay that way. The rest of the
# repository has around sixty more that predate this check; adding a scope here
# is how one becomes covered, and the intended direction is to add scopes, never
# to remove them.
DEFAULT_SCOPES = ("crates/lashlang",)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def included_files(roots: list[Path]) -> list[Path]:
    found: set[Path] = set()
    sources: list[Path] = []
    for root in roots:
        if root.is_file():
            sources.append(root)
        else:
            sources.extend(root.rglob("*.rs"))
    for source in sources:
        if "target" in source.parts:
            continue
        try:
            text = source.read_text()
        except (OSError, UnicodeDecodeError):
            continue
        for match in INCLUDE.finditer(text):
            target = (source.parent / match.group(1)).resolve()
            if target.is_file():
                found.add(target)
    return sorted(found)


def main() -> int:
    root = repo_root()
    scopes = sys.argv[1:] or list(DEFAULT_SCOPES)
    roots = [root / scope for scope in scopes]
    missing = [str(path) for path in roots if not path.exists()]
    if missing:
        print(f"error: scope not found: {', '.join(missing)}", file=sys.stderr)
        return 1
    targets = included_files(roots)
    if not targets:
        print("error: no include!d files found; the scan is broken", file=sys.stderr)
        return 1

    failures: list[str] = []
    for target in targets:
        result = subprocess.run(
            ["rustfmt", "--edition", "2024", "--check", str(target)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            failures.append(str(target.relative_to(root)))
            if result.stdout:
                print(result.stdout, end="")
            if result.stderr:
                print(result.stderr, end="", file=sys.stderr)

    if failures:
        print(
            f"include!d files are not formatted ({len(failures)}): "
            + ", ".join(failures),
            file=sys.stderr,
        )
        return 1
    print(f"include!d file formatting: ok ({len(targets)} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
