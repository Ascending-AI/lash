#!/usr/bin/env python3
"""Every substrate stub names the lane that fills it (I0, FIG-5194; ADR 0132).

I0 lands the substrate seams as compiled skeletons whose bodies are
`todo!("<lane> (<ticket>): <what>")`. A stub without a known lane tag is
work nobody owns. The default mode fails on any `todo!(` or `unimplemented!(`
under the substrate paths whose message does not start with a known lane and
that lane's ticket, and prints the stubs per lane. `--final` (L13, FIG-5193)
also fails on every stub that remains.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import Counter
from pathlib import Path

# The lanes that own substrate stubs, with their tickets.
LANES = {
    "V0": "FIG-5170",
    "L3": "FIG-5172",
    "L3s": "FIG-5196",
    "L4": "FIG-5174",
    "L5": "FIG-5173",
    "L6": "FIG-5175",
    "L6b": "FIG-5176",
    "L7": "FIG-5177",
    "L7b": "FIG-5198",
    "L7p": "FIG-5197",
    "L8": "FIG-5178",
    "L10a": "FIG-5190",
    "L10g": "FIG-5200",
    "L11": "FIG-5187",
    "L13": "FIG-5193",
}

# The crates the substrate seams live in or reach (I0's boundary).
SUBSTRATE_PATHS = (
    "crates/lash",
    "crates/lash-conformance",
    "crates/lash-core",
    "crates/lash-core-effect",
    "crates/lash-core-execution",
    "crates/lash-core-store",
    "crates/lash-durable",
    "crates/lash-durable-test",
    "crates/lash-vm-runtime",
    "crates/lash-postgres-store",
    "crates/lash-protocol-rlm",
    "crates/lash-sansio",
    "crates/lash-sqlite-store",
    "crates/lash-store-sql",
    "crates/lash-vm-broker",
    "crates/lash-vm-client",
    "crates/lash-vm",
)

# The facade's compile-time witnesses type-check signatures with `todo!()`
# as typed holes in code that never runs; they are not stubs.
WITNESSES = re.compile(r"crates/lash/tests/[a-z_]+_evidence(?:_b)?\.rs\Z")

STUB = re.compile(r"\b(todo|unimplemented)!\s*\(")
TAG = re.compile(r'\s*"(?P<lane>[A-Za-z0-9]+) \((?P<ticket>FIG-\d+)\): \S')


def code_only(source: str) -> str:
    """`source` with comments and string contents blanked, offsets kept."""
    out = list(source)
    index, length = 0, len(source)
    while index < length:
        if source.startswith("//", index):
            end = source.find("\n", index)
            end = length if end < 0 else end
            for at in range(index, end):
                out[at] = " "
            index = end
        elif source.startswith("/*", index):
            depth, at = 1, index + 2
            while at < length and depth:
                if source.startswith("/*", at):
                    depth, at = depth + 1, at + 2
                elif source.startswith("*/", at):
                    depth, at = depth - 1, at + 2
                else:
                    at += 1
            for blank in range(index, at):
                if out[blank] != "\n":
                    out[blank] = " "
            index = at
        elif source[index] == '"':
            at = index + 1
            while at < length and source[at] != '"':
                at += 2 if source[at] == "\\" else 1
            for blank in range(index + 1, at):
                if out[blank] != "\n":
                    out[blank] = " "
            index = at + 1
        else:
            index += 1
    return "".join(out)


def stubs(root: Path) -> list[tuple[str, int, str | None]]:
    """Each stub as (path, line, lane or None when untagged)."""
    found = []
    for base in SUBSTRATE_PATHS:
        directory = root / base
        if not directory.is_dir():
            continue
        for path in sorted(directory.rglob("*.rs")):
            relative = path.relative_to(root).as_posix()
            if WITNESSES.match(relative):
                continue
            if any(relative.startswith(other + "/") for other in SUBSTRATE_PATHS if other != base and other.startswith(base)):
                continue  # crates/lash is a prefix of crates/lash-*: each file once
            source = path.read_text(encoding="utf-8")
            code = code_only(source)
            for match in STUB.finditer(code):
                line = source.count("\n", 0, match.start()) + 1
                tag = TAG.match(source, match.end())
                lane = None
                if tag and LANES.get(tag["lane"]) == tag["ticket"]:
                    lane = tag["lane"]
                found.append((relative, line, lane))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--final", action="store_true", help="also fail on every remaining stub")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    found = stubs(args.root)
    untagged = [(path, line) for path, line, lane in found if lane is None]
    for path, line in untagged:
        print(f"{path}:{line}: a stub without a known lane tag: todo!(\"<lane> (<ticket>): ...\")", file=sys.stderr)
    counts = Counter(lane for _, _, lane in found if lane)
    order = list(LANES)
    print("substrate stubs by lane: " + (", ".join(
        f"{lane} {counts[lane]}" for lane in order if counts[lane]) or "none") + f" ({sum(counts.values())} in all)")
    if args.final:
        for path, line, lane in found:
            if lane:
                print(f"{path}:{line}: {lane} stub remains", file=sys.stderr)
        return 1 if found else 0
    return 1 if untagged else 0


if __name__ == "__main__":
    sys.exit(main())
