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
# The guarded entry is `crates/lash-postgres-store/src/postgres/guarded_tx.rs`:
# `begin_guarded` (BEGIN, then the fence `SELECT format_version FROM
# lash_fleet_format WHERE singleton FOR SHARE` as the first statement),
# `guarded` (the same, retried on contention) and `begin_migration` (a
# migration step's entry, fenced once the catalog can record `F`). Findings
# under `crates/lash-postgres-store/src/`, outside that file:
#
# * a transaction begun anywhere else: `.begin()`, `Connection::begin(...)`
#   or `Acquire::begin(...)`;
# * a statement run straight on a pool, or on a connection checked out of one,
#   whose SQL writes (`INSERT`, `UPDATE ... SET`, `DELETE`, `MERGE`,
#   `TRUNCATE`) or cannot be resolved. A `<set>.<name>.sql()` access resolves
#   `name` through every `lash_store_sql::statements!` block of
#   `crates/lash-store-sql` and the PostgreSQL store; a name any block spells as
#   a write counts as one;
# * a guarded transaction that sets its isolation level: PostgreSQL takes that
#   only as a transaction's first statement, and the fence already is, so a
#   snapshot transaction is a read and stays unguarded.
#
# Test code is out of scope: files a `#[cfg(test)]` or `testing`-gated `mod`
# declares, and inline modules gated the same way. String literals are kept,
# because SQL literals are part of what this section reads.

_STRING_START = re.compile(r'(b?r#*")|(b?")')


def strip_comments(source: str) -> str:
    """`source` with every comment blanked and every newline kept.

    String and raw-string literals are kept verbatim: SQL literals are part
    of what the check reads. Character literals and lifetimes need no care
    for the comment markers this looks for.
    """

    out: list[str] = []
    index = 0
    length = len(source)
    while index < length:
        char = source[index]
        if source.startswith("//", index):
            end = source.find("\n", index)
            end = length if end == -1 else end
            out.append(" " * (end - index))
            index = end
            continue
        if source.startswith("/*", index):
            depth = 0
            start = index
            while index < length:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                    if depth == 0:
                        break
                else:
                    index += 1
            out.append("".join(c if c == "\n" else " " for c in source[start:index]))
            continue
        if char == "'" and index + 2 < length and source[index + 2] == "'":
            out.append(source[index : index + 3])
            index += 3
            continue
        if char == "'" and source.startswith("'\\", index):
            end = source.find("'", index + 2)
            end = length if end == -1 else end + 1
            out.append(source[index:end])
            index = end
            continue
        match = _STRING_START.match(source, index)
        if match and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_")):
            raw = match.group(1)
            if raw:
                hashes = raw.count("#")
                terminator = '"' + "#" * hashes
                end = source.find(terminator, index + len(raw))
                end = length if end == -1 else end + len(terminator)
            else:
                end = index + len(match.group(2))
                while end < length and source[end] != '"':
                    end += 2 if source[end] == "\\" else 1
                end = min(end + 1, length)
            out.append(source[index:end])
            index = end
            continue
        out.append(char)
        index += 1
    return "".join(out)


def string_literals(text: str) -> list[str]:
    """The contents of every string literal in `text`."""

    found: list[str] = []
    for match in re.finditer(r'r(#*)"(.*?)"\1|"((?:[^"\\]|\\.)*)"', text, re.S):
        found.append(match.group(2) if match.group(2) is not None else match.group(3))
    return found


_TEST_CFG = re.compile(
    r"#\[cfg\((?:test|any\(test,\s*feature\s*=\s*\"testing\"\)|feature\s*=\s*\"testing\")\)\]"
)


def _attribute_run_before(text: str, position: int) -> str:
    """The attributes stacked directly above the item starting at `position`."""

    lines = text[:position].split("\n")
    attributes: list[str] = []
    for line in reversed(lines[:-1]):
        stripped = line.strip()
        if stripped.startswith("#[") or stripped.startswith("///") or not stripped:
            attributes.append(stripped)
            if not stripped:
                break
            continue
        break
    return "\n".join(attributes)


def matching_brace(text: str, open_index: int) -> int:
    """The index of the brace closing the one at `open_index`."""

    depth = 0
    index = open_index
    length = len(text)
    while index < length:
        char = text[index]
        if char == '"' or (char == "r" and re.match(r'r#*"', text[index:index + 8] or "")):
            match = re.match(r'r(#*)"', text[index:])
            if match and (index == 0 or not (text[index - 1].isalnum() or text[index - 1] == "_")):
                terminator = '"' + match.group(1)
                end = text.find(terminator, index + len(match.group(0)))
                index = len(text) if end == -1 else end + len(terminator)
                continue
            if char == '"':
                index += 1
                while index < length and text[index] != '"':
                    index += 2 if text[index] == "\\" else 1
                index += 1
                continue
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return length - 1


def blank_test_modules(text: str) -> str:
    """`text` with every test-gated inline `mod name { ... }` blanked."""

    for match in reversed(list(re.finditer(r"\bmod\s+\w+\s*\{", text))):
        if not _TEST_CFG.search(_attribute_run_before(text, _line_start(text, match.start()))):
            continue
        close = matching_brace(text, match.end() - 1)
        segment = text[match.start() : close + 1]
        text = text[: match.start()] + re.sub(r"[^\n]", " ", segment) + text[close + 1 :]
    return text


def _line_start(text: str, position: int) -> int:
    return text.rfind("\n", 0, position) + 1


def test_only_module_files(crate_src: Path) -> set[Path]:
    """Files a test- or `testing`-gated `mod name;` declares, transitively."""

    excluded: set[Path] = set()
    pending = [crate_src / "lib.rs"]
    seen: set[Path] = set()
    while pending:
        path = pending.pop()
        if path in seen or not path.is_file():
            continue
        seen.add(path)
        text = strip_comments(path.read_text())
        for match in re.finditer(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", text, re.M):
            attributes = _attribute_run_before(text, match.start())
            explicit = re.search(r'#\[path\s*=\s*"([^"]+)"\]', attributes)
            directory = path.parent if path.name in ("lib.rs", "mod.rs") else path.with_suffix("")
            if explicit:
                target = (path.parent / explicit.group(1)).resolve()
            else:
                candidate = directory / f"{match.group(1)}.rs"
                target = candidate if candidate.exists() else directory / match.group(1) / "mod.rs"
            if _TEST_CFG.search(attributes) or path.resolve() in excluded:
                excluded.add(target.resolve())
            pending.append(target)
    # A module file's own children are declared relative to it.
    return excluded


_FN = re.compile(r"\bfn\s+(\w+)")


def enclosing_functions(text: str) -> list[str]:
    """For each line (0-based), the innermost function it sits in, or ''."""

    spans: list[tuple[int, int, str]] = []
    for match in _FN.finditer(text):
        brace = text.find("{", match.end())
        semicolon = text.find(";", match.end())
        if brace == -1 or (semicolon != -1 and semicolon < brace):
            continue
        close = matching_brace(text, brace)
        spans.append((match.start(), close, match.group(1)))
    line_offsets = [0]
    for index, char in enumerate(text):
        if char == "\n":
            line_offsets.append(index + 1)
    names: list[str] = []
    for offset in line_offsets:
        best = ""
        best_start = -1
        for start, end, name in spans:
            if start <= offset <= end and start > best_start:
                best, best_start = name, start
        names.append(best)
    return names


POSTGRES_SRC = "crates/lash-postgres-store/src"
POSTGRES_GUARD = "crates/lash-postgres-store/src/postgres/guarded_tx.rs"
STATEMENT_ROOTS = ("crates/lash-store-sql/src", POSTGRES_SRC)

_WRITE_SQL = re.compile(
    r"\b(INSERT\s+INTO|UPDATE\s+\w+\s+(?:AS\s+\w+\s+)?SET|DELETE\s+FROM|MERGE\s+INTO|TRUNCATE)\b",
    re.I,
)
_BEGIN = re.compile(r"\.begin\(\s*\)|\b(?:Connection|Acquire)::begin\s*\(")
_RUN = re.compile(r"\.(execute|fetch_one|fetch_optional|fetch_all|fetch)\(\s*")
# A pool, or a connection checked out of one: autocommit, so a write there is
# a transaction of its own.
_POOL_RECEIVER = re.compile(
    r"^&?\*?(?:[\w.]+\.)?pool(?:\(\))?$|^&?\*?[\w.]*storage\.pool\(\)$"
)
_CONNECTION_RECEIVER = re.compile(r"^(?:&mut\s*\*?\s*connection|connection\.as_mut\(\))$")
_ISOLATION = re.compile(r"SET\s+TRANSACTION|\.begin_repeatable_read")
_STATEMENTS_BLOCK = re.compile(r"statements!\s*\{")
_STATEMENT = re.compile(r"(\w+)\s*=\s*(r#*\"|\")", re.S)
_NAMED_SQL = re.compile(r"\.(\w+)\s*\.sql\(\)")
_ROLE_BINDING = re.compile(r"\b(\w+):\s*&self\.(\w+),")


def statement_writes(root: Path) -> dict[str, bool]:
    """Every `statements!` name, and whether any block spells it as a write."""

    writes: dict[str, bool] = {}
    for base in STATEMENT_ROOTS:
        for path in sorted((root / base).rglob("*.rs")):
            text = strip_comments(path.read_text())
            for block in _STATEMENTS_BLOCK.finditer(text):
                close = matching_brace(text, block.end() - 1)
                body = text[block.end() : close]
                index = 0
                while True:
                    match = _STATEMENT.search(body, index)
                    if not match:
                        break
                    literal_start = match.start(2)
                    literal = string_literals(body[literal_start:])
                    sql = literal[0] if literal else ""
                    name = match.group(1)
                    writes[name] = writes.get(name, False) or bool(_WRITE_SQL.search(sql))
                    index = literal_start + len(sql) + 2
    # A role struct (`ObligationSql`) re-exposes a set's statements under role
    # names: `claim: &self.obligation_claim`. The role name writes when any
    # statement bound to it does.
    for base in STATEMENT_ROOTS:
        for path in sorted((root / base).rglob("*.rs")):
            for role, statement in _ROLE_BINDING.findall(strip_comments(path.read_text())):
                if statement in writes:
                    writes[role] = writes.get(role, False) or writes[statement]
    return writes


def _call_argument(text: str, open_paren: int) -> tuple[str, int]:
    depth = 0
    index = open_paren
    while index < len(text):
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth == 0:
                return text[open_paren + 1 : index].strip(), index
        index += 1
    return text[open_paren + 1 :].strip(), len(text)


def _statement_start(text: str, position: int) -> int:
    """Where the Rust statement holding `position` starts."""

    depth = 0
    index = position - 1
    while index >= 0:
        char = text[index]
        if char in ")]":
            depth += 1
        elif char in "([":
            if depth == 0:
                return index + 1
            depth -= 1
        elif char == "}":
            depth += 1
        elif char == "{":
            if depth == 0:
                return index + 1
            depth -= 1
        elif char == ";" and depth == 0:
            return index + 1
        index -= 1
    return 0


def _statement_sql_writes(
    text: str, start: int, end: int, local_sql: dict[str, str], writes: dict[str, bool]
) -> bool | None:
    """Whether the statement run in `text[start:end]` writes; None if unknown."""

    segment = text[start:end]
    names = _NAMED_SQL.findall(segment)
    literals = [literal for literal in string_literals(segment) if literal.strip()]
    known = False
    for name in names:
        if name in writes:
            known = True
            if writes[name]:
                return True
    for literal in literals:
        if re.match(r"\s*(SELECT|WITH|INSERT|UPDATE|DELETE|MERGE|TRUNCATE|SET)\b", literal, re.I):
            known = True
            if _WRITE_SQL.search(literal):
                return True
    for variable in re.findall(r"\bquery(?:_as|_scalar)?(?:::<[^>]*>)?\(\s*(\w+)\s*\)", segment):
        if variable in local_sql:
            known = True
            if _WRITE_SQL.search(local_sql[variable]) or local_sql[variable] == "WRITE":
                return True
    if known:
        return False
    return None


def _local_sql(text: str, function_start: int, position: int, writes: dict[str, bool]) -> dict[str, str]:
    """`let name = <sql>` bindings above `position` in the same function."""

    bindings: dict[str, str] = {}
    for match in re.finditer(r"\blet\s+(?:mut\s+)?(\w+)\s*(?::[^=]+)?=\s*([^;]+);", text[function_start:position]):
        value = match.group(2)
        literal = string_literals(value)
        if literal:
            bindings[match.group(1)] = literal[0]
            continue
        for name in _NAMED_SQL.findall(value) or re.findall(r"&\s*[\w.()]*\.(\w+)\s*$", value.strip()):
            if name in writes:
                bindings[match.group(1)] = "WRITE" if writes[name] else "SELECT"
    # A `let query = sqlx::query(...)` binding: the query builder itself.
    return bindings


def check_postgres(root: Path = ROOT) -> list[Site]:
    src = root / POSTGRES_SRC
    excluded = test_only_module_files(src)
    writes = statement_writes(root)
    findings: list[Site] = []
    for path in sorted(src.rglob("*.rs")):
        relative = path.relative_to(root).as_posix()
        if path.resolve() in excluded or relative == POSTGRES_GUARD:
            continue
        text = blank_test_modules(strip_comments(path.read_text()))
        functions = enclosing_functions(text)
        for match in _BEGIN.finditer(text):
            line = line_of(text, match.start())
            findings.append(
                Site(relative, functions[line - 1] or "<module>", line, "transaction begun outside the guarded entry")
            )
        for match in _ISOLATION.finditer(text):
            function_start = _function_start(text, match.start())
            if "begin_guarded(" in text[function_start : match.start()]:
                line = line_of(text, match.start())
                findings.append(
                    Site(
                        relative,
                        functions[line - 1] or "<module>",
                        line,
                        "isolation level set after the fence: it must be a transaction's first "
                        "statement, so a snapshot transaction is a read and stays unguarded",
                    )
                )
        for match in _RUN.finditer(text):
            receiver, close = _call_argument(text, match.end() - 1)
            receiver = re.sub(r"\s+", " ", receiver)
            line = line_of(text, match.start())
            function = functions[line - 1] or "<module>"
            function_start = _function_start(text, match.start())
            on_pool = bool(_POOL_RECEIVER.match(receiver))
            on_connection = bool(_CONNECTION_RECEIVER.match(receiver)) and bool(
                re.search(r"acquire_runtime_connection\(|\.acquire\(\)", text[function_start : match.start()])
            )
            if not (on_pool or on_connection):
                continue
            start = _statement_start(text, match.start())
            verdict = _statement_sql_writes(
                text, start, match.start(), _local_sql(text, function_start, start, writes), writes
            )
            if verdict is False:
                continue
            what = "a pool" if on_pool else "a checked-out connection"
            reason = (
                f"mutating statement run straight on {what}"
                if verdict
                else f"statement with unresolved SQL run straight on {what}"
            )
            findings.append(Site(relative, function, line, reason))
    return findings


def _function_start(text: str, position: int) -> int:
    best = 0
    for match in _FN.finditer(text, 0, position):
        brace = text.find("{", match.end())
        if brace != -1 and brace < position and matching_brace(text, brace) >= position:
            best = match.start()
    return best


SECTIONS = {
    "sqlite": check_sqlite,
    "postgres": check_postgres,
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
