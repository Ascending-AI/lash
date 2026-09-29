#!/usr/bin/env python3
"""Keep every mutating store transaction behind the writer fence (ADR 0115 §2.2).

Every mutation checks the fleet epoch `F` inside its own transaction, as that
transaction's first statement, so a writer whose release a newer one has
finalized is refused `WriterFenced` having written nothing. The fence lives in
one guarded transaction entry per backend. This lint keeps that entry total:
a transaction opened anywhere else, or a mutating statement run outside any
transaction, fails the check unless `scripts/guarded-transaction-readonly.txt`
lists its site with the reason it only reads.

Allowlist lines are `<backend> <path>::<fn>  # <why it only reads>`, keyed by
the enclosing function so the exemption names the code it justifies. An entry
that no longer matches a flagged site fails the check too.

Each backend is its own section below. Test code (`#[cfg(test)]` items and the
crate's test modules) is not production and is not checked.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
ALLOWLIST = ROOT / "scripts/guarded-transaction-readonly.txt"


@dataclass(frozen=True)
class Site:
    path: str
    function: str
    line: int
    what: str

    @property
    def key(self) -> str:
        return f"{self.path}::{self.function}"


def load_allowlist() -> dict[str, dict[str, str]]:
    """`{backend: {path::fn: reason}}`; a line without a reason is an error."""
    entries: dict[str, dict[str, str]] = {}
    problems = []
    for number, raw in enumerate(ALLOWLIST.read_text().splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        body, _, reason = line.partition("#")
        fields = body.split()
        if len(fields) != 2 or not reason.strip():
            problems.append(f"{ALLOWLIST.name}:{number}: want `<backend> <path>::<fn>  # <reason>`")
            continue
        backend, key = fields
        entries.setdefault(backend, {})[key] = reason.strip()
    if problems:
        sys.exit("\n".join(problems))
    return entries


# --- Rust source helpers ------------------------------------------------------

CFG_TEST_ITEM = re.compile(r"#\s*\[\s*cfg\s*\(\s*(?:test|all\s*\(\s*test\b[^\]]*\))\s*\)\s*\]")
ITEM_START = re.compile(
    r"\s*(?:#\s*\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:mod|fn|impl|use|const|static|struct|enum|trait|type|async|unsafe|macro_rules)\b"
)
FN_HEADER = re.compile(r"\bfn\s+(\w+)\s*(?:<[^{;]*?>)?\s*\(")
RAW_STRING = re.compile(r'r(#*)"')
CHAR_LITERAL = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]*\}|.)|[^\\'\n])'")


def masked(text: str) -> str:
    """`text` with comments, string literals and char literals blanked to
    spaces (newlines kept), so brackets and identifiers are code only."""
    out = list(text)
    i = 0
    while i < len(text):
        c = text[i]
        raw = None
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = len(text) if end < 0 else end
        elif text.startswith("/*", i):
            end = text.find("*/", i) + 2
        elif (
            c == "r"
            and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_"))
            and (raw := RAW_STRING.match(text, i))
        ):
            closing = '"' + raw.group(1)
            end = text.find(closing, raw.end()) + len(closing)
        elif c == '"':
            j = i + 1
            while text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            end = j + 1
        elif c == "'" and (char := CHAR_LITERAL.match(text, i)):
            end = char.end()
        else:
            i += 1
            continue
        for j in range(i, end):
            if out[j] != "\n":
                out[j] = " "
        i = end
    return "".join(out)


def bracket_pairs(code: str) -> dict[int, int]:
    """Each opening bracket's offset, mapped to the offset just past its closer."""
    pairs: dict[int, int] = {}
    stack: list[int] = []
    for i, c in enumerate(code):
        if c in "({[":
            stack.append(i)
        elif c in ")}]" and stack:
            pairs[stack.pop()] = i + 1
    return pairs


@dataclass
class Source:
    path: str
    text: str
    code: str
    pairs: dict[int, int]


def source(path: str, text: str) -> Source:
    """A file with its `#[cfg(test)]` items blanked (line numbers kept), and
    its code-only view."""
    code = masked(text)
    while match := CFG_TEST_ITEM.search(code):
        if ITEM_START.match(code, match.end()):
            pairs = bracket_pairs(code)
            brace = code.find("{", match.end())
            semicolon = code.find(";", match.end())
            end = semicolon + 1 if brace < 0 or 0 <= semicolon < brace else pairs[brace]
        else:
            # A field, an argument or a statement: it ends at its comma or
            # its line.
            end = min(
                offset
                for offset in (code.find(",", match.end()), code.find("\n", match.end()), len(code))
                if offset >= 0
            )
        blank = re.sub(r"[^\n]", " ", code[match.start() : end])
        code = code[: match.start()] + blank + code[end:]
        text = text[: match.start()] + blank + text[end:]
    return Source(path, text, code, bracket_pairs(code))


@dataclass
class Function:
    name: str
    body_start: int
    body_end: int


def functions(src: Source) -> list[Function]:
    found = []
    for match in FN_HEADER.finditer(src.code):
        close = src.pairs.get(match.end() - 1)
        if close is None:
            continue
        brace = src.code.find("{", close)
        semicolon = src.code.find(";", close)
        if brace < 0 or 0 <= semicolon < brace:
            continue
        found.append(Function(match.group(1), brace, src.pairs[brace]))
    return found


def enclosing(fns: list[Function], offset: int) -> str:
    inner = [fn for fn in fns if fn.body_start <= offset < fn.body_end]
    if not inner:
        return "<module>"
    return min(inner, key=lambda fn: fn.body_end - fn.body_start).name


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


# --- SQLite (lane L3) ---------------------------------------------------------
#
# The guarded entry is `SqliteConnection::write` / `write_flow`
# (crates/lash-sqlite-store/src/conn.rs): `BEGIN IMMEDIATE`, then the fence
# (`compat::fence`) as the first statement. Two other entries open write
# transactions by design and are named here rather than allowlisted, because
# they are not read-only:
#
# * `SqliteConnection::install`, the installer: the one write before the
#   database's `lash_compat` row exists. It admits or provisions the stamp and
#   `F` itself, inside the same transaction, and arms the fence.
# * `compat::advance_set_observed`, a migration or finalize: it holds every
#   database under `BEGIN EXCLUSIVE`, so no writer runs beside it.
#
# A rusqlite transaction opened anywhere else, a raw connection opened outside
# the wrapper, or a mutating statement run in autocommit through
# `SqliteConnection::call` fails the check unless allowlisted as read-only.
# `SqliteConnection::read` closures are not checked: `read` always rolls back.

SQLITE_SOURCE = ROOT / "crates/lash-sqlite-store/src"
SQLITE_TEST_FILES = re.compile(r"(^|/)(tests?|test_support|testing|\w+_tests)\.rs$|(^|/)tests/")
SQLITE_GUARDS = {
    "crates/lash-sqlite-store/src/conn.rs::write_flow",
    "crates/lash-sqlite-store/src/conn.rs::install",
    "crates/lash-sqlite-store/src/compat.rs::advance_set_observed",
}
SQLITE_TRANSACTION_CALL = re.compile(
    r"\.\s*(?:transaction_with_behavior|unchecked_transaction|transaction|savepoint(?:_with_name)?)\s*\("
)
SQLITE_TRANSACTION_SQL = re.compile(r"\"\s*(?:BEGIN|SAVEPOINT|COMMIT|END)\b")
SQLITE_RAW_CONNECTION = re.compile(r"\bConnection::open(?:_with_flags|_in_memory)?\s*\(")
SQLITE_WRITE_VERB = re.compile(
    r"\.\s*(?:execute|execute_batch|raw_execute)\s*\(|\bcached_execute\s*\("
)
SQLITE_CALL = re.compile(r"\.\s*call\s*\(")
# A free or path call (`helper(`, `Self::helper(`, `crate::m::helper(`), not a
# method call: the closures `call` runs reach crate code only through these.
CALLEE = re.compile(r"(?<![.\w:])((?:\w+::)*)(\w+)\s*(?:::<[^>]*>)?\s*\(")


def sqlite_sources() -> list[Source]:
    sources = []
    for path in sorted(SQLITE_SOURCE.rglob("*.rs")):
        if SQLITE_TEST_FILES.search(path.relative_to(SQLITE_SOURCE).as_posix()):
            continue
        sources.append(source(path.relative_to(ROOT).as_posix(), path.read_text()))
    return sources


Node = tuple[str, str]


IMPL = re.compile(r"\bimpl\b(?:\s*<[^{]*?>)?\s+(?:[\w:<>, ]+?\s+for\s+)?(?:[\w]+::)*(\w+)")


@dataclass
class Crate:
    """Where each crate function is defined, and which types each file
    implements, so a call can be resolved to the definitions it may reach."""

    defined: dict[str, set[str]]
    impls: dict[str, set[str]]


def resolve(path: str, qualifier: str, name: str, crate: Crate) -> set[Node]:
    """The crate functions a call may reach.

    `crate::m::f` and `super::m::f` resolve into module `m`; `Type::f` into
    the files that implement `Type`; `Self::f` and a bare `f` into the calling
    file when it defines `f`, else anywhere. A lowercase path that names no
    crate module, or that names the calling file's own module (which calls
    itself without a path), is another crate's.
    """
    files = crate.defined.get(name, set())
    parts = [part for part in qualifier.split("::") if part]
    if parts and parts[0] in ("crate", "super"):
        parts = parts[1:]
        if not parts:
            return {(file, name) for file in files}
        module = f"crates/lash-sqlite-store/src/{parts[0]}"
        files = {file for file in files if file == f"{module}.rs" or file.startswith(f"{module}/")}
    elif not parts or parts == ["Self"]:
        if path in files:
            files = {path}
    elif parts[-1][:1].isupper():
        files = {file for file in files if parts[-1] in crate.impls.get(file, set())}
    else:
        module = f"crates/lash-sqlite-store/src/{parts[0]}"
        if path == f"{module}.rs" or path.startswith(f"{module}/"):
            return set()
        files = {file for file in files if file == f"{module}.rs" or file.startswith(f"{module}/")}
    return {(file, name) for file in files}


def sqlite_writing_functions(sources: list[Source]) -> set[Node]:
    """The crate functions, as `(file, name)`, that run a mutating statement
    directly or through another crate function. A call that may reach
    several definitions reaches all of them, which only makes the check
    stricter."""
    crate = sqlite_crate(sources)
    bodies: list[tuple[Node, str]] = []
    for src in sources:
        for fn in functions(src):
            bodies.append(((src.path, fn.name), src.code[fn.body_start : fn.body_end]))
    calls: dict[Node, set[Node]] = {}
    writing: set[Node] = set()
    for node, body in bodies:
        for qualifier, name in CALLEE.findall(body):
            calls.setdefault(node, set()).update(resolve(node[0], qualifier, name, crate))
        if SQLITE_WRITE_VERB.search(body):
            writing.add(node)
    while grown := {
        node for node, callees in calls.items() if node not in writing and callees & writing
    }:
        writing |= grown
    return writing


def sqlite_crate(sources: list[Source]) -> Crate:
    crate = Crate({}, {})
    for src in sources:
        crate.impls[src.path] = set(IMPL.findall(src.code))
        for fn in functions(src):
            crate.defined.setdefault(fn.name, set()).add(src.path)
    return crate


def check_sqlite() -> list[Site]:
    sources = sqlite_sources()
    crate = sqlite_crate(sources)
    writing = sqlite_writing_functions(sources)
    flagged = []
    for src in sources:
        fns = functions(src)

        def site(offset: int, what: str) -> Site:
            return Site(src.path, enclosing(fns, offset), line_of(src.text, offset), what)

        for match in SQLITE_TRANSACTION_CALL.finditer(src.code):
            flagged.append(site(match.start(), f"opens a transaction: `{match.group(0)}`"))
        # Transaction SQL is spelled in a string literal, so this rule reads
        # the text, skipping what `#[cfg(test)]` blanked.
        for match in SQLITE_TRANSACTION_SQL.finditer(src.text):
            flagged.append(site(match.start(), f"runs transaction SQL: `{match.group(0)}`"))
        for match in SQLITE_RAW_CONNECTION.finditer(src.code):
            flagged.append(site(match.start(), "opens a connection outside SqliteConnection"))
        for match in SQLITE_CALL.finditer(src.code):
            opening = match.end() - 1
            closure = src.code[opening : src.pairs.get(opening, opening)]
            direct = SQLITE_WRITE_VERB.search(closure)
            via = sorted(
                {
                    name
                    for qualifier, name in CALLEE.findall(closure)
                    if resolve(src.path, qualifier, name, crate) & writing
                }
            )
            if direct or via:
                reason = f"`{direct.group(0)}`" if direct else f"via {', '.join(via)}"
                flagged.append(
                    site(match.start(), f"mutates in autocommit through `call` ({reason})")
                )
    return [site for site in flagged if site.key not in SQLITE_GUARDS]


# --- PostgreSQL (lane L2) -----------------------------------------------------
#
# Lane L2 adds `check_postgres` here and registers it in `SECTIONS`.


SECTIONS = {
    "sqlite": check_sqlite,
}


def main() -> int:
    allowlist = load_allowlist()
    problems = []
    for backend, check in SECTIONS.items():
        allowed = allowlist.get(backend, {})
        flagged = check()
        for site in flagged:
            if site.key not in allowed:
                problems.append(f"{site.path}:{site.line}: {site.function}: {site.what}")
        stale = sorted(set(allowed) - {site.key for site in flagged})
        problems += [f"{ALLOWLIST.name}: {backend} {key} matches no flagged site" for key in stale]
    unknown = sorted(set(allowlist) - set(SECTIONS))
    problems += [f"{ALLOWLIST.name}: no section checks backend `{backend}`" for backend in unknown]
    if problems:
        print("unguarded store mutation sites (ADR 0115 §2.2):", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        print(
            "route the mutation through the backend's guarded entry, or list a read-only "
            f"site in {ALLOWLIST.relative_to(ROOT)} with its reason",
            file=sys.stderr,
        )
        return 1
    print("guarded transactions: every store mutation site is behind the writer fence")
    return 0


if __name__ == "__main__":
    sys.exit(main())
