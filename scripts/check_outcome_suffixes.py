#!/usr/bin/env python3
"""ADR 0068 outcome-suffix gate.

A public type a host can name must not end in `Result`, `Disposition`, or
`Summary`. `Result` is reserved for `std::Result` aliases; `Disposition` and
`Summary` are retired outright (a projection is a `...View` or the domain
noun, an aggregate over many items is a `...Report`).

The scanned surface is the whole workspace's Rust source: every `.rs` file
under `crates/`, `examples/`, and `runbooks/` — the roots the workspace
member list spans — so `#[path]`-shared sources such as `examples/shared/`
and crate-local bench/example targets are covered too. For each file the
scan collects every `pub` item declaration and every leaf name a `pub use`
tree binds, at any module depth:

* a declaration site flags its own name;
* a `pub use` leaf flags the name the host writes — including an `as` alias,
  which mints a name nothing declared;
* a `pub use ...::*` glob needs no resolution: every item it can surface is
  a `pub` declaration in some scanned file, flagged at that declaration.

Exempt, each with the reason the rule does not reach it:

* `tests/` directories — separate test targets, not part of a crate's
  public surface;
* `#[cfg(test)]`-gated items (`mod` blocks, file modules, and standalone
  items) — private test support no host can name;
* `crates/lash-restate-test/src/protocol/generated.rs` — vendored
  prost-build output, byte-for-byte the file restate-sdk-shared-core
  compiles from the pinned protocol; its names are the Restate spec's, not
  this workspace's.
"""

from __future__ import annotations

from pathlib import Path
import re
import sys

REPO = Path(__file__).resolve().parents[1]
SOURCE_ROOTS = ("crates", "examples", "runbooks")

RETIRED = ("Result", "Disposition", "Summary")

ITEM = re.compile(r"\bpub\s+(?:struct|enum|union|trait|type)\s+([A-Za-z_][A-Za-z0-9_]*)")
PUB_USE = re.compile(r"\bpub\s+use\b")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
CFG_ATTR = re.compile(r"#\s*\[\s*cfg\s*\(")
USE_TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|::|\{|\}|,|\*|\S")
# What may sit between a `#[cfg(test)]` attribute and the item it gates:
# whitespace, other attributes, visibility, and `mod`'s own keywords.
ATTR_TARGET = re.compile(
    r"(?:\s|#\s*\[[^\]]*\])*(?:pub\s*(?:\([^)]*\))?\s+)?([A-Za-z_][A-Za-z0-9_]*)\b\s*([A-Za-z_][A-Za-z0-9_]*)?"
)
NONCODE_START = re.compile(r"//|/\*|b?r#*\"|b?\"|b?'")
CHAR_LITERAL = re.compile(r"b?'(?:\\(?:.|x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\})|[^'\\\n])'")

# Genuine `std::Result` aliases — the one use of `Result` ADR 0068 reserves.
# A name lands here only by ending in `= ... Result<...>` at its definition;
# domain types never do.
RESULT_ALIASES = frozenset(
    {
        "Result",
        "MaintenanceResult",
        "TriggerEffectResult",
        "TriggerOccurrenceReclamationResult",
    }
)

# Files exempted beyond test support, each named with its reason in the
# module docstring.
EXEMPT_FILES = frozenset(
    {
        "crates/lash-restate-test/src/protocol/generated.rs",
    }
)


def blank_noncode(text: str) -> str:
    """Blank comments, strings, and char literals, preserving offsets.

    Newlines stay so line numbers survive; a `{` or `;` left standing is real
    code, never a format string or a doc example.
    """
    out = list(text)
    pos, n = 0, len(text)

    def blank(a: int, b: int) -> None:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while True:
        match = NONCODE_START.search(text, pos)
        if match is None:
            break
        token = match.group(0)
        i = match.start()
        if token == "//":
            end = text.find("\n", i)
            end = n if end < 0 else end
            blank(i, end)
            pos = end
        elif token == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                pair = text[j : j + 2]
                if pair == "/*":
                    depth += 1
                    j += 2
                elif pair == "*/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)
            pos = j
        elif token.endswith('"'):
            if "r" in token[:-1]:  # raw string: r"..." / r#"..."# / br#"..."#
                closer = '"' + "#" * token.count("#")
                end = text.find(closer, i + len(token))
                end = n if end < 0 else end + len(closer)
            else:
                end = i + len(token)
                while end < n:
                    if text[end] == "\\":
                        end += 2
                        continue
                    if text[end] == '"':
                        end += 1
                        break
                    end += 1
            blank(i, end)
            pos = end
        else:  # `'` or `b'`: a char literal blanks; a lifetime does not
            char = CHAR_LITERAL.match(text, i)
            if char is not None:
                blank(i, char.end())
                pos = char.end()
            else:
                pos = i + 1
    return "".join(out)


def cfg_gates_on_test(args: str) -> bool:
    """True when a `cfg(...)` predicate positively requires `test`.

    `feature = "testing"` is not `test` — strings are already blanked — and
    `not(test)` gates *against* test builds, so it does not exempt.
    """
    toks = re.findall(r"[A-Za-z_][A-Za-z0-9_]*|[(),=]", args)

    def parse(i: int) -> tuple[bool, int]:
        name = toks[i]
        i += 1
        if i < len(toks) and toks[i] == "=":
            return False, i + 1
        if i < len(toks) and toks[i] == "(":
            vals = []
            i += 1
            while i < len(toks) and toks[i] != ")":
                if toks[i] == ",":
                    i += 1
                    continue
                val, i = parse(i)
                vals.append(val)
            i += 1  # the ')' itself
            if name == "not":
                return (not vals[0]) if vals else False, i
            return any(vals), i
        return name == "test", i

    if not toks:
        return False
    gated, _ = parse(0)
    return gated


def item_end(san: str, start: int) -> int:
    """End offset of the item starting at `start`: its `;`, or its `}`."""
    depth, i, n = 0, start, len(san)
    while i < n:
        char = san[i]
        if char == ";" and depth == 0:
            return i + 1
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


def module_dir(source: Path) -> Path:
    """The directory a `mod name;` in `source` resolves against."""
    if source.name in ("lib.rs", "main.rs", "mod.rs", "build.rs"):
        return source.parent
    return source.parent / source.stem


def test_exemptions(
    san: str, source: Path
) -> tuple[list[tuple[int, int]], list[Path]]:
    """Spans a `#[cfg(test)]` attribute gates, plus the files behind
    `#[cfg(test)] mod name;` declarations found here."""
    spans: list[tuple[int, int]] = []
    file_mods: list[Path] = []
    for attr in CFG_ATTR.finditer(san):
        open_paren = attr.end() - 1
        depth, i = 1, open_paren + 1
        while i < len(san) and depth:
            if san[i] == "(":
                depth += 1
            elif san[i] == ")":
                depth -= 1
            i += 1
        if not cfg_gates_on_test(san[open_paren + 1 : i - 1]):
            continue
        close = san.find("]", i)
        if close < 0:
            continue
        target = ATTR_TARGET.match(san, close + 1)
        if target is None:
            continue
        keyword, name = target.group(1), target.group(2)
        if keyword != "mod" or name is None:
            spans.append((target.start(1), item_end(san, target.start(1))))
            continue
        sep = re.match(r"\s*([;{])", san[target.end(2) :])
        if sep is None:
            continue
        if sep.group(1) == ";":
            base = module_dir(source)
            file_mods.append(base / f"{name}.rs")
            file_mods.append(base / name / "mod.rs")
        else:
            spans.append((target.start(1), item_end(san, target.end(2) + sep.start())))
    return spans, file_mods


def use_tree_leaves(tree: str) -> list[str]:
    """Names a `pub use` tree binds: `a::{b::C, d as E}` -> `["C", "E"]`.

    `self` binds its parent module — a path segment, not a type — and a glob
    binds no name. Anything the parser cannot walk raises `ValueError` so a
    malformed tree fails loud instead of sliding a leaf past the gate.
    """
    toks = USE_TOKEN.findall(tree)
    if "".join(toks) != "".join(tree.split()):
        raise ValueError(f"unrecognized characters in use tree: {tree!r}")
    leaves: list[str] = []

    def parse(i: int) -> int:
        if toks[i] == "{":
            i += 1
            while i < len(toks) and toks[i] != "}":
                if toks[i] == ",":
                    i += 1
                    continue
                advanced = parse(i)
                if advanced <= i:
                    raise ValueError(f"stalled on {toks[i]!r} in use tree: {tree!r}")
                i = advanced
            if i >= len(toks):
                raise ValueError(f"unclosed brace in use tree: {tree!r}")
            return i + 1
        last = None
        while i < len(toks):
            tok = toks[i]
            if tok == "as":
                if i + 1 >= len(toks) or not IDENT.fullmatch(toks[i + 1]):
                    raise ValueError(f"dangling `as` in use tree: {tree!r}")
                leaves.append(toks[i + 1])
                return i + 2
            if IDENT.fullmatch(tok):
                last = tok
                i += 1
                continue
            if tok == "::":
                i += 1
                if i < len(toks) and toks[i] in ("{", "*"):
                    return parse(i) if toks[i] == "{" else i + 1
                continue
            break
        if last is not None and last != "self":
            leaves.append(last)
        return i

    if toks:
        end = parse(0)
        while end < len(toks) and toks[end] == ",":
            end += 1
        if end != len(toks):
            raise ValueError(f"trailing tokens in use tree: {tree!r}")
    return leaves


def in_spans(pos: int, spans: list[tuple[int, int]]) -> bool:
    return any(start <= pos < end for start, end in spans)


def rust_sources(repo: Path) -> list[Path]:
    files = []
    for root in SOURCE_ROOTS:
        base = repo / root
        if base.is_dir():
            files.extend(sorted(base.rglob("*.rs")))
    return [
        source
        for source in files
        if "tests" not in source.relative_to(repo).parts
        and str(source.relative_to(repo)) not in EXEMPT_FILES
    ]


def violations(repo: Path) -> dict[str, list[str]]:
    """Every host-nameable retired-suffix name: the sites that mint it."""
    sources = rust_sources(repo)
    scanned = {}
    exempt_files = set()
    for source in sources:
        san = blank_noncode(source.read_text(encoding="utf-8"))
        spans, file_mods = test_exemptions(san, source)
        scanned[source] = (san, spans)
        exempt_files.update(file_mods)

    found = {}
    for source in sources:
        if source in exempt_files:
            continue
        san, spans = scanned[source]
        for match in ITEM.finditer(san):
            if in_spans(match.start(), spans):
                continue
            line = san.count("\n", 0, match.start()) + 1
            found.setdefault(match.group(1), []).append(
                f"{source.relative_to(repo)}:{line}"
            )
        for match in PUB_USE.finditer(san):
            if in_spans(match.start(), spans):
                continue
            line = san.count("\n", 0, match.start()) + 1
            site = f"{source.relative_to(repo)}:{line}"
            boundary = san.find(";", match.end())
            if boundary < 0:
                found.setdefault("<unterminated `pub use`>", []).append(site)
                continue
            tree = san[match.end() : boundary]
            try:
                names = use_tree_leaves(tree)
            except ValueError:
                found.setdefault(f"<unparsed `pub use {tree.strip()[:60]}>`", []).append(
                    site
                )
                continue
            for name in names:
                found.setdefault(name, []).append(site)

    return {
        name: sites
        for name, sites in found.items()
        if (name.endswith(RETIRED) and name not in RESULT_ALIASES)
        or name.startswith("<")
    }


def main(repo: Path = REPO) -> int:
    found = violations(repo)
    if not found:
        print("outcome suffixes: no retired-suffix type is host-nameable")
        return 0
    print(
        "ADR 0068: no host-nameable type ends in Result, Disposition, or Summary:",
        file=sys.stderr,
    )
    for name in sorted(found):
        print(f"  {name}", file=sys.stderr)
        for site in found[name]:
            print(f"    {site}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
