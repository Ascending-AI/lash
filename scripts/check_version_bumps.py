#!/usr/bin/env python3
"""Require a version bump, and its upgrade evidence, when a guarded shape changes.

Every ``[[surface]]`` of ``scripts/versioned-surfaces.toml`` names a version
constant. The shapes that constant guards are declared in code, on the
constant itself, by a marker in its doc comment::

    /// The demo record's stored format.
    ///
    /// version_guard(
    ///     items(Request, encode_request),
    ///     items(path = "crates/demo/src/wire.rs", Envelope),
    ///     shapes(path = "crates/demo/src/dto/*.rs", cover(Reply)),
    ///     impls("Serialize for Request"),
    ///     file(path = "crates/demo/schema.sql", cover("CREATE TABLE demo")),
    /// )
    pub const DEMO_VERSION: u32 = 3;

The marker is documentation, not an attribute: it states what the version
versions, no build, lint or feature-gate check reads it, and this script and
``check_format_registry.py`` are its only readers. It sits in the constant's
own block of doc comments and attributes, with no blank line between that
block and the ``const``.

- ``items`` guards the named declarations (``const``, ``static``, ``fn``,
  ``struct``, ``enum``, ``type``), comments and formatting ignored;
- ``shapes`` guards every Serde-derived struct and enum in the matched files,
  and ``cover(..)`` names shapes that must be among them;
- ``impls`` guards hand-written ``Serialize for T`` / ``Deserialize for T``
  impls;
- ``file`` guards whole files, and ``cover(..)`` names text they must contain.

A guard without ``path`` reads the file that defines the constant. A path is
repository-relative and may be a glob. A surface that versions no shape the
tree can project says so instead: ``version_guard(unshaped = "<reason>")``.

The gate compares two commits. For each surface it projects the guarded
shapes at ``--base`` and at ``--head``; when they differ, the head's constant
must be strictly greater than the base's, and a surface held to the decoder
laws (``upgrade = "migrate"`` without ``unguarded``) must register a
``RecordUpcaster`` row for every version it steps over. The shapes are
projected under the head's markers and under the base's, so dropping a shape
from a marker does not excuse changing it, and a constant that disappears
does not excuse the shapes it guarded.

There is no report-only mode. A changed shape without its bump, a bump
without its evidence, and a guard that cannot be evaluated all exit nonzero.

Guard hashing ignores only allowlisted non-wire derives inside top-level
``derive`` attributes and ``derive`` entries of ``cfg_attr``. Wire-producing
and unknown derives remain in the preimage, as do namespaced attributes, macro
arguments, and every ``serde`` attribute.

Only the Python standard library is used so the check can run before the Rust
toolchain is installed.
"""

from __future__ import annotations

import argparse
import contextlib
from dataclasses import dataclass
import fnmatch
from functools import lru_cache
import hashlib
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Iterable


ROOT = Path(__file__).resolve().parents[1]
REGISTRY = "scripts/versioned-surfaces.toml"
MARKER = "version_guard"
GUARD_KINDS = ("items", "shapes", "impls", "file")
# The one table a record lift is registered in (ADR 0106 §2). Its default-build
# arm is the upgrade evidence a bumped decoder-law surface owes.
UPCASTER_REGISTRY = "crates/lash-core-store/src/store/fleet_format.rs"
UPCASTER_TABLE = "RECORD_UPCASTERS"
SYNTHETIC_NEXT_CFG = '#[cfg(feature="synthetic-next")]'


class CheckError(RuntimeError):
    """Configuration, repository, or source-shape error."""


# A non-unique `CREATE INDEX IF NOT EXISTS ... ;` statement, whole.
#
# In a catalog whose open path always executes the entire schema text, such a
# statement is not a compatibility boundary: it reaches an existing database on
# its next open, and a database that already has the index stays readable by a
# binary whose schema omits it. Both directions are therefore compatible on one
# file, which is why a guard may declare `elide` and let an index-only addition
# ship without a version bump.
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


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    # Captured as bytes and decoded here rather than through subprocess's text
    # mode, for two reasons that both matter to the guard signatures built from
    # this output. Strict UTF-8 cannot read a binary file at all, so decoding
    # is lenient -- but lenient as `surrogateescape`, which round-trips every
    # byte to a distinct code point, never `replace`, which collapses every
    # invalid byte to one U+FFFD and would render a changed binary file
    # identical to the original. And text mode also applies universal-newline
    # translation, which folds CR and CRLF into LF: in a binary payload that is
    # a real content change reading as no change.
    completed = subprocess.run(["git", *args], cwd=repo, capture_output=True)
    result: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
        completed.args,
        completed.returncode,
        completed.stdout.decode("utf-8", errors="surrogateescape"),
        completed.stderr.decode("utf-8", errors="surrogateescape"),
    )
    if check and result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise CheckError(f"git {' '.join(args)} failed: {detail}")
    return result


def resolve_revision(repo: Path, revision: str) -> str:
    return git(repo, "rev-parse", "--verify", f"{revision}^{{commit}}").stdout.strip()


class TreeView:
    """One tree the guards are projected from: a commit, or the working tree."""

    label: str

    def __init__(self) -> None:
        self._projected: dict[str, str | None] = {}

    def matching_paths(self, patterns: Iterable[str]) -> tuple[str, ...]:
        raise NotImplementedError

    def content(self, path: str) -> str | None:
        raise NotImplementedError

    def projected(self, path: str) -> str | None:
        """The file as the guards hash it: Rust sources without their markers."""
        if path not in self._projected:
            content = self.content(path)
            if content is not None and path.endswith(".rs"):
                content = strip_markers(content)
            self._projected[path] = content
        return self._projected[path]


class RevisionView(TreeView):
    def __init__(self, repo: Path, revision: str) -> None:
        super().__init__()
        self.repo = repo
        self.revision = revision
        self.label = revision[:12]
        self._paths: tuple[str, ...] | None = None
        self._contents: dict[str, str | None] = {}

    def matching_paths(self, patterns: Iterable[str]) -> tuple[str, ...]:
        if self._paths is None:
            output = git(self.repo, "ls-tree", "-r", "--name-only", self.revision).stdout
            self._paths = tuple(line for line in output.splitlines() if line)
        matches = {
            path
            for path in self._paths
            for pattern in patterns
            if fnmatch.fnmatchcase(path, pattern)
        }
        return tuple(sorted(matches))

    def content(self, path: str) -> str | None:
        if path not in self._contents:
            result = git(self.repo, "show", f"{self.revision}:{path}", check=False)
            self._contents[path] = result.stdout if result.returncode == 0 else None
        return self._contents[path]


class WorktreeView(TreeView):
    """The files on disk, so the registry check reads what a commit would."""

    label = "working tree"

    def __init__(self, repo: Path) -> None:
        super().__init__()
        self.repo = repo
        self._contents: dict[str, str | None] = {}

    def matching_paths(self, patterns: Iterable[str]) -> tuple[str, ...]:
        matches: set[str] = set()
        for pattern in patterns:
            wildcard = re.search(r"[*?\[]", pattern)
            if wildcard is None:
                if (self.repo / pattern).is_file():
                    matches.add(pattern)
                continue
            prefix = pattern[: wildcard.start()].rpartition("/")[0]
            base = self.repo / prefix
            if not base.is_dir():
                continue
            for path in base.rglob("*"):
                relative = path.relative_to(self.repo).as_posix()
                if (
                    path.is_file()
                    and "/target/" not in f"/{relative}"
                    and fnmatch.fnmatchcase(relative, pattern)
                ):
                    matches.add(relative)
        return tuple(sorted(matches))

    def content(self, path: str) -> str | None:
        if path not in self._contents:
            try:
                data = (self.repo / path).read_bytes()
            except OSError:
                self._contents[path] = None
            else:
                self._contents[path] = data.decode("utf-8", errors="surrogateescape")
        return self._contents[path]


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
    """Return the end of one Rust item, ignoring delimiters inside literals.

    All three delimiter kinds are tracked, not braces alone, because an item's
    terminator is only its terminator at the top level of the item. A brace
    inside a bracket is a struct literal in an initializer -- the registry
    constant ``const XS: &[Builtin] = &[Builtin { .. }, ..];`` is the shape that
    matters here -- and a semicolon inside a bracket is an array length. Reading
    either as the end truncates the item after its first element, which reads as
    a guard over a table while covering only its head: appending to the table
    then changes nothing the guard can see.
    """
    index = start
    brace_depth = 0
    bracket_depth = 0
    paren_depth = 0
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
            if brace_depth == 0 and bracket_depth == 0 and paren_depth == 0:
                saw_brace = True
            brace_depth += 1
            index += 1
        elif char == "}":
            brace_depth -= 1
            index += 1
            if saw_brace and brace_depth == 0:
                return index
        elif char == "[":
            bracket_depth += 1
            index += 1
        elif char == "]":
            bracket_depth -= 1
            index += 1
        elif char == "(":
            paren_depth += 1
            index += 1
        elif char == ")":
            paren_depth -= 1
            index += 1
        elif char == ";" and not brace_depth and not bracket_depth and not paren_depth:
            return index + 1
        else:
            index += 1
    raise CheckError("unterminated Rust item while extracting guarded shape")


@lru_cache(maxsize=None)
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


_SCAN_CANDIDATE = re.compile(r"""#\[|/[/*]|["']|b["']|b?r[#"]""")


@lru_cache(maxsize=None)
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
            # Nothing starts here; skip to the next character that could open
            # an attribute, a comment or a literal.
            candidate = _SCAN_CANDIDATE.search(text, index + 1)
            index = len(text) if candidate is None else candidate.start()
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


NON_WIRE_DERIVES = {
    "Debug",
    "Clone",
    "Copy",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
    "Default",
    "schemars::JsonSchema",
    "thiserror::Error",
}


def _rust_group_end(text: str, open_index: int) -> int | None:
    """Return the exclusive end of a token-aware parenthesized Rust group."""
    if open_index >= len(text) or text[open_index] != "(":
        return None
    index = open_index + 1
    depth = 1
    while index < len(text):
        following = text[index + 1] if index + 1 < len(text) else ""
        if text.startswith("//", index):
            newline = text.find("\n", index + 2)
            index = len(text) if newline < 0 else newline + 1
        elif text.startswith("/*", index):
            skipped = _rust_trivia_end(text, index)
            if skipped is None:
                return None
            index = skipped
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            if closing < 0:
                return None
            index = closing + len(closer)
        elif text[index] == '"' or (text[index] == "b" and following == '"'):
            index += 2 if text[index] == "b" else 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        break
            else:
                return None
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif text[index] == "(":
            depth += 1
            index += 1
        elif text[index] == ")":
            depth -= 1
            index += 1
            if depth == 0:
                return index
        else:
            index += 1
    return None


def _rust_top_level_items(text: str, start: int, end: int) -> tuple[tuple[int, int], ...] | None:
    """Split a Rust token range on top-level commas."""
    items: list[tuple[int, int]] = []
    item_start = start
    index = start
    delimiters: list[str] = []
    pairs = {"(": ")", "[": "]", "{": "}"}
    while index < end:
        following = text[index + 1] if index + 1 < end else ""
        if text.startswith("//", index):
            newline = text.find("\n", index + 2, end)
            index = end if newline < 0 else newline + 1
        elif text.startswith("/*", index):
            skipped = _rust_trivia_end(text[:end], index)
            if skipped is None:
                return None
            index = skipped
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start, end)
            if closing < 0:
                return None
            index = closing + len(closer)
        elif text[index] == '"' or (text[index] == "b" and following == '"'):
            index += 2 if text[index] == "b" else 1
            while index < end:
                if text[index] == "\\":
                    index += 2
                else:
                    closing = text[index] == '"'
                    index += 1
                    if closing:
                        break
            else:
                return None
        elif char_end := _char_literal_end(text, index):
            index = char_end
        elif text[index] in pairs:
            delimiters.append(pairs[text[index]])
            index += 1
        elif text[index] in ")]}":
            if not delimiters or delimiters.pop() != text[index]:
                return None
            index += 1
        elif text[index] == "," and not delimiters:
            items.append((item_start, index))
            item_start = index + 1
            index += 1
        else:
            index += 1
    if delimiters:
        return None
    items.append((item_start, end))
    return tuple(items)


def _derive_contents(text: str, start: int, end: int) -> tuple[int, int] | None:
    """Return contents for an item that is exactly `derive(...)`."""
    cursor = _rust_trivia_end(text, start)
    if cursor is None:
        return None
    name = re.match(r"derive\b", text[cursor:end])
    if name is None:
        return None
    cursor = _rust_trivia_end(text, cursor + len(name.group(0)))
    if cursor is None or cursor >= end or text[cursor] != "(":
        return None
    group_end = _rust_group_end(text[:end], cursor)
    if group_end is None:
        return None
    tail = _rust_trivia_end(text, group_end)
    if tail != end:
        return None
    return cursor + 1, group_end - 1


def _attribute_derive_lists(attribute: str) -> tuple[tuple[int, int], ...]:
    """Find only top-level derive and cfg_attr derive-list positions."""
    content_start = 2
    content_end = len(attribute) - 1
    cursor = _rust_trivia_end(attribute, content_start)
    if cursor is None:
        return ()
    name = re.match(r"([A-Za-z_][A-Za-z0-9_]*)\b", attribute[cursor:content_end])
    if name is None:
        return ()
    attribute_name = name.group(1)
    cursor = _rust_trivia_end(attribute, cursor + len(attribute_name))
    if cursor is None or cursor >= content_end or attribute[cursor] != "(":
        return ()
    group_end = _rust_group_end(attribute[:content_end], cursor)
    if group_end is None or _rust_trivia_end(attribute, group_end) != content_end:
        return ()
    if attribute_name == "derive":
        return ((cursor + 1, group_end - 1),)
    if attribute_name != "cfg_attr":
        return ()
    items = _rust_top_level_items(attribute, cursor + 1, group_end - 1)
    if items is None:
        return ()
    return tuple(
        contents
        for item_start, item_end in items[1:]
        if (contents := _derive_contents(attribute, item_start, item_end)) is not None
    )


def normalize_rust_derive_lists(text: str) -> str:
    """Ignore allowlisted non-wire derives in real derive attributes only."""
    replacements: list[tuple[int, int, str]] = []
    for attribute_start, attribute_end in rust_outer_attribute_ranges(text):
        attribute = text[attribute_start:attribute_end]
        for start, end in _attribute_derive_lists(attribute):
            items = _rust_top_level_items(attribute, start, end)
            if items is None:
                continue
            retained = [
                strip_rust_trivia(attribute[item_start:item_end])
                for item_start, item_end in items
                if strip_rust_trivia(attribute[item_start:item_end])
                and strip_rust_trivia(attribute[item_start:item_end]) not in NON_WIRE_DERIVES
            ]
            replacements.append(
                (attribute_start + start, attribute_start + end, ",".join(retained))
            )
    normalized = text
    for start, end, replacement in reversed(replacements):
        normalized = normalized[:start] + replacement + normalized[end:]
    return normalized


RUST_DECLARATION = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?"
    r"(?:(?:const[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?fn|const|static|struct|enum|type)"
    r"[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_SERDE_SHAPE = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?"
    r"(?:struct|enum)[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_SERDE_IMPL = re.compile(
    r"(?m)^[ \t]*impl(?:[ \t]*<[^>{}]*>)?[ \t]+"
    r"((?:serde::)?(?:Serialize|Deserialize)(?:[ \t]*<[^>{}]*>)?)"
    r"[ \t]+for[ \t]+([A-Za-z_][A-Za-z0-9_]*)\b"
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
    """Every production declaration called one of `names`.

    A name several items share (an `as_str` on each of two enums) guards all
    of them: the second is keyed `name#2`, in source order.
    """
    wanted = set(names)
    found: dict[str, str] = {}
    attribute_ranges = rust_outer_attribute_ranges(text)
    excluded_ranges = test_only_module_ranges(text, attribute_ranges)
    for match in RUST_DECLARATION.finditer(text):
        name = match.group(1)
        if name not in wanted:
            continue
        if any(start <= match.start() < end for start, end in excluded_ranges):
            continue
        start = rust_item_start_with_attributes(text, match.start(), attribute_ranges)
        # The gate projects the default build: an item's `synthetic-next` arm
        # is the upgrade harness's stand-in successor, not a shipped shape.
        if SYNTHETIC_NEXT_CFG in strip_rust_trivia(text[start : match.start()]):
            continue
        end = rust_item_end(text, match.start())
        value = strip_rust_trivia(normalize_rust_derive_lists(text[start:end]))
        key = name
        ordinal = 2
        while key in found:
            key = f"{name}#{ordinal}"
            ordinal += 1
        found[key] = value
    return found


def named_rust_serde_impls(text: str, names: Iterable[str]) -> dict[str, str]:
    wanted = set(names)
    found: dict[str, str] = {}
    for match in RUST_SERDE_IMPL.finditer(text):
        trait = "Deserialize" if "Deserialize" in match.group(1) else "Serialize"
        name = f"{trait} for {match.group(2)}"
        if name not in wanted:
            continue
        end = rust_item_end(text, match.start())
        value = strip_rust_trivia(text[match.start() : end])
        if name in found and found[name] != value:
            raise CheckError(f"guarded Rust impl {name} is ambiguous in one file")
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
        value = strip_rust_trivia(normalize_rust_derive_lists(text[start:end]))
        key = name
        ordinal = 2
        while key in found:
            key = f"{name}#{ordinal}"
            ordinal += 1
        found[key] = value
    return found



@dataclass(frozen=True)
class Guard:
    kind: str
    paths: tuple[str, ...]
    symbols: tuple[str, ...] = ()
    must_cover: tuple[str, ...] = ()
    elide: str | None = None

    @property
    def label(self) -> str:
        return f"{self.kind}({', '.join(self.paths)})"


@dataclass(frozen=True)
class Declaration:
    """What one surface's constant declares it guards."""

    guards: tuple[Guard, ...] = ()
    unshaped: str | None = None


@dataclass(frozen=True)
class Surface:
    constant: str
    constant_path: str
    upgrade: str
    decoder_laws: bool

    @property
    def key(self) -> str:
        return f"{self.constant_path}:{self.constant}"


# One guarded entry: where it was found, what it is called, and its projection.
# Two signatures compare on name and projection alone, so moving a shape to
# another file is not a shape change.
Entry = tuple[str, str, str]

MARKER_TOKEN = re.compile(
    r"""\s*(?:
        (?P<comment>//[^\n]*)
      | (?P<string>"(?:[^"\\]|\\.)*")
      | (?P<ident>[A-Za-z_][A-Za-z0-9_]*)
      | (?P<punct>[(),=])
    )""",
    re.VERBOSE,
)


def _marker_tokens(text: str) -> list[tuple[str, str]]:
    tokens: list[tuple[str, str]] = []
    index = 0
    while index < len(text):
        if not text[index:].strip():
            break
        match = MARKER_TOKEN.match(text, index)
        if match is None:
            raise CheckError(
                f"{MARKER} marker has an unreadable token at "
                f"{text[index:].strip()[:24]!r}"
            )
        index = match.end()
        if match.group("comment") is not None:
            continue
        for kind in ("string", "ident", "punct"):
            value = match.group(kind)
            if value is not None:
                if kind == "string":
                    value = re.sub(r"\\(.)", r"\1", value[1:-1])
                tokens.append((kind, value))
    return tokens


MARKER_OPENING = re.compile(rf"^[ \t]*///[ \t]*{MARKER}\b", re.MULTILINE)
DOC_LINE = re.compile(r"[ \t]*///(?P<body>[^\n]*)\n?")
BLANK_DOC_LINE = re.compile(r"[ \t]*///[ \t]*\n\Z")


def marker_blocks(text: str) -> list[tuple[int, int, str]]:
    """Every marker comment in `text`: its line range and its body.

    A marker opens on a `/// version_guard` line and runs over the doc lines
    that follow until its parentheses close.
    """
    blocks: list[tuple[int, int, str]] = []
    cursor = 0
    while (opening := MARKER_OPENING.search(text, cursor)) is not None:
        start = cursor = opening.start()
        body: list[str] = []
        depth = 0
        opened = False
        while (line := DOC_LINE.match(text, cursor)) is not None:
            body.append(line.group("body"))
            cursor = line.end()
            for kind, value in _marker_tokens(line.group("body")):
                if (kind, value) == ("punct", "("):
                    depth += 1
                    opened = True
                elif (kind, value) == ("punct", ")"):
                    depth -= 1
            if opened and depth <= 0:
                break
        blocks.append((start, cursor, "\n".join(body)))
    return blocks


def strip_markers(text: str) -> str:
    """Remove every marker, so declaring a guard is not a change to a
    whole-file guard over the file that declares it."""
    if MARKER not in text:
        return text
    try:
        blocks = marker_blocks(text)
    except CheckError:
        return text
    for start, end, _ in reversed(blocks):
        # The blank doc line that sets the marker apart from the prose above
        # it goes with it.
        previous = text.rfind("\n", 0, max(start - 1, 0)) + 1
        if BLANK_DOC_LINE.match(text[previous:start]):
            start = previous
        text = text[:start] + text[end:]
    return text


class _MarkerParser:
    def __init__(self, tokens: list[tuple[str, str]], constant_path: str) -> None:
        self.tokens = tokens
        self.index = 0
        self.constant_path = constant_path

    def peek(self) -> tuple[str, str]:
        if self.index >= len(self.tokens):
            raise CheckError(f"{MARKER} marker ends early")
        return self.tokens[self.index]

    def take(self, kind: str, value: str | None = None) -> str:
        found_kind, found = self.peek()
        if found_kind != kind or (value is not None and found != value):
            wanted = value if value is not None else f"a {kind}"
            raise CheckError(f"{MARKER} marker expected {wanted}, found {found!r}")
        self.index += 1
        return found

    def at(self, kind: str, value: str) -> bool:
        return self.index < len(self.tokens) and self.tokens[self.index] == (kind, value)

    def list_items(self, parse_item) -> None:
        """`( item, item, )` with an optional trailing comma."""
        self.take("punct", "(")
        while not self.at("punct", ")"):
            parse_item()
            if not self.at("punct", ")"):
                self.take("punct", ",")
        self.take("punct", ")")

    def parse(self) -> Declaration:
        self.take("ident", MARKER)
        guards: list[Guard] = []
        reasons: list[str] = []

        def entry() -> None:
            name = self.take("ident")
            if name == "unshaped":
                self.take("punct", "=")
                reason = self.take("string")
                if not reason.strip():
                    raise CheckError(f"{MARKER} unshaped must state a reason")
                reasons.append(reason)
            elif name in GUARD_KINDS:
                guards.append(self.guard(name))
            else:
                raise CheckError(
                    f"{MARKER} marker has unknown entry {name!r}; known: "
                    + ", ".join((*GUARD_KINDS, "unshaped"))
                )

        self.list_items(entry)
        if self.index != len(self.tokens):
            raise CheckError(f"{MARKER} marker has trailing tokens")
        if len(reasons) > 1 or (reasons and guards):
            raise CheckError(
                f"{MARKER} marker states unshaped beside other entries; a surface "
                "either declares guards or says why it has none"
            )
        if not reasons and not guards:
            raise CheckError(f"{MARKER} marker declares nothing")
        return Declaration(tuple(guards), reasons[0] if reasons else None)

    def guard(self, kind: str) -> Guard:
        paths: list[str] = []
        symbols: list[str] = []
        cover: list[str] = []
        elide: list[str] = []

        def argument() -> None:
            token_kind, value = self.peek()
            self.index += 1
            if token_kind == "string":
                symbols.append(value)
            elif token_kind != "ident":
                raise CheckError(f"{MARKER} {kind}(..) has unexpected {value!r}")
            elif value in {"path", "elide"} and self.at("punct", "="):
                self.take("punct", "=")
                (paths if value == "path" else elide).append(self.take("string"))
            elif value == "cover" and self.at("punct", "("):
                self.list_items(lambda: cover.append(self._name()))
            else:
                symbols.append(value)

        self.list_items(argument)
        if not all(paths) or len(paths) != len(set(paths)):
            raise CheckError(f"{MARKER} {kind}(..) paths must be distinct and non-empty")
        if kind in {"items", "impls"} and not symbols:
            raise CheckError(f"{MARKER} {kind}(..) names nothing to guard")
        if kind in {"shapes", "file"} and symbols:
            raise CheckError(
                f"{MARKER} {kind}(..) takes path, cover and elide, not "
                + ", ".join(symbols)
            )
        if kind in {"items", "impls"} and cover:
            raise CheckError(f"{MARKER} {kind}(..) does not take cover")
        for values, what in ((symbols, "names"), (cover, "cover")):
            if len(values) != len(set(values)):
                raise CheckError(f"{MARKER} {kind}(..) {what} contain duplicates")
        if len(elide) > 1 or (elide and elide[0] not in ELISIONS):
            raise CheckError(
                f"{MARKER} {kind}(..) has unsupported elide {elide!r}; known: "
                + ", ".join(sorted(ELISIONS))
            )
        return Guard(
            kind,
            tuple(paths) or (self.constant_path,),
            tuple(symbols),
            tuple(cover),
            elide[0] if elide else None,
        )

    def _name(self) -> str:
        kind, value = self.peek()
        if kind not in {"ident", "string"}:
            raise CheckError(f"{MARKER} cover(..) has unexpected {value!r}")
        self.index += 1
        return value


def constant_definitions(text: str, name: str) -> list[tuple[str, str]]:
    """Every production definition of `const name`: the block of comments and
    attributes above it, and its value."""
    attribute_ranges = rust_outer_attribute_ranges(text)
    excluded = test_only_module_ranges(text, attribute_ranges)
    pattern = re.compile(
        rf"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?const[ \t]+{re.escape(name)}\b"
        r"\s*:[^=;]+=\s*(?P<value>[^;]+);"
    )
    found: list[tuple[str, str]] = []
    for match in pattern.finditer(text):
        if any(start <= match.start() < end for start, end in excluded):
            continue
        start = rust_item_start_with_attributes(text, match.start(), attribute_ranges)
        # The block also takes in the comment lines directly above the first
        # attribute, up to the nearest blank line or item.
        while start > 0:
            line_start = text.rfind("\n", 0, start - 1) + 1
            if not text[line_start:start].lstrip().startswith("//"):
                break
            start = line_start
        found.append((text[start : match.start()], match.group("value").strip()))
    return found


def declaration_in(text: str, surface: Surface) -> Declaration | None:
    """The guards `surface`'s constant declares, or None when it declares none.

    A constant with a `synthetic-next` arm may carry the marker on either arm;
    every marker found is read, and a marker that does not parse is an error
    rather than an absence, so a typo cannot quietly un-guard a surface.
    """
    definitions = constant_definitions(text, surface.constant)
    if not definitions:
        raise CheckError(
            f"{surface.constant_path} defines no constant {surface.constant}"
        )
    guards: list[Guard] = []
    reasons: list[str] = []
    for block, _ in definitions:
        try:
            for _, _, body in marker_blocks(block):
                parsed = _MarkerParser(
                    _marker_tokens(body), surface.constant_path
                ).parse()
                guards.extend(parsed.guards)
                if parsed.unshaped is not None:
                    reasons.append(parsed.unshaped)
        except CheckError as error:
            raise CheckError(f"{surface.key}: {error}") from error
    if not guards and not reasons:
        return None
    if len(reasons) > 1 or (reasons and guards):
        raise CheckError(
            f"{surface.key}: states unshaped beside other {MARKER} entries"
        )
    return Declaration(tuple(guards), reasons[0] if reasons else None)


def guard_signature(
    view: TreeView,
    guard: Guard,
    *,
    enforce_presence: bool,
    base_signature: tuple[Entry, ...] | None = None,
) -> tuple[Entry, ...]:
    paths = view.matching_paths(guard.paths)
    elide_fn = ELISIONS.get(guard.elide) if guard.elide else None

    def apply_elision(path: str, name: str, value: str) -> str:
        if elide_fn is None or base_signature is None:
            return value
        same_name = [entry for entry in base_signature if entry[1] == name]
        same_path = [entry for entry in same_name if entry[0] == path]
        candidates = same_path or same_name
        return elide_fn(value, candidates[0][2] if len(candidates) == 1 else "")

    signature: list[Entry] = []
    found_symbols: set[str] = set()
    covered: set[str] = set()
    for path in paths:
        content = view.projected(path)
        if content is None:
            continue
        if guard.kind == "file":
            signature.append((path, "", apply_elision(path, "", content)))
            covered.update(marker for marker in guard.must_cover if marker in content)
            continue
        if guard.kind == "items":
            items = named_rust_items(content, guard.symbols)
            found_symbols.update(name.partition("#")[0] for name in items)
        elif guard.kind == "impls":
            items = named_rust_serde_impls(content, guard.symbols)
            found_symbols.update(items)
        else:
            items = serde_shapes(content)
            covered.update(name.partition("#")[0] for name in items)
        signature.extend(
            (path, name, apply_elision(path, name, value))
            for name, value in items.items()
        )

    if enforce_presence:
        where = f"{view.label}: {guard.label}"
        if not paths:
            raise CheckError(f"{where} matches no file")
        if guard.kind in {"items", "impls"}:
            missing = sorted(set(guard.symbols) - found_symbols)
            if missing:
                raise CheckError(f"{where} does not find " + ", ".join(missing))
        else:
            if guard.kind == "shapes" and not signature:
                raise CheckError(f"{where} finds no Serde-derived shape")
            missing = sorted(set(guard.must_cover) - covered)
            if missing:
                raise CheckError(f"{where} does not cover " + ", ".join(missing))
    return tuple(sorted(signature))


def comparable(signature: Iterable[Entry]) -> frozenset[tuple[str, str]]:
    """What two trees are compared on: each guarded item's name and projection.

    The path is left out, so a shape that moves to another file compares
    equal, and so does one that two guards both cover.
    """
    return frozenset((name.partition("#")[0], value) for _, name, value in signature)


def changed_names(base: Iterable[Entry], head: Iterable[Entry]) -> list[str]:
    """The guarded items present, or projected, differently on the two sides."""
    base, head = tuple(base), tuple(head)
    differing = comparable(base) ^ comparable(head)
    return sorted(
        {
            name.partition("#")[0] or path
            for path, name, value in (*base, *head)
            if (name.partition("#")[0], value) in differing
        }
    )


def surface_fingerprint(entries: Iterable[Entry]) -> str:
    """A digest of the guarded bytes, printed so two runs can be compared."""
    digest = hashlib.sha256()
    for name, value in sorted(comparable(entries)):
        digest.update(name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(value.encode("utf-8", errors="surrogateescape"))
        digest.update(b"\0")
    return f"sha256:{digest.hexdigest()}"


def _default_build_value(text: str, name: str, where: str) -> str:
    definitions = constant_definitions(text, name)
    if len(definitions) > 1:
        definitions = [
            (attributes, value)
            for attributes, value in definitions
            if SYNTHETIC_NEXT_CFG not in strip_rust_trivia(attributes)
        ]
    if len(definitions) != 1:
        raise CheckError(
            f"{where}: expected one default-build definition of {name}, "
            f"found {len(definitions)}"
        )
    return definitions[0][1]


def version_at(view: TreeView, surface: Surface) -> int:
    """The default build's value of the surface's constant.

    The value is an integer literal, another constant of the same file that
    resolves to one, or a string whose trailing digits are the version
    (`"lashlang-vm-abi-v14"`).
    """
    content = view.content(surface.constant_path)
    where = f"{view.label}: {surface.constant_path}"
    if content is None:
        raise CheckError(f"{where} cannot be read for {surface.constant}")
    name = surface.constant
    for _ in range(4):
        value = _default_build_value(content, name, where)
        if re.fullmatch(r"[0-9][0-9_]*", value):
            return int(value.replace("_", ""))
        tagged = re.fullmatch(r'"[^"\\]*?([0-9]+)"', value)
        if tagged is not None:
            return int(tagged.group(1))
        if re.fullmatch(r"[A-Z][A-Z0-9_]*", value) is None:
            break
        name = value
    raise CheckError(
        f"{where}: {surface.constant} does not resolve to an integer version"
    )


def surface_of(raw: dict, location: str) -> Surface:
    """One `[[surface]]` table of the registry."""
    constant = raw.get("constant")
    constant_path = raw.get("constant_path")
    upgrade = raw.get("upgrade")
    if not all(
        isinstance(value, str) and value for value in (constant, constant_path, upgrade)
    ):
        raise CheckError(f"{location} needs constant, constant_path and upgrade")
    return Surface(
        constant,
        constant_path,
        upgrade,
        decoder_laws=upgrade == "migrate" and "unguarded" not in raw,
    )


def load_surfaces(text: str, where: str) -> tuple[Surface, ...]:
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise CheckError(f"cannot read {where}: {error}") from error
    raw_surfaces = document.get("surface")
    if not isinstance(raw_surfaces, list) or not raw_surfaces:
        raise CheckError(f"{where}: expected at least one [[surface]] entry")
    surfaces: list[Surface] = []
    seen: set[str] = set()
    for index, raw in enumerate(raw_surfaces, start=1):
        location = f"{where}: surface {index}"
        if not isinstance(raw, dict):
            raise CheckError(f"{location} must be a table")
        surface = surface_of(raw, location)
        if surface.key in seen:
            raise CheckError(f"{location} duplicates {surface.key}")
        seen.add(surface.key)
        surfaces.append(surface)
    return tuple(surfaces)


def registered_lifts(view: TreeView) -> set[tuple[str, int]]:
    """`(constant, from_version)` for every row of the default build's
    `RECORD_UPCASTERS` table."""
    where = f"{view.label}: {UPCASTER_REGISTRY}"
    content = view.content(UPCASTER_REGISTRY)
    if content is None:
        raise CheckError(f"{where} cannot be read for {UPCASTER_TABLE}")
    table = strip_rust_trivia(_default_build_value(content, UPCASTER_TABLE, where))
    if not (table.startswith("&[") and table.endswith("]")):
        raise CheckError(
            f"{where}: {UPCASTER_TABLE} must be an inline array of RecordUpcaster "
            "rows so its lifts can be read"
        )
    rows = re.findall(r'constant:"(\w+)",from_version:([0-9_]+),', table)
    if len(rows) != table.count("RecordUpcaster{"):
        raise CheckError(
            f"{where}: keep each {UPCASTER_TABLE} row as RecordUpcaster "
            '{ constant: "..", from_version: N, .. }'
        )
    return {(constant, int(version.replace("_", ""))) for constant, version in rows}


@dataclass(frozen=True)
class Finding:
    surface: Surface
    detail: str


@dataclass(frozen=True)
class CheckResult:
    evaluated: tuple[Surface, ...]
    unshaped: tuple[Surface, ...]
    failures: tuple[Finding, ...]
    errors: tuple[Finding, ...]
    bumped: tuple[Finding, ...] = ()
    registered: tuple[Surface, ...] = ()


def _declaration(view: TreeView, surface: Surface) -> Declaration | None:
    content = view.content(surface.constant_path)
    if content is None:
        raise CheckError(
            f"{view.label}: cannot read {surface.constant_path} for {surface.constant}"
        )
    return declaration_in(content, surface)


def _compare(
    base: TreeView,
    head: TreeView,
    head_guards: tuple[Guard, ...],
    base_guards: tuple[Guard, ...] = (),
) -> tuple[list[str], list[Entry]]:
    """The guarded items that differ between the two trees, and the head's
    guarded entries.

    Both trees are projected under the head's guards and the base's together:
    the head's alone would let a marker drop a shape and change it in one
    commit, and the base's alone would miss a shape the head newly guards.
    """
    base_entries: list[Entry] = []
    head_entries: list[Entry] = []
    declared: list[Entry] = []
    for guard in dict.fromkeys((*head_guards, *base_guards)):
        base_signature = guard_signature(base, guard, enforce_presence=False)
        head_signature = guard_signature(
            head,
            guard,
            enforce_presence=guard in head_guards,
            base_signature=base_signature,
        )
        base_entries.extend(base_signature)
        head_entries.extend(head_signature)
        if guard in head_guards:
            declared.extend(head_signature)
    return changed_names(base_entries, head_entries), declared


def _base_surface(
    surface: Surface, base_surfaces: dict[str, Surface], head_keys: set[str]
) -> Surface | None:
    """The base surface `surface` continues: the same key, or the one base
    surface of the same constant name whose own key is gone (a relocation)."""
    if surface.key in base_surfaces:
        return base_surfaces[surface.key]
    moved = [
        candidate
        for key, candidate in base_surfaces.items()
        if candidate.constant == surface.constant and key not in head_keys
    ]
    return moved[0] if len(moved) == 1 else None


def check_surfaces(repo: Path, base: str, head: str) -> CheckResult:
    base_view = RevisionView(repo, resolve_revision(repo, base))
    head_view = RevisionView(repo, resolve_revision(repo, head))
    head_registry = head_view.content(REGISTRY)
    if head_registry is None:
        raise CheckError(f"{head_view.label}: cannot read {REGISTRY}")
    surfaces = load_surfaces(head_registry, f"{head_view.label}:{REGISTRY}")
    base_registry = base_view.content(REGISTRY)
    base_surfaces = (
        {}
        if base_registry is None
        else {
            surface.key: surface
            for surface in load_surfaces(base_registry, f"{base_view.label}:{REGISTRY}")
        }
    )
    head_keys = {surface.key for surface in surfaces}

    evaluated: list[Surface] = []
    unshaped: list[Surface] = []
    failures: list[Finding] = []
    errors: list[Finding] = []
    bumped: list[Finding] = []
    registered: list[Surface] = []
    continued: set[str] = set()
    lifts: set[tuple[str, int]] | None = None

    for surface in surfaces:
        try:
            declaration = _declaration(head_view, surface)
            if declaration is None:
                raise CheckError(
                    f"{surface.constant} declares no {MARKER} marker: name the "
                    f"shapes it guards on the constant, or state "
                    f'{MARKER}(unshaped = "<reason>")'
                )
            head_version = version_at(head_view, surface)
            previous = _base_surface(surface, base_surfaces, head_keys)
            base_declaration = None
            if previous is not None:
                # A base marker that cannot be read binds nothing: the head's
                # guards still project both trees, and the commit that repairs
                # the marker can pass.
                with contextlib.suppress(CheckError):
                    base_declaration = _declaration(base_view, previous)
            changed, head_entries = _compare(
                base_view,
                head_view,
                declaration.guards,
                () if base_declaration is None else base_declaration.guards,
            )
            if previous is None:
                registered.append(surface)
            else:
                continued.add(previous.key)
                base_version = version_at(base_view, previous)
                if head_version < base_version:
                    failures.append(
                        Finding(
                            surface,
                            f"{surface.constant} moved backwards, "
                            f"{base_version} to {head_version}",
                        )
                    )
                elif changed and head_version == base_version:
                    failures.append(
                        Finding(
                            surface,
                            f"{surface.constant} is {head_version} on both sides but "
                            f"its guarded shape changed ({', '.join(changed)}; head "
                            f"{surface_fingerprint(head_entries)}). Bump "
                            f"{surface.constant} strictly past {base_version}.",
                        )
                    )
                elif head_version > base_version:
                    missing: list[int] = []
                    if surface.decoder_laws:
                        if lifts is None:
                            lifts = registered_lifts(head_view)
                        missing = [
                            version
                            for version in range(base_version, head_version)
                            if (surface.constant, version) not in lifts
                        ]
                    if missing:
                        failures.append(
                            Finding(
                                surface,
                                f"{surface.constant} moved {base_version} to "
                                f"{head_version} without its upgrade evidence: "
                                f"register a RecordUpcaster row in {UPCASTER_TABLE} "
                                f"({UPCASTER_REGISTRY}) lifting from version "
                                + ", ".join(str(version) for version in missing),
                            )
                        )
                    else:
                        evidence = (
                            "lift rows registered"
                            if surface.decoder_laws
                            else f"{surface.upgrade} surface, the bump is the evidence"
                        )
                        bumped.append(
                            Finding(
                                surface,
                                f"{base_version} to {head_version} ({evidence})",
                            )
                        )
        except CheckError as error:
            errors.append(Finding(surface, str(error)))
            continue
        evaluated.append(surface)
        if declaration.unshaped is not None:
            unshaped.append(surface)

    # A constant that disappears does not take its shapes' obligations with it.
    for key, surface in base_surfaces.items():
        if key in head_keys or key in continued:
            continue
        try:
            declaration = _declaration(base_view, surface)
            if declaration is None:
                continue
            base_entries: list[Entry] = []
            head_entries = []
            for guard in declaration.guards:
                base_signature = guard_signature(
                    base_view, guard, enforce_presence=False
                )
                base_entries.extend(base_signature)
                head_entries.extend(
                    guard_signature(
                        head_view,
                        guard,
                        enforce_presence=False,
                        base_signature=base_signature,
                    )
                )
            # Shapes deleted with their constant owe nothing; shapes that stay
            # must stay as they were.
            surviving = {name for name, _ in comparable(head_entries)}
            changed = [
                name
                for name in changed_names(base_entries, head_entries)
                if name in surviving or any(path == name for path, _, _ in head_entries)
            ]
        except CheckError as error:
            errors.append(Finding(surface, str(error)))
            continue
        if changed:
            failures.append(
                Finding(
                    surface,
                    f"{surface.constant} left the registry while the shapes it "
                    f"guarded changed ({', '.join(changed)}); keep the constant "
                    "and bump it",
                )
            )

    return CheckResult(
        tuple(evaluated),
        tuple(unshaped),
        tuple(failures),
        tuple(errors),
        tuple(bumped),
        tuple(registered),
    )


def worktree_problems(repo: Path, surfaces: Iterable[Surface]) -> list[str]:
    """What stops the working tree's markers from being evaluated.

    `check_format_registry.py` reports these, so a surface cannot land without
    a guard, or with one that names something the tree does not have.
    """
    view = WorktreeView(repo)
    problems: list[str] = []
    for surface in surfaces:
        try:
            declaration = _declaration(view, surface)
            if declaration is None:
                problems.append(
                    f"{surface.key} declares no {MARKER} marker: name the shapes it "
                    f"guards in a `/// {MARKER}(..)` doc comment on the constant, or "
                    f'state {MARKER}(unshaped = "<reason>")'
                )
                continue
            version_at(view, surface)
            for guard in declaration.guards:
                guard_signature(view, guard, enforce_presence=True)
        except CheckError as error:
            problems.append(f"{surface.key} has a guard that cannot be evaluated: {error}")
    return problems


def guarded_path_patterns(repo: Path) -> frozenset[str]:
    """Every path a change to a registered surface can touch, as the working
    tree declares it: the registry, each constant's file, and each guard's
    paths (globs included). CI selects the gate with it."""
    view = WorktreeView(repo)
    registry = view.content(REGISTRY)
    if registry is None:
        raise CheckError(f"cannot read {REGISTRY}")
    patterns = {REGISTRY, UPCASTER_REGISTRY}
    for surface in load_surfaces(registry, REGISTRY):
        patterns.add(surface.constant_path)
        declaration = _declaration(view, surface)
        if declaration is not None:
            for guard in declaration.guards:
                patterns.update(guard.paths)
    return frozenset(patterns)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--base", required=True, help="baseline commit")
    parser.add_argument("--head", required=True, help="candidate commit")
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        base = resolve_revision(args.repo, args.base)
        head = resolve_revision(args.repo, args.head)
        result = check_surfaces(args.repo, base, head)
    except CheckError as error:
        print(f"version-bump check error: {error}", file=sys.stderr)
        return 2

    total = len(result.evaluated) + len(result.errors)
    summary = (
        f"{len(result.evaluated)} of {total} surfaces evaluated "
        f"({len(result.evaluated) - len(result.unshaped)} guarded, "
        f"{len(result.unshaped)} unshaped with a stated reason), "
        f"{len(result.errors)} not evaluated; base {base[:12]}, head {head[:12]}"
    )
    for finding in result.bumped:
        print(f"bumped: {finding.surface.key} {finding.detail}")
    for surface in result.registered:
        print(f"registered by this change: {surface.key}")
    if result.errors:
        print("version-bump check could not evaluate:", file=sys.stderr)
        for finding in result.errors:
            print(f"- {finding.surface.key}: {finding.detail}", file=sys.stderr)
    if result.failures:
        print("version-bump check failed:", file=sys.stderr)
        for finding in result.failures:
            print(f"- {finding.surface.key}: {finding.detail}", file=sys.stderr)
    if result.errors:
        print(f"version-bump check error: {summary}", file=sys.stderr)
        return 2
    if result.failures:
        print(f"version-bump check failed: {summary}", file=sys.stderr)
        return 1
    print(f"version-bump check passed: {summary}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
