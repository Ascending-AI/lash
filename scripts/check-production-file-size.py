#!/usr/bin/env python3
"""Check Rust source line budgets without spawning a process per file."""

from fnmatch import fnmatchcase
import os
import re
import stat
import sys


EXCLUDED = (
    "*/.git", "*/.claude", "*/target", "*/.tgt", "*/vendor",
    "*/crates/lash-regress", "*/vendored", "*/generated",
)
TEST_PATHS = (
    "*/lash-conformance/src/*", "*/tests/*", "*/test/*", "*/testing/*",
    "*/src/tests.rs", "*/src/test.rs", "*/src/*/tests.rs", "*/src/*/test.rs",
    "*/src/*_tests.rs", "*/language/support.rs",
)
DOC_COMMENT = re.compile(rb"^[ \t\r\v\f]*//[/!]")


def rust_files(path: str):
    if any(fnmatchcase(path, pattern) or fnmatchcase(path, pattern + "/*") for pattern in EXCLUDED):
        return
    mode = os.stat(path, follow_symlinks=False).st_mode
    if stat.S_ISDIR(mode):
        with os.scandir(path) as entries:
            for entry in entries:
                yield from rust_files(entry.path)
    elif stat.S_ISREG(mode) and path.endswith(".rs"):
        yield path


def main() -> int:
    production_limit = int(os.environ.get("LASH_PRODUCTION_RUST_LINE_LIMIT") or "1600")
    test_limit = int(os.environ.get("LASH_TEST_RUST_LINE_LIMIT") or "2500")
    failures = []
    for root in sys.argv[1:] or ["."]:
        for path in rust_files(root):
            relative = path.removeprefix("./")
            is_test = any(fnmatchcase(relative, pattern) for pattern in TEST_PATHS)
            limit = test_limit if is_test else production_limit
            with open(path, "rb") as source:
                lines = sum(not DOC_COMMENT.match(line) for line in source)
            if lines > limit:
                kind = "test" if is_test else "production"
                failures.append(f"{kind}:{lines}:{relative}")
    if failures:
        print("Rust files over line budget:", file=sys.stderr)
        print(f"  production limit: {production_limit} lines", file=sys.stderr)
        print(f"  test/support limit: {test_limit} lines", file=sys.stderr)
        print("\n".join(f"  {failure}" for failure in failures), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
