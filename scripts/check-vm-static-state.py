#!/usr/bin/env python3
"""Keep guest-derived state out of statics in the VM crates (FIG-4158, ADR 0123).

Model code runs in a worker that is reset and reused across sessions. A reset
drops the one owned `VmInstance` and installs a pristine one, so every piece of
guest-derived state has to live in that instance: anything a `static`,
`thread_local!`, `OnceLock`, `LazyLock` or `lazy_static!` holds survives the
reset and is shared by the next session.

This check refuses every static item in the VM, compiler and runtime crates
unless `scripts/vm-static-state-allowlist.txt` lists it with a one-line reason.
The allowlist is for guest-free constants and build tables only: data no guest
input ever reaches, or test-only instrumentation that never links into a
worker. An entry that no longer matches a static fails the check too, so the
list stays exactly the reviewed set.

Allowlist lines are `<path>::<scope>::<NAME>  # <reason>`, where `<scope>` is
the enclosing `fn` (or `<module>` for an item at module level). Every static
item form declares a `static NAME:` item, so that is what the check keys on:
`thread_local!` and `lazy_static!` bodies, `OnceLock`/`LazyLock` cells and
`static mut` all do.

Only `src/` is checked: a crate's `tests/`, `examples/` and `benches/` trees
build separate binaries that never link into a worker.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
ALLOWLIST = Path("scripts/vm-static-state-allowlist.txt")

# The crates whose code runs inside a VM worker. The worker crate joins the
# list the moment it exists, so its first static is reviewed like any other.
VM_CRATES = (
    "crates/lashlang",
    "crates/lash-typescript",
    "crates/lash-lashlang-runtime",
)
WORKER_CRATE = "crates/lash-vm-worker"

STATIC_ITEM = re.compile(r"(?<![\w'])static\s+(?:mut\s+|ref\s+)?([A-Za-z_]\w*)\s*:")


@dataclass(frozen=True)
class StaticItem:
    path: str
    scope: str
    name: str
    line: int

    @property
    def key(self) -> str:
        return f"{self.path}::{self.scope}::{self.name}"


def checked_crates(root: Path) -> list[str]:
    crates = list(VM_CRATES)
    if (root / WORKER_CRATE).is_dir():
        crates.append(WORKER_CRATE)
    return crates


def strip_comments(text: str) -> list[str]:
    """Source lines with `//` and `/* */` comments and string literals blanked.

    Line numbers are preserved. Raw strings and char literals are treated as
    ordinary text; neither ever spells a static item in these crates.
    """
    out: list[str] = []
    in_block = 0
    for raw in text.splitlines():
        line = []
        index = 0
        in_string = False
        while index < len(raw):
            pair = raw[index : index + 2]
            char = raw[index]
            if in_block:
                if pair == "*/":
                    in_block -= 1
                    index += 2
                    continue
                if pair == "/*":
                    in_block += 1
                    index += 2
                    continue
                index += 1
                continue
            if in_string:
                if char == "\\":
                    index += 2
                    continue
                if char == '"':
                    in_string = False
                line.append(" ")
                index += 1
                continue
            if pair == "//":
                break
            if pair == "/*":
                in_block += 1
                index += 2
                continue
            if char == '"':
                in_string = True
                line.append(" ")
                index += 1
                continue
            line.append(char)
            index += 1
        out.append("".join(line))
    return out


CHAR_LITERAL = re.compile(r"'(?:[^'\\\n]|\\[^\n]{1,9}?)'")
FN_NAME = re.compile(r"(?<![\w])fn\s+(\w+)")


def statics_in(root: Path, path: Path) -> list[StaticItem]:
    relative = path.relative_to(root).as_posix()
    items = []
    # The enclosing `fn` of each item: a `fn` opens its scope at the first `{`
    # after its name and closes it at the matching `}`; a bodiless `fn ...;`
    # (a trait method) opens nothing.
    depth = 0
    pending: str | None = None
    scopes: list[tuple[int, str]] = []
    for number, line in enumerate(strip_comments(path.read_text()), start=1):
        line = CHAR_LITERAL.sub("   ", line)
        for match in STATIC_ITEM.finditer(line):
            scope = scopes[-1][1] if scopes else "<module>"
            items.append(StaticItem(relative, scope, match.group(1), number))
        events = [(m.start(), "fn", m.group(1)) for m in FN_NAME.finditer(line)]
        events += [(i, c, "") for i, c in enumerate(line) if c in "{};"]
        for _, kind, name in sorted(events):
            if kind == "fn":
                pending = name
            elif kind == "{":
                depth += 1
                if pending is not None:
                    scopes.append((depth, pending))
                    pending = None
            elif kind == "}":
                if scopes and scopes[-1][0] == depth:
                    scopes.pop()
                depth -= 1
            elif kind == ";":
                pending = None
    return items


def collect(root: Path) -> list[StaticItem]:
    items = []
    for crate in checked_crates(root):
        source = root / crate / "src"
        if not source.is_dir():
            continue
        for path in sorted(source.rglob("*.rs")):
            items.extend(statics_in(root, path))
    return items


def load_allowlist(root: Path) -> tuple[dict[str, str], list[str]]:
    """`{key: reason}` and the malformed lines; a line without a reason is one."""
    entries: dict[str, str] = {}
    problems = []
    path = root / ALLOWLIST
    for number, raw in enumerate(path.read_text().splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        body, _, reason = line.partition("#")
        fields = body.split()
        if len(fields) != 1 or fields[0].count("::") != 2 or not reason.strip():
            problems.append(f"{ALLOWLIST}:{number}: want `<path>::<scope>::<NAME>  # <reason>`")
            continue
        if fields[0] in entries:
            problems.append(f"{ALLOWLIST}:{number}: `{fields[0]}` is listed twice")
            continue
        entries[fields[0]] = reason.strip()
    return entries, problems


def check(root: Path) -> list[str]:
    allowlist, problems = load_allowlist(root)
    items = collect(root)
    seen = set()
    for item in items:
        seen.add(item.key)
        if item.key not in allowlist:
            problems.append(
                f"{item.path}:{item.line}: static `{item.name}` (in `{item.scope}`) is not on "
                f"{ALLOWLIST}; guest-derived state belongs in the VmInstance, and a guest-free "
                "constant needs an allowlist entry with its reason"
            )
    for key in sorted(set(allowlist) - seen):
        problems.append(f"{ALLOWLIST}: `{key}` matches no static item; remove the stale entry")
    return problems


def main(argv: list[str]) -> int:
    root = Path(argv[1]).resolve() if len(argv) > 1 else ROOT
    problems = check(root)
    if problems:
        print("VM static-state check failed:", file=sys.stderr)
        print("\n".join(f"  {problem}" for problem in problems), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
