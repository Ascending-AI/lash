#!/usr/bin/env python3
"""Require version bumps when versioned wire or persistence shapes change.

The guarded surfaces live in ``scripts/versioned-surfaces.toml``.  Each guard
compares a deliberately narrow projection of the merge-base tree with the head
tree; a changed projection requires the head's version constant to be strictly
greater than the merge-base value.

For pull requests, this guarantee assumes CI checks a current GitHub merge ref
whose target-branch parent is the latest protected-branch tip. Repositories must
enforce an up-to-date branch or merge queue; a stale merge ref can produce an
older merge-base and cannot prove that independently-landed bumps stay ordered.

Registering a surface -- enrolling a shape that may move in the same change,
with no base version to be strictly greater than -- is a burned one-time
baseline in ``REGISTRATION_BASELINES``, never a category the check infers.  An
inferred category would read a renamed or relocated constant as brand new and
silently un-guard a live surface for one change; a pinned key and fingerprint
cannot be reached by any refactor.

Renaming identifiers across a guarded surface without moving the format it
versions is the other burned one-time baseline, ``IDENTIFIER_RENAME_BASELINES``,
pinned the same way: the guards project guarded text, so a retyped variant reads
as a shape change even when the serialized bytes are identical, and only a
reviewer can say which it was.

Only the Python standard library is used so the check can run before the Rust
toolchain is installed.  Pull-request CI passes the PR merge-base explicitly.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import fnmatch
import hashlib
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Iterable


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG = Path(__file__).with_name("versioned-surfaces.toml")

# Burned one-time proofs that a surface is being registered rather than
# changed. A registration has no merge-base version to be strictly greater
# than, so it is excused from the bump it would otherwise owe -- and that
# excuse is granted only to these exact surface keys carrying these exact
# guarded bytes, never to a category the check infers.
#
# Inference was the earlier design and it was wrong: it asked whether the
# surface key was absent from the merge-base inventory, which is also true of a
# constant someone renamed or moved to another file. An innocent refactor
# therefore looked identical to a registration and un-guarded the surface for
# that change. A pinned key plus a fingerprint of the guarded shape at the
# moment of enrolment answers the real question instead -- "is this the one
# enrolment we reviewed?" -- and no rename can answer it yes.
#
# The fingerprint covers the surface's whole head guard signature: every
# guarded path and symbol, trivia-stripped, in the order the guards declare
# them. A failing check prints the fingerprint it computed, so burning a new
# registration is a deliberate, diffable act by a human who read the shape.
# Entries stay after the surface lands; they are dead-but-honest history, and
# re-adding a removed entry over a live constant is not a registration.
REGISTRATION_BASELINES = {
    # FIG-1529: enrolment of the durable graph-node body, whose already-current
    # shape gained its schema_version stamp in the same change.
    "crates/lash-core/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION": (
        "sha256:bcb22b869fe0b86eb9a507b5f1841b733360cfba1164a407d189648998cf1dc9"
    ),
}

# Burned one-time proofs that a change moved Rust identifiers across a guarded
# surface without moving the format the surface versions. The guards project
# the TEXT of the guarded items, identifiers included, so a rename of a type or
# a variant reads as a shape change even when every serde name, wire tag, and
# emitted fingerprint string is byte-identical on both sides -- and a version
# bump for that would publish a false incompatibility to peers and stored data.
#
# The exemption is granted the same way a registration is, and for the same
# reason: only to these exact surface keys carrying these exact guarded bytes,
# pinned to the head signature the reviewer read. There is no inferred
# "identifier-only" category and there must never be one, because the check
# cannot see whether a rename reached the serialized bytes; a human reads that
# and burns the answer here. Entries stay after the change lands as
# dead-but-honest history.
IDENTIFIER_RENAME_BASELINES = {
    # FIG-1036, one time only: the outcome-suffix vocabulary rename retyped
    # Rust identifiers across these three surfaces while leaving every serde
    # field name, variant name, and emitted fingerprint tag byte-identical, so
    # REMOTE_PROTOCOL_VERSION stays 41, SESSION_NODE_BODY_SCHEMA_VERSION stays
    # 1, and PROCESS_REGISTRATION_FAMILY_VERSION stays 4.
    # Known residual, inherited from REGISTRATION_BASELINES and equally narrow:
    # the baseline pins a STATE, not a transition, so a future change that
    # restores the guarded text to exactly these bytes would re-match and be
    # excused a second time.
    "crates/lash-remote-protocol/src/lib.rs:REMOTE_PROTOCOL_VERSION": (
        "sha256:35bf1df42914317cac344d2c979fad7ba889e034abf8472f3c3f95ea5729d328"
    ),
    "crates/lash-core/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION": (
        "sha256:6b74366597e750eba28ae339771f9641f15bc78295cb3fcc00712d3e89899d02"
    ),
    "crates/lash-core/src/runtime/process/validation.rs:"
    "PROCESS_REGISTRATION_FAMILY_VERSION": (
        "sha256:fd2a5cd3cc916b265166f95f896524883c047382634e959d152df48e860f9ed7"
    ),
    # FIG-1623: structural envelope hoist; identical key set/values, object key
    # order changed (kind first→third); key order ruled outside the trace wire
    # contract, so no schema bump is owed.
    "crates/lash-trace/src/lib.rs:TRACE_SCHEMA_VERSION": (
        "sha256:587b0088e2344437aeeff0446339baca01ef0a2f3fa5b054f41581717eda7e0a"
    ),
}


@dataclass(frozen=True)
class Guard:
    kind: str
    paths: tuple[str, ...]
    symbols: tuple[str, ...] = ()
    must_cover: tuple[str, ...] = ()
    elide: str | None = None


# A non-unique `CREATE INDEX IF NOT EXISTS ... ;` statement, whole.
#
# In a catalog whose open path always executes the entire schema text, such a
# statement is not a compatibility boundary: it reaches an existing database on
# its next open, and a database that already has the index stays readable by a
# binary whose schema omits it. Both directions are therefore compatible on one
# file, which is why a guarded surface may declare `elide` and let an
# index-only addition ship without a version bump.
#
# Elision applies only to statements that introduce NEW index names relative to
# the base revision. A statement whose index name already exists on the base side
# is a modification, not an addition: it is not elided, so altering an existing
# index's definition (such as its column list) demands a version bump. Existing
# index statements on the base side are also kept in the base signature, so
# removing an index deliberately demands a version bump. `UNIQUE` is deliberately
# not matched: a unique index is a constraint, `IF NOT EXISTS` will not
# re-create a differently-shaped one, and it must keep demanding a bump.
IDEMPOTENT_SQL_INDEX = re.compile(
    r"\s*CREATE\s+INDEX\s+IF\s+NOT\s+EXISTS\s+([^\s(]+)\s+ON\b.*?;",
    re.DOTALL | re.IGNORECASE,
)


def normalize_sql_index_name(name: str) -> str:
    return name.strip('"`[]').lower()


def extract_idempotent_sql_index_names(text: str) -> set[str]:
    return {
        normalize_sql_index_name(match.group(1))
        for match in IDEMPOTENT_SQL_INDEX.finditer(text)
    }


def elide_new_sql_indexes(head_value: str, base_value: str = "") -> str:
    base_indexes = extract_idempotent_sql_index_names(base_value)

    def replacer(match: re.Match[str]) -> str:
        name = normalize_sql_index_name(match.group(1))
        if name not in base_indexes:
            return ""
        return match.group(0)

    return IDEMPOTENT_SQL_INDEX.sub(replacer, head_value)


ELISIONS = {
    "sql_idempotent_index": elide_new_sql_indexes,
}


@dataclass(frozen=True)
class Surface:
    constant: str
    constant_path: str
    description: str
    guards: tuple[Guard, ...]
    version_regex: str | None = None

    @property
    def key(self) -> str:
        return f"{self.constant_path}:{self.constant}"


@dataclass(frozen=True)
class Failure:
    surface: Surface
    base_version: int
    head_version: int
    changed_guards: tuple[str, ...]
    fingerprint: str = ""


@dataclass(frozen=True)
class SurfaceError:
    surface: Surface
    detail: str


@dataclass(frozen=True)
class Unregistered:
    """A changed shape whose constant has no merge-base value and no baseline.

    This is what a renamed or relocated version constant looks like from the
    gate's side, so it is reported as a failure rather than an undecidable
    surface: the shape moved, and nothing in the inventory can say what it was
    supposed to move past.
    """

    surface: Surface
    changed_guards: tuple[str, ...]
    fingerprint: str


@dataclass(frozen=True)
class CheckResult:
    failures: tuple[Failure, ...]
    errors: tuple[SurfaceError, ...]
    registrations: tuple[Surface, ...] = ()
    unregistered: tuple[Unregistered, ...] = ()
    identifier_renames: tuple[Surface, ...] = ()


class CheckError(RuntimeError):
    """Configuration, repository, or source-shape error."""


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True
    )
    if check and result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise CheckError(f"git {' '.join(args)} failed: {detail}")
    return result


def resolve_revision(repo: Path, revision: str) -> str:
    return git(repo, "rev-parse", "--verify", f"{revision}^{{commit}}").stdout.strip()


def load_config(path: Path) -> tuple[Surface, ...]:
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise CheckError(f"cannot read {path}: {error}") from error

    raw_surfaces = document.get("surface")
    if not isinstance(raw_surfaces, list) or not raw_surfaces:
        raise CheckError(f"{path}: expected at least one [[surface]] entry")

    surfaces: list[Surface] = []
    seen: set[tuple[str, str]] = set()
    for index, raw_surface in enumerate(raw_surfaces, start=1):
        location = f"{path}: surface {index}"
        if not isinstance(raw_surface, dict):
            raise CheckError(f"{location} must be a table")
        try:
            constant = raw_surface["constant"]
            constant_path = raw_surface["constant_path"]
            description = raw_surface["description"]
            raw_guards = raw_surface["guard"]
        except KeyError as error:
            raise CheckError(f"{location} is missing {error.args[0]}") from error
        if not all(
            isinstance(value, str) and value
            for value in (constant, constant_path, description)
        ):
            raise CheckError(f"{location} has an empty or non-string required field")
        identity = (constant_path, constant)
        if identity in seen:
            raise CheckError(f"{location} duplicates {constant} in {constant_path}")
        seen.add(identity)
        if not isinstance(raw_guards, list) or not raw_guards:
            raise CheckError(f"{location} must contain at least one [[surface.guard]]")

        guards: list[Guard] = []
        for guard_index, raw_guard in enumerate(raw_guards, start=1):
            guard_location = f"{location}, guard {guard_index}"
            if not isinstance(raw_guard, dict):
                raise CheckError(f"{guard_location} must be a table")
            kind = raw_guard.get("kind")
            paths = raw_guard.get("paths")
            symbols = raw_guard.get("symbols", [])
            must_cover = raw_guard.get("must_cover", [])
            if kind not in {"file", "rust_items", "rust_serde_shapes"}:
                raise CheckError(f"{guard_location} has unsupported kind {kind!r}")
            if not isinstance(paths, list) or not paths or not all(
                isinstance(value, str) and value for value in paths
            ):
                raise CheckError(f"{guard_location} paths must be non-empty strings")
            if not isinstance(symbols, list) or not all(
                isinstance(value, str) and value for value in symbols
            ):
                raise CheckError(f"{guard_location} symbols must be strings")
            if not isinstance(must_cover, list) or not all(
                isinstance(value, str) and value for value in must_cover
            ):
                raise CheckError(f"{guard_location} must_cover must be strings")
            if kind == "rust_items" and not symbols:
                raise CheckError(f"{guard_location} rust_items requires symbols")
            if kind != "rust_items" and symbols:
                raise CheckError(f"{guard_location} only rust_items accepts symbols")
            if kind == "rust_serde_shapes" and not must_cover:
                raise CheckError(
                    f"{guard_location} rust_serde_shapes requires must_cover"
                )
            if kind not in {"file", "rust_serde_shapes"} and must_cover:
                raise CheckError(
                    f"{guard_location} only file and rust_serde_shapes accept "
                    "must_cover"
                )
            if len(must_cover) != len(set(must_cover)):
                raise CheckError(f"{guard_location} must_cover contains duplicates")
            elide = raw_guard.get("elide")
            if elide is not None and elide not in ELISIONS:
                raise CheckError(
                    f"{guard_location} has unsupported elide {elide!r}; known: "
                    + ", ".join(sorted(ELISIONS))
                )
            guards.append(
                Guard(kind, tuple(paths), tuple(symbols), tuple(must_cover), elide)
            )

        version_regex = raw_surface.get("version_regex")
        if version_regex is not None and not isinstance(version_regex, str):
            raise CheckError(f"{location} version_regex must be a string")
        surfaces.append(
            Surface(
                constant=constant,
                constant_path=constant_path,
                description=description,
                guards=tuple(guards),
                version_regex=version_regex,
            )
        )
    return tuple(surfaces)


class RepositoryView:
    def __init__(self, repo: Path) -> None:
        self.repo = repo
        self._trees: dict[str, tuple[str, ...]] = {}
        self._contents: dict[tuple[str, str], str | None] = {}

    def paths(self, revision: str) -> tuple[str, ...]:
        if revision not in self._trees:
            output = git(self.repo, "ls-tree", "-r", "--name-only", revision).stdout
            self._trees[revision] = tuple(line for line in output.splitlines() if line)
        return self._trees[revision]

    def matching_paths(self, revision: str, patterns: Iterable[str]) -> tuple[str, ...]:
        matches = {
            path
            for path in self.paths(revision)
            for pattern in patterns
            if fnmatch.fnmatchcase(path, pattern)
        }
        return tuple(sorted(matches))

    def content(self, revision: str, path: str) -> str | None:
        key = (revision, path)
        if key not in self._contents:
            result = git(self.repo, "show", f"{revision}:{path}", check=False)
            self._contents[key] = result.stdout if result.returncode == 0 else None
        return self._contents[key]


def _raw_string_start(text: str, index: int) -> tuple[int, str] | None:
    prefix = index
    if text.startswith("br", index):
        prefix += 1
    if not text.startswith("r", prefix):
        return None
    cursor = prefix + 1
    while cursor < len(text) and text[cursor] == "#":
        cursor += 1
    if cursor >= len(text) or text[cursor] != '"':
        return None
    hashes = text[prefix + 1 : cursor]
    return cursor + 1, '"' + hashes


def _char_literal_end(text: str, index: int) -> int | None:
    quote = index + 1 if text.startswith("b'", index) else index
    if quote >= len(text) or text[quote] != "'" or quote + 1 >= len(text):
        return None
    cursor = quote + 1
    if text[cursor] == "\\":
        cursor += 2
        while cursor < len(text):
            if text[cursor] == "'":
                return cursor + 1
            cursor += 2 if text[cursor] == "\\" else 1
        return None
    if cursor + 1 < len(text) and text[cursor + 1] == "'":
        return cursor + 2
    return None


def rust_item_end(text: str, start: int) -> int:
    """Return the end of one Rust item, ignoring delimiters inside literals."""
    index = start
    brace_depth = 0
    saw_brace = False
    block_comment_depth = 0
    state = "normal"
    raw_closer = ""
    while index < len(text):
        char = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if state == "line_comment":
            if char == "\n":
                state = "normal"
            index += 1
            continue
        if state == "block_comment":
            if char == "/" and following == "*":
                block_comment_depth += 1
                index += 2
            elif char == "*" and following == "/":
                block_comment_depth -= 1
                index += 2
                if block_comment_depth == 0:
                    state = "normal"
            else:
                index += 1
            continue
        if state == "string":
            if char == "\\":
                index += 2
            else:
                index += 1
                if char == '"':
                    state = "normal"
            continue
        if state == "char":
            if char == "\\":
                index += 2
            else:
                index += 1
                if char == "'":
                    state = "normal"
            continue
        if state == "raw":
            closing = text.find(raw_closer, index)
            if closing < 0:
                raise CheckError(
                    "unterminated Rust raw string while extracting guarded item"
                )
            index = closing + len(raw_closer)
            state = "normal"
            continue

        if char == "/" and following == "/":
            state = "line_comment"
            index += 2
        elif char == "/" and following == "*":
            state = "block_comment"
            block_comment_depth = 1
            index += 2
        elif raw := _raw_string_start(text, index):
            index, raw_closer = raw
            state = "raw"
        elif char == '"' or (char == "b" and following == '"'):
            state = "string"
            index += 2 if char == "b" else 1
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif char == "{":
            saw_brace = True
            brace_depth += 1
            index += 1
        elif char == "}":
            brace_depth -= 1
            index += 1
            if saw_brace and brace_depth == 0:
                return index
        elif char == ";" and not saw_brace:
            return index + 1
        else:
            index += 1
    raise CheckError("unterminated Rust item while extracting guarded shape")


def strip_rust_trivia(text: str) -> str:
    """Remove Rust whitespace/comments while preserving literals and tokens."""
    output: list[str] = []
    index = 0
    block_depth = 0
    while index < len(text):
        char = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if char.isspace():
            index += 1
        elif char == "/" and following == "/":
            newline = text.find("\n", index + 2)
            index = len(text) if newline < 0 else newline + 1
        elif char == "/" and following == "*":
            index += 2
            block_depth = 1
            while index < len(text) and block_depth:
                if text.startswith("/*", index):
                    block_depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    block_depth -= 1
                    index += 2
                else:
                    index += 1
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            if closing < 0:
                raise CheckError(
                    "unterminated Rust raw string while normalizing guarded item"
                )
            end = closing + len(closer)
            output.append(text[index:end])
            index = end
        elif char == '"' or (char == "b" and following == '"'):
            start = index
            index += 2 if char == "b" else 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        break
            output.append(text[start:index])
        elif char_end := _char_literal_end(text, index):
            output.append(text[index:char_end])
            index = char_end
        else:
            output.append(char)
            index += 1
    return "".join(output)


def rust_attribute_end(text: str, start: int) -> int:
    """Return the end of one balanced Rust outer attribute."""
    if not text.startswith("#[", start):
        raise CheckError("Rust attribute extraction did not start at #[")
    index = start + 2
    bracket_depth = 1
    block_comment_depth = 0
    state = "normal"
    raw_closer = ""
    while index < len(text):
        char = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if state == "line_comment":
            if char == "\n":
                state = "normal"
            index += 1
            continue
        if state == "block_comment":
            if char == "/" and following == "*":
                block_comment_depth += 1
                index += 2
            elif char == "*" and following == "/":
                block_comment_depth -= 1
                index += 2
                if block_comment_depth == 0:
                    state = "normal"
            else:
                index += 1
            continue
        if state == "string":
            if char == "\\":
                index += 2
            else:
                index += 1
                if char == '"':
                    state = "normal"
            continue
        if state == "raw":
            closing = text.find(raw_closer, index)
            if closing < 0:
                raise CheckError(
                    "unterminated Rust raw string while extracting attribute"
                )
            index = closing + len(raw_closer)
            state = "normal"
            continue

        if char == "/" and following == "/":
            state = "line_comment"
            index += 2
        elif char == "/" and following == "*":
            state = "block_comment"
            block_comment_depth = 1
            index += 2
        elif raw := _raw_string_start(text, index):
            index, raw_closer = raw
            state = "raw"
        elif char == '"' or (char == "b" and following == '"'):
            state = "string"
            index += 2 if char == "b" else 1
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif char == "[":
            bracket_depth += 1
            index += 1
        elif char == "]":
            bracket_depth -= 1
            index += 1
            if bracket_depth == 0:
                return index
        else:
            index += 1
    raise CheckError("unterminated Rust outer attribute")


def _raw_outer_attribute_ranges(text: str, start: int) -> tuple[tuple[int, int], ...]:
    """Conservatively recover attributes after malformed Rust trivia."""
    ranges: list[tuple[int, int]] = []
    cursor = start
    while True:
        attribute_start = text.find("#[", cursor)
        if attribute_start < 0:
            return tuple(ranges)
        try:
            attribute_end = rust_attribute_end(text, attribute_start)
        except CheckError:
            cursor = attribute_start + 2
        else:
            ranges.append((attribute_start, attribute_end))
            cursor = attribute_end


def rust_outer_attribute_ranges(text: str) -> tuple[tuple[int, int], ...]:
    """Find outer attributes while ignoring attribute-looking text in trivia."""
    ranges: list[tuple[int, int]] = []
    index = 0
    while index < len(text):
        following = text[index + 1] if index + 1 < len(text) else ""
        if text.startswith("#[", index):
            end = rust_attribute_end(text, index)
            ranges.append((index, end))
            index = end
        elif text[index] == "/" and following == "/":
            newline = text.find("\n", index + 2)
            index = len(text) if newline < 0 else newline + 1
        elif text[index] == "/" and following == "*":
            malformed_start = index + 2
            index += 2
            block_depth = 1
            while index < len(text) and block_depth:
                if text.startswith("/*", index):
                    block_depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    block_depth -= 1
                    index += 2
                else:
                    index += 1
            if block_depth:
                ranges.extend(_raw_outer_attribute_ranges(text, malformed_start))
                return tuple(ranges)
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            if closing < 0:
                ranges.extend(_raw_outer_attribute_ranges(text, content_start))
                return tuple(ranges)
            index = closing + len(closer)
        elif text[index] == '"' or (text[index] == "b" and following == '"'):
            malformed_start = index + (2 if text[index] == "b" else 1)
            index += 2 if text[index] == "b" else 1
            closed = False
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        closed = True
                        break
            if not closed:
                ranges.extend(_raw_outer_attribute_ranges(text, malformed_start))
                return tuple(ranges)
        elif char_end := _char_literal_end(text, index):
            index = char_end
        else:
            index += 1
    return tuple(ranges)


def rust_item_start_with_attributes(
    text: str,
    declaration_start: int,
    attribute_ranges: tuple[tuple[int, int], ...] | None = None,
) -> int:
    """Walk back across complete attributes and interleaved Rust comments."""
    line_start = text.rfind("\n", 0, declaration_start) + 1
    cursor = line_start
    ranges = attribute_ranges or rust_outer_attribute_ranges(text[:declaration_start])
    eligible = [attribute for attribute in ranges if attribute[1] <= cursor]
    while eligible:
        start, end = eligible[-1]
        if strip_rust_trivia(text[end:cursor]):
            break
        cursor = start
        eligible.pop()
    return cursor


RUST_DECLARATION = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?"
    r"(?:const|static|fn|struct|enum|type)[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_SERDE_SHAPE = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?"
    r"(?:struct|enum)[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_INLINE_MODULE = re.compile(
    r"(?:pub(?:\s*\(\s*(?:crate|self|super|in\s+(?:crate|self|super)"
    r"(?:::[A-Za-z_][A-Za-z0-9_]*)*)\s*\))?\s+)?"
    r"mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{",
    re.ASCII,
)


_CFG_ASCII_SPACE = " \t\r\n"
_CFG_SIMPLE_STRING = re.compile(r'"[A-Za-z0-9_.-]*"')


def _rust_space_end(text: str, start: int) -> int:
    cursor = start
    while cursor < len(text) and text[cursor] in _CFG_ASCII_SPACE:
        cursor += 1
    return cursor


def _rust_string_end(text: str, start: int) -> int | None:
    """Return one simple cfg string's end, refusing anything with escapes.

    Proving a predicate test-only never requires interpreting Rust escape
    semantics, so instead of validating escapes this accepts only the plain
    identifier-like strings real cfg values use. Any other string leaves the
    predicate unproven and the region swept.
    """
    matched = _CFG_SIMPLE_STRING.match(text, start)
    return None if matched is None else matched.end()


def _cfg_predicate(text: str, start: int = 0) -> tuple[int, bool] | None:
    """Strictly parse enough cfg grammar to prove a predicate requires `test`."""
    cursor = _rust_space_end(text, start)
    identifier = re.match(r"[A-Za-z_][A-Za-z0-9_]*", text[cursor:])
    if identifier is None:
        return None
    name = identifier.group(0)
    cursor = _rust_space_end(text, cursor + len(name))
    if cursor == len(text) or text[cursor] in ",)":
        return cursor, name == "test"
    if text[cursor] == "=":
        cursor = _rust_space_end(text, cursor + 1)
        string_end = _rust_string_end(text, cursor)
        if string_end is None:
            return None
        return _rust_space_end(text, string_end), False
    if text[cursor] != "(" or name not in {"all", "any", "not"}:
        return None

    cursor = _rust_space_end(text, cursor + 1)
    children: list[bool] = []
    while cursor < len(text) and text[cursor] != ")":
        child = _cfg_predicate(text, cursor)
        if child is None:
            return None
        cursor, test_only = child
        children.append(test_only)
        if cursor < len(text) and text[cursor] == ",":
            cursor = _rust_space_end(text, cursor + 1)
        elif cursor >= len(text) or text[cursor] != ")":
            return None
    if cursor >= len(text) or text[cursor] != ")":
        return None
    if name == "not" and len(children) != 1:
        return None
    if name == "all":
        test_only = any(children)
    elif name == "any":
        test_only = bool(children) and all(children)
    else:
        test_only = False
    return cursor + 1, test_only


def _test_only_cfg(attribute: str) -> bool:
    cursor = _rust_space_end(attribute, 0)
    if cursor >= len(attribute) or attribute[cursor] != "#":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    if cursor >= len(attribute) or attribute[cursor] != "[":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    cfg = re.match(r"cfg\b", attribute[cursor:])
    if cfg is None:
        return False
    cursor = _rust_space_end(attribute, cursor + len(cfg.group(0)))
    if cursor >= len(attribute) or attribute[cursor] != "(":
        return False
    parsed = _cfg_predicate(attribute, cursor + 1)
    if parsed is None:
        return False
    cursor, test_only = parsed
    if cursor >= len(attribute) or attribute[cursor] != ")":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    if cursor >= len(attribute) or attribute[cursor] != "]":
        return False
    cursor = _rust_space_end(attribute, cursor + 1)
    return cursor == len(attribute) and test_only


def _rust_trivia_end(text: str, start: int) -> int | None:
    """Skip whitespace and comments, returning None for malformed trivia."""
    cursor = start
    while cursor < len(text):
        if text[cursor] in _CFG_ASCII_SPACE:
            cursor += 1
        elif text.startswith("//", cursor):
            newline = text.find("\n", cursor + 2)
            cursor = len(text) if newline < 0 else newline + 1
        elif text.startswith("/*", cursor):
            cursor += 2
            depth = 1
            while cursor < len(text) and depth:
                if text.startswith("/*", cursor):
                    depth += 1
                    cursor += 2
                elif text.startswith("*/", cursor):
                    depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            if depth:
                return None
        else:
            break
    return cursor


def test_only_module_ranges(
    text: str, attribute_ranges: tuple[tuple[int, int], ...]
) -> tuple[tuple[int, int], ...]:
    """Return bodies of inline modules that are certainly gated on `test`."""
    ranges: list[tuple[int, int]] = []
    attributes_by_start = {start: end for start, end in attribute_ranges}
    for start, end in attribute_ranges:
        if not _test_only_cfg(text[start:end]):
            continue
        cursor = end
        while True:
            cursor = _rust_trivia_end(text, cursor)
            if cursor is None:
                break
            next_attribute = attributes_by_start.get(cursor)
            if next_attribute is None:
                break
            cursor = next_attribute
        if cursor is None:
            continue
        module = RUST_INLINE_MODULE.match(text, cursor)
        if module is None:
            continue
        try:
            item_end = rust_item_end(text, cursor)
        except CheckError:
            continue
        if text[item_end - 1] == "}":
            ranges.append((module.end(), item_end - 1))
    return tuple(ranges)


def named_rust_items(text: str, names: Iterable[str]) -> dict[str, str]:
    wanted = set(names)
    found: dict[str, str] = {}
    attribute_ranges = rust_outer_attribute_ranges(text)
    for match in RUST_DECLARATION.finditer(text):
        name = match.group(1)
        if name not in wanted:
            continue
        start = rust_item_start_with_attributes(text, match.start(), attribute_ranges)
        end = rust_item_end(text, match.start())
        value = strip_rust_trivia(text[start:end])
        if name in found and found[name] != value:
            raise CheckError(f"guarded Rust symbol {name} is ambiguous in one file")
        found[name] = value
    return found


def serde_shapes(text: str) -> dict[str, str]:
    found: dict[str, str] = {}
    attribute_ranges = rust_outer_attribute_ranges(text)
    excluded_ranges = test_only_module_ranges(text, attribute_ranges)
    for match in RUST_SERDE_SHAPE.finditer(text):
        if any(start <= match.start() < end for start, end in excluded_ranges):
            continue
        name = match.group(1)
        start = rust_item_start_with_attributes(text, match.start(), attribute_ranges)
        attributes = text[start : match.start()]
        if "Serialize" not in attributes and "Deserialize" not in attributes:
            continue
        end = rust_item_end(text, match.start())
        value = strip_rust_trivia(text[start:end])
        key = name
        ordinal = 2
        while key in found:
            key = f"{name}#{ordinal}"
            ordinal += 1
        found[key] = value
    return found


def guard_signature(
    view: RepositoryView,
    revision: str,
    guard: Guard,
    *,
    enforce_presence: bool,
    base_signature: tuple[tuple[str, str], ...] | None = None,
) -> tuple[tuple[str, str], ...]:
    paths = view.matching_paths(revision, guard.paths)
    base_by_key = dict(base_signature) if base_signature is not None else {}
    elide_fn = ELISIONS.get(guard.elide) if guard.elide else None

    def apply_elision(key: str, val: str) -> str:
        if elide_fn is not None and base_signature is not None:
            return elide_fn(val, base_by_key.get(key, ""))
        return val

    signature: list[tuple[str, str]] = []
    found_symbols: set[str] = set()
    covered_shapes: set[str] = set()
    covered_markers: set[str] = set()
    for path in paths:
        content = view.content(revision, path)
        if content is None:
            continue
        if guard.kind == "file":
            signature.append((path, apply_elision(path, content)))
            covered_markers.update(
                marker for marker in guard.must_cover if marker in content
            )
        elif guard.kind == "rust_items":
            items = named_rust_items(content, guard.symbols)
            found_symbols.update(items)
            signature.extend(
                (f"{path}:{name}", apply_elision(f"{path}:{name}", value))
                for name, value in items.items()
            )
        else:
            items = serde_shapes(content)
            covered_shapes.update(name.partition("#")[0] for name in items)
            signature.extend(
                (f"{path}:{name}", apply_elision(f"{path}:{name}", value))
                for name, value in items.items()
            )

    if guard.kind == "rust_items" and enforce_presence:
        missing = sorted(set(guard.symbols) - found_symbols)
        if missing:
            raise CheckError(
                f"{revision[:12]}: guarded Rust symbols not found in "
                f"{', '.join(guard.paths)}: "
                + ", ".join(missing)
            )
    elif guard.kind == "rust_serde_shapes" and enforce_presence:
        missing = sorted(set(guard.must_cover) - covered_shapes)
        if missing:
            raise CheckError(
                f"{revision[:12]}: required Serde shapes not found in "
                f"{', '.join(guard.paths)}: " + ", ".join(missing)
            )
    elif guard.kind == "file" and enforce_presence:
        if not paths:
            raise CheckError(
                f"{revision[:12]}: guarded file pattern matched nothing: "
                f"{', '.join(guard.paths)}"
            )
        missing = sorted(set(guard.must_cover) - covered_markers)
        if missing:
            raise CheckError(
                f"{revision[:12]}: required file markers not found in "
                f"{', '.join(guard.paths)}: " + ", ".join(missing)
            )
    return tuple(sorted(signature))


def version_at(view: RepositoryView, revision: str, surface: Surface) -> int:
    content = view.content(revision, surface.constant_path)
    if content is None:
        raise CheckError(
            f"{revision[:12]}: cannot read {surface.constant_path} for "
            f"{surface.constant}"
        )
    pattern = surface.version_regex or (
        rf"\b{re.escape(surface.constant)}\b\s*:[^=\n]+="
        r"\s*([0-9][0-9_]*)\s*;"
    )
    try:
        matches = list(re.finditer(pattern, content, re.MULTILINE))
    except re.error as error:
        raise CheckError(
            f"invalid version_regex for {surface.constant}: {error}"
        ) from error
    if len(matches) != 1 or len(matches[0].groups()) != 1:
        raise CheckError(
            f"{revision[:12]}: expected exactly one single-capture version match for "
            f"{surface.constant} in {surface.constant_path}, found {len(matches)}"
        )
    value = matches[0].group(1).replace("_", "")
    try:
        return int(value)
    except ValueError as error:
        raise CheckError(
            f"{revision[:12]}: {surface.constant} version {value!r} is not an integer"
        ) from error


def inventory_keys(document: object) -> frozenset[str] | None:
    """The surface keys a parsed inventory declares, or None if unreadable."""
    if not isinstance(document, dict):
        return None
    raw_surfaces = document.get("surface")
    if not isinstance(raw_surfaces, list):
        return None
    keys: set[str] = set()
    for raw_surface in raw_surfaces:
        if not isinstance(raw_surface, dict):
            return None
        constant = raw_surface.get("constant")
        constant_path = raw_surface.get("constant_path")
        if not isinstance(constant, str) or not isinstance(constant_path, str):
            return None
        keys.add(f"{constant_path}:{constant}")
    return frozenset(keys)


def base_inventory_keys(
    repo: Path, base_revision: str, config: Path
) -> frozenset[str] | None:
    """The surfaces the merge-base inventory declared, or None when undecidable.

    A surface absent from that inventory is registered by this change, so its
    guarded shape has no version to be compared against and none to be bumped
    past.  Whenever the base inventory cannot be read the answer is None and
    every surface is checked as pre-existing, which is the stricter reading.
    """
    try:
        relative = config.resolve().relative_to(repo.resolve()).as_posix()
    except (OSError, ValueError):
        return None
    result = git(repo, "show", f"{base_revision}:{relative}", check=False)
    if result.returncode != 0:
        return None
    try:
        return inventory_keys(tomllib.loads(result.stdout))
    except tomllib.TOMLDecodeError:
        return None


def surface_fingerprint(entries: Iterable[tuple[str, str]]) -> str:
    """Pin the guarded bytes a registration enrolled.

    The digest covers every guarded path and symbol the surface's head
    signature carries, in guard order, so a baseline burned for one enrolment
    cannot be reused by a later change to the same shape.
    """
    digest = hashlib.sha256()
    for key, value in entries:
        digest.update(key.encode("utf-8"))
        digest.update(b"\0")
        digest.update(value.encode("utf-8"))
        digest.update(b"\0")
    return f"sha256:{digest.hexdigest()}"


def check_surfaces(
    repo: Path,
    base: str,
    head: str,
    surfaces: Iterable[Surface],
    base_keys: frozenset[str] | None = None,
) -> CheckResult:
    base_revision = resolve_revision(repo, base)
    head_revision = resolve_revision(repo, head)
    view = RepositoryView(repo)
    failures: list[Failure] = []
    errors: list[SurfaceError] = []
    registrations: list[Surface] = []
    unregistered: list[Unregistered] = []
    identifier_renames: list[Surface] = []
    for surface in surfaces:
        try:
            head_version = version_at(view, head_revision, surface)
        except CheckError as error:
            errors.append(SurfaceError(surface, str(error)))
            continue
        changed_guards: list[str] = []
        head_entries: list[tuple[str, str]] = []
        for guard in surface.guards:
            try:
                base_signature = guard_signature(
                    view, base_revision, guard, enforce_presence=False
                )
                head_signature = guard_signature(
                    view,
                    head_revision,
                    guard,
                    enforce_presence=True,
                    base_signature=base_signature,
                )
            except CheckError as error:
                errors.append(
                    SurfaceError(
                        surface,
                        f"guard {', '.join(guard.paths)}: {error}",
                    )
                )
                continue
            head_entries.extend(head_signature)
            if base_signature != head_signature:
                changed_guards.extend(guard.paths)
        if not changed_guards:
            continue
        try:
            base_version = version_at(view, base_revision, surface)
        except CheckError as error:
            # No merge-base version exists for this constant. Either the change
            # registers the surface -- a burned baseline says so by name and by
            # the exact shape it enrolled -- or the constant was renamed,
            # relocated, or added over a shape that was already moving, and the
            # shape change stands unaccounted for.
            fingerprint = surface_fingerprint(head_entries)
            registered = (
                base_keys is not None
                and surface.key not in base_keys
                and REGISTRATION_BASELINES.get(surface.key) == fingerprint
            )
            if registered:
                registrations.append(surface)
                continue
            if base_keys is None or surface.key in base_keys:
                errors.append(SurfaceError(surface, str(error)))
                continue
            unregistered.append(
                Unregistered(
                    surface=surface,
                    changed_guards=tuple(dict.fromkeys(changed_guards)),
                    fingerprint=fingerprint,
                )
            )
            continue
        if head_version <= base_version:
            # The guarded text moved. A burned identifier-rename baseline is a
            # reviewer's signed answer that the format underneath it did not,
            # and it holds only for the exact guarded bytes it pinned.
            fingerprint = surface_fingerprint(head_entries)
            if IDENTIFIER_RENAME_BASELINES.get(surface.key) == fingerprint:
                identifier_renames.append(surface)
                continue
            failures.append(
                Failure(
                    surface=surface,
                    base_version=base_version,
                    head_version=head_version,
                    changed_guards=tuple(dict.fromkeys(changed_guards)),
                    fingerprint=fingerprint,
                )
            )
    return CheckResult(
        tuple(failures),
        tuple(errors),
        tuple(registrations),
        tuple(unregistered),
        tuple(identifier_renames),
    )


def select_surfaces(
    surfaces: tuple[Surface, ...], selectors: Iterable[str]
) -> tuple[Surface, ...]:
    selected: list[Surface] = []
    for selector in selectors:
        key_matches = [surface for surface in surfaces if surface.key == selector]
        matches = key_matches or [
            surface for surface in surfaces if surface.constant == selector
        ]
        if len(matches) != 1:
            detail = ""
            if matches:
                detail = "; use one of: " + ", ".join(
                    surface.key for surface in matches
                )
            raise CheckError(
                f"--surface {selector} matched {len(matches)} inventory entries"
                f"{detail}"
            )
        selected.extend(matches)
    return tuple(selected)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="merge-base commit to compare")
    parser.add_argument("--head", default="HEAD", help="head commit (default: HEAD)")
    parser.add_argument(
        "--config", type=Path, default=DEFAULT_CONFIG, help="surface inventory TOML"
    )
    parser.add_argument(
        "--surface",
        action="append",
        default=[],
        help=(
            "check one surface by unique constant or <constant_path>:<constant> "
            "key (repeatable; diagnostic use)"
        ),
    )
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    try:
        surfaces = load_config(args.config)
        if args.surface:
            surfaces = select_surfaces(surfaces, args.surface)
        base = resolve_revision(args.repo, args.base)
        head = resolve_revision(args.repo, args.head)
        result = check_surfaces(
            args.repo,
            base,
            head,
            surfaces,
            base_inventory_keys(args.repo, base, args.config),
        )
    except CheckError as error:
        print(f"version-bump check error: {error}", file=sys.stderr)
        return 2

    if result.errors:
        print(
            f"version-bump check errors against merge-base {base[:12]}:",
            file=sys.stderr,
        )
        for error in result.errors:
            print(f"- {error.surface.key}: {error.detail}", file=sys.stderr)

    if result.failures or result.unregistered:
        print(
            f"version-bump check failed against merge-base {base[:12]}:",
            file=sys.stderr,
        )
        for entry in result.unregistered:
            paths = ", ".join(entry.changed_guards)
            print(
                f"- {entry.surface.constant} has no merge-base value in "
                f"{entry.surface.constant_path} and no burned registration "
                f"baseline, but its guarded shape changed ({paths}). A renamed or "
                f"relocated constant keeps its merge-base identity and bumps that; "
                f"a genuinely new surface registers once by adding "
                f"{entry.surface.key!r}: {entry.fingerprint!r} to "
                f"REGISTRATION_BASELINES in scripts/check_version_bumps.py.",
                file=sys.stderr,
            )
        for failure in result.failures:
            paths = ", ".join(failure.changed_guards)
            print(
                f"- {failure.surface.constant} is {failure.head_version}; merge-base "
                f"value is {failure.base_version}. Guarded shape changed ({paths}). Bump "
                f"{failure.surface.constant} strictly past {failure.base_version}. "
                f"If the change only retyped Rust identifiers and a reviewer has "
                f"confirmed the serialized bytes are identical on both sides, burn "
                f"that reading once by adding {failure.surface.key!r}: "
                f"{failure.fingerprint!r} to IDENTIFIER_RENAME_BASELINES in "
                f"scripts/check_version_bumps.py.",
                file=sys.stderr,
            )
    if result.errors:
        return 2
    if result.failures or result.unregistered:
        return 1

    registered = ""
    if result.registrations:
        names = ", ".join(surface.key for surface in result.registrations)
        registered = f"; registered by this change: {names}"
    renamed = ""
    if result.identifier_renames:
        names = ", ".join(surface.key for surface in result.identifier_renames)
        renamed = f"; identifier-rename baseline honoured for: {names}"
    print(
        f"version-bump check passed: {len(surfaces)} surfaces against "
        f"merge-base {base[:12]}{registered}{renamed}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
