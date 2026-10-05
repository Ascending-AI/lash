#!/usr/bin/env python3
"""Check guarded shapes and their version/upgrade evidence across two commits.

The inventory reads constants declaring ``/// version_surface = "coexist"``
in source. Declarations use ``migrate``, ``drain`` and ``coexist`` policies.
The shapes that constant guards are declared in code, on the
constant itself, by a marker in its doc comment::

    /// The demo record's stored format.
    ///
    /// version_guard(
    ///     roots(Request),
    ///     roots(path = "crates/demo/src/wire.rs", Envelope),
    ///     items(encode_request),
    ///     shapes(path = "crates/demo/src/dto/*.rs", cover(Reply)),
    ///     impls("Serialize for Request"),
    ///     file(path = "crates/demo/schema.sql", cover("CREATE TABLE demo")),
    ///     catalog(path = "crates/demo/src/migrate.rs", MIGRATIONS),
    /// )
    pub const DEMO_VERSION: u32 = 3;

The marker is documentation, not an attribute: it states what the version
versions, no build, lint or feature-gate check reads it, and this script and
``check_format_registry.py`` are its only readers. It sits in the constant's
own block of doc comments and attributes, with no blank line between that
block and the ``const``.

- ``roots`` names the types the format starts from, its envelope or its
  persisted record, and guards every shape reachable from them: the gate
  walks their Serde-visible fields, container arguments and aliases to a
  closure (see "Reachability" below), so a marker never lists leaf types;
- ``items`` guards the named declarations (``const``, ``static``, ``fn``,
  ``struct``, ``enum``, ``type``), comments and formatting ignored. It is for
  what no type reaches: encoders, tags, tables, and types under a
  hand-written impl. It is not followed;
- ``shapes`` guards every Serde-derived struct and enum in the matched files
  and, like ``roots``, everything they reach; ``cover(..)`` names shapes that
  must be among them;
- ``impls`` guards hand-written ``Serialize for T`` / ``Deserialize for T``
  impls. The impls of a reachable type are guarded without being named;
- ``file`` guards whole files, and ``cover(..)`` names text they must contain;
- ``catalog`` guards nothing: it names the migration catalog a DDL stamp's bump
  must extend (below), and ``rows = ".."`` keeps only the rows that contain that
  text when one table serves several stamps.

A guard without ``path`` reads the file that defines the constant. A path is
repository-relative and may be a glob. A surface that versions no shape the
tree can project says so instead: ``version_guard(unshaped = "<reason>")``.

The gate compares two commits. For each surface it projects the guarded
shapes at ``--base`` and at ``--head``; in strict mode, when they differ, the head's constant
must be strictly greater than the base's, and the bump must carry its upgrade
evidence:

- a surface held to the decoder laws (``upgrade = "migrate"`` without
  ``unguarded``) registers a ``RecordUpcaster`` row for every version it steps
  over;
- a DDL stamp, a surface that declares a ``catalog``, has in that catalog's
  default build a chain of steps from the base's version to the head's, in
  the constant's own numbers. The 1.0 cut resets every stamp to 1 and empties
  the catalogs, so from the cut on a stamp's value is its catalog's version;
- for every other surface the bump is the evidence.

The shapes are projected under the head's markers and under the base's, so
dropping a shape from a marker does not excuse changing it, a constant that
disappears does not excuse the shapes it guarded, and dropping a ``catalog``
does not excuse the step.

Before the 1.0 cut (FIG-3846), an unchanged version with a changed shape is
reported with the head shape hash and does not fail. FIG-4494 flips
``STRICT_VERSION_BUMPS`` at the cut; ``--strict`` exercises that policy now.
Strict mode requires the bump. In both modes, a bump without its evidence,
a backwards version, and a guard that cannot be evaluated exit nonzero.
A reachable type the tree cannot resolve is such a guard.
``version_guard_closure.py`` prints each surface's closure, its cycles and
what the walk leaves opaque.

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
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Iterable


ROOT = Path(__file__).resolve().parents[1]
REGISTRY = "scripts/versioned-surfaces.toml"
MARKER = "version_guard"
GUARD_KINDS = ("items", "shapes", "impls", "file", "roots")
# FIG-4494 flips this at the 1.0 cut for every caller, including CI.
STRICT_VERSION_BUMPS = False
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
        self._reachability: Reachability | None = None
        self._closures: dict[Guard, tuple[Closure, list[str]]] = {}

    def preload(self, paths: Iterable[str]) -> None:
        """Read `paths` ahead of use, where one read is cheaper than many."""

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

    def preload(self, paths: Iterable[str]) -> None:
        wanted = [path for path in paths if path not in self._contents]
        if not wanted:
            return
        request = "".join(f"{self.revision}:{path}\n" for path in wanted).encode()
        completed = subprocess.run(
            ["git", "cat-file", "--batch"], cwd=self.repo, input=request, capture_output=True
        )
        if completed.returncode != 0:
            detail = completed.stderr.decode(errors="replace").strip()
            raise CheckError(f"git cat-file --batch failed: {detail}")
        output = completed.stdout
        cursor = 0
        for path in wanted:
            line_end = output.index(b"\n", cursor)
            header = output[cursor:line_end].split()
            cursor = line_end + 1
            if header[-1] == b"missing":
                self._contents[path] = None
                continue
            size = int(header[2])
            blob = output[cursor : cursor + size]
            cursor += size + 1
            self._contents[path] = (
                blob.decode("utf-8", errors="surrogateescape") if header[1] == b"blob" else None
            )


class WorktreeView(TreeView):
    """The files on disk, so the registry check reads what a commit would."""

    label = "working tree"

    def __init__(self, repo: Path) -> None:
        super().__init__()
        self.repo = repo
        self._contents: dict[str, str | None] = {}
        self._listings: dict[str, tuple[str, ...]] = {}

    def _listing(self, prefix: str) -> tuple[str, ...]:
        """Every file under `prefix`, build output and vendored trees left out."""
        if prefix not in self._listings:
            found: list[str] = []
            base = self.repo / prefix
            for directory, names, files in os.walk(base):
                names[:] = [
                    name
                    for name in names
                    if name not in {"target", "node_modules"} and not name.startswith(".")
                ]
                relative = os.path.relpath(directory, self.repo).replace(os.sep, "/")
                stem = "" if relative == "." else relative + "/"
                found.extend(stem + name for name in files)
            self._listings[prefix] = tuple(found)
        return self._listings[prefix]

    def matching_paths(self, patterns: Iterable[str]) -> tuple[str, ...]:
        matches: set[str] = set()
        for pattern in patterns:
            wildcard = re.search(r"[*?\[]", pattern)
            if wildcard is None:
                if (self.repo / pattern).is_file():
                    matches.add(pattern)
                continue
            prefix = pattern[: wildcard.start()].rpartition("/")[0]
            if not (self.repo / prefix).is_dir():
                continue
            matches.update(
                relative
                for relative in self._listing(prefix)
                if fnmatch.fnmatchcase(relative, pattern)
            )
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
class Catalog:
    """The migration catalog a DDL stamp's bump must extend."""

    path: str
    table: str
    rows: str | None = None

    @property
    def label(self) -> str:
        return f"{self.table} ({self.path})"


@dataclass(frozen=True)
class Declaration:
    """What one surface's constant declares it guards."""

    guards: tuple[Guard, ...] = ()
    unshaped: str | None = None
    catalogs: tuple[Catalog, ...] = ()


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
        catalogs: list[Catalog] = []
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
            elif name == "catalog":
                catalogs.append(self.catalog())
            else:
                raise CheckError(
                    f"{MARKER} marker has unknown entry {name!r}; known: "
                    + ", ".join((*GUARD_KINDS, "catalog", "unshaped"))
                )

        self.list_items(entry)
        if self.index != len(self.tokens):
            raise CheckError(f"{MARKER} marker has trailing tokens")
        if len(reasons) > 1 or (reasons and (guards or catalogs)):
            raise CheckError(
                f"{MARKER} marker states unshaped beside other entries; a surface "
                "either declares guards or says why it has none"
            )
        if not reasons and not guards:
            raise CheckError(f"{MARKER} marker declares nothing")
        return Declaration(
            tuple(guards), reasons[0] if reasons else None, tuple(catalogs)
        )

    def catalog(self) -> Catalog:
        paths: list[str] = []
        rows: list[str] = []
        tables: list[str] = []

        def argument() -> None:
            value = self.take("ident")
            if value in {"path", "rows"} and self.at("punct", "="):
                self.take("punct", "=")
                (paths if value == "path" else rows).append(self.take("string"))
            else:
                tables.append(value)

        self.list_items(argument)
        if len(tables) != 1:
            raise CheckError(f"{MARKER} catalog(..) names exactly one table")
        if len(paths) > 1 or len(rows) > 1 or not all((*paths, *rows)):
            raise CheckError(
                f"{MARKER} catalog(..) takes at most one non-empty path and rows"
            )
        return Catalog(
            paths[0] if paths else self.constant_path,
            tables[0],
            rows[0] if rows else None,
        )

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
        if kind in {"items", "impls", "roots"} and not symbols:
            raise CheckError(f"{MARKER} {kind}(..) names nothing to guard")
        if kind in {"shapes", "file"} and symbols:
            raise CheckError(
                f"{MARKER} {kind}(..) takes path, cover and elide, not "
                + ", ".join(symbols)
            )
        if kind in {"items", "impls", "roots"} and cover:
            raise CheckError(f"{MARKER} {kind}(..) does not take cover")
        if kind == "roots" and elide:
            raise CheckError(f"{MARKER} roots(..) does not take elide")
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
    catalogs: list[Catalog] = []
    reasons: list[str] = []
    for block, _ in definitions:
        try:
            for _, _, body in marker_blocks(block):
                parsed = _MarkerParser(
                    _marker_tokens(body), surface.constant_path
                ).parse()
                guards.extend(parsed.guards)
                catalogs.extend(parsed.catalogs)
                if parsed.unshaped is not None:
                    reasons.append(parsed.unshaped)
        except CheckError as error:
            raise CheckError(f"{surface.key}: {error}") from error
    if not guards and not reasons:
        return None
    if len(reasons) > 1 or (reasons and (guards or catalogs)):
        raise CheckError(
            f"{surface.key}: states unshaped beside other {MARKER} entries"
        )
    return Declaration(
        tuple(guards), reasons[0] if reasons else None, tuple(catalogs)
    )


# --- Reachability ------------------------------------------------------------
#
# A `roots(..)` guard names the types a format starts from: its envelope or its
# persisted record. Everything those types serialize is guarded with them. The
# walk below computes that closure from the tree: from each root it follows the
# Serde-visible fields of structs and enums, the generic arguments of the
# containers they sit in, and type aliases, resolving each name the way the
# file that writes it does (its own definitions, its `use` imports, then the
# crate).
#
# - a `#[serde(skip)]` field is not followed;
# - `from`, `try_from` and `into` name the type the shape serializes as, and it
#   is followed;
# - a `with`, `serialize_with` or `deserialize_with` adapter of this repository
#   is guarded as text, and the field's type is still followed;
# - a type whose Serde impls are hand-written is guarded with those impls and
#   is not followed further: its fields do not say what it writes;
# - a type declared by a `macro_rules!` invocation is guarded as the invocation
#   and the macro;
# - a type of another package, an external adapter and an associated type are
#   opaque: reported, and not followed;
# - a name that resolves to nothing, or to several definitions, is an error.
#   The guard cannot be evaluated until the code or the marker says which type
#   is meant.

RUST_TYPE_DEFINITION = re.compile(
    r"(?m)^([ \t]*)(?:pub(?:\([^)]*\))?[ \t]+)?(struct|enum|union|type)[ \t]+"
    r"([A-Za-z_][A-Za-z0-9_]*)\b"
)
RUST_USE = re.compile(r"(?m)^[ \t]*(pub(?:\([^)]*\))?[ \t]+)?use[ \t]+([^;]+);")
RUST_MODULE_OPENING = re.compile(
    r"(?m)^([ \t]*)(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*\{"
)
RUST_MODULE_FILE = re.compile(
    r"(?m)^(?:[ \t]*#\[path[ \t]*=[ \t]*\"([^\"]+)\"\][ \t]*\n)?"
    r"[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*;"
)
RUST_MACRO_DEFINITION = re.compile(r"(?m)^[ \t]*macro_rules![ \t]+([A-Za-z_][A-Za-z0-9_]*)\b")
RUST_MACRO_CALL = re.compile(r"(?m)^([ \t]*)([A-Za-z_][A-Za-z0-9_]*)![ \t]*[({\[]")
RUST_IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
RUST_NUMBER = re.compile(r"[0-9][0-9A-Za-z_]*(?:\.[0-9][0-9A-Za-z_]*)?")

# Names every Rust file has without importing them.
RUST_PRELUDE_TYPES = frozenset(
    "bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 "
    "String Vec Option Box Result".split()
)
RUST_STANDARD_CRATES = frozenset({"std", "core", "alloc"})
RUST_TYPE_KEYWORDS = frozenset({"mut", "const", "unsafe", "extern", "fn", "for", "Self", "as"})
RUST_SOURCE_PATTERNS = ("crates/*.rs", "examples/*.rs")
CARGO_MANIFEST_PATTERNS = ("Cargo.toml", "crates/*Cargo.toml", "examples/*Cargo.toml", "runbooks/*Cargo.toml")
MAX_REPORTED_PROBLEMS = 12

Token = tuple[str, str]


@lru_cache(maxsize=None)
def rust_tokens(text: str) -> tuple[Token, ...]:
    """Split Rust source into identifiers, lifetimes, literals and punctuation."""
    tokens: list[Token] = []
    index = 0
    while index < len(text):
        char = text[index]
        if char.isspace():
            index += 1
        elif text.startswith("//", index) or text.startswith("/*", index):
            skipped = _rust_trivia_end(text, index)
            if skipped is None:
                raise CheckError("unterminated Rust comment while reading a guarded shape")
            index = skipped
        elif raw := _raw_string_start(text, index):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            if closing < 0:
                raise CheckError("unterminated Rust raw string while reading a guarded shape")
            tokens.append(("literal", text[content_start:closing]))
            index = closing + len(closer)
        elif char == '"' or text.startswith('b"', index):
            start = index = index + (2 if char == "b" else 1)
            while index < len(text) and text[index] != '"':
                index += 2 if text[index] == "\\" else 1
            tokens.append(("literal", text[start:index]))
            index += 1
        elif char_end := _char_literal_end(text, index):
            tokens.append(("char", text[index:char_end]))
            index = char_end
        elif char == "'" and (name := RUST_IDENTIFIER.match(text, index + 1)):
            tokens.append(("lifetime", name.group(0)))
            index = name.end()
        elif name := RUST_IDENTIFIER.match(text, index):
            tokens.append(("ident", name.group(0)))
            index = name.end()
        elif number := RUST_NUMBER.match(text, index):
            tokens.append(("number", number.group(0)))
            index = number.end()
        elif text.startswith(("::", "->", "=>"), index):
            tokens.append(("punct", text[index : index + 2]))
            index += 2
        else:
            tokens.append(("punct", char))
            index += 1
    return tuple(tokens)


@dataclass(frozen=True)
class TypeRef:
    """One type name as a field writes it."""

    segments: tuple[str, ...]
    absolute: bool = False
    associated: bool = False

    @property
    def label(self) -> str:
        return ("::" if self.absolute else "") + "::".join(self.segments)


def type_refs(
    tokens: Iterable[Token], generics: frozenset[str] = frozenset()
) -> tuple[TypeRef, ...]:
    """Every type a type expression names: the type itself and the generic
    arguments of its containers. Trait objects, lifetimes, array lengths and
    the item's own generic parameters name no shape."""
    tokens = tuple(tokens)
    refs: list[TypeRef] = []
    index = 0
    while index < len(tokens):
        kind, value = tokens[index]
        if (kind, value) == ("punct", ";"):
            # `[T; N]`: the length is an expression.
            depth = 0
            while index < len(tokens):
                if tokens[index] == ("punct", "["):
                    depth += 1
                elif tokens[index] == ("punct", "]"):
                    if depth == 0:
                        break
                    depth -= 1
                index += 1
            continue
        if kind != "ident":
            index += 1
            continue
        segments = [value]
        end = index + 1
        while (
            end + 1 < len(tokens)
            and tokens[end] == ("punct", "::")
            and tokens[end + 1][0] == "ident"
        ):
            segments.append(tokens[end + 1][1])
            end += 2
        before = tokens[index - 1] if index else ("", "")
        before2 = tokens[index - 2] if index > 1 else ("", "")
        after = tokens[end] if end < len(tokens) else ("", "")
        trait = before in {("ident", "dyn"), ("ident", "impl"), ("punct", "+")}
        index = end
        if value in {"dyn", "impl"} or (len(segments) == 1 and value in RUST_TYPE_KEYWORDS):
            continue
        if trait or (after == ("punct", "=") and len(segments) == 1):
            # A trait bound, or the name of an associated-type binding.
            continue
        if before == ("punct", "::"):
            if before2 == ("punct", ">"):
                refs.append(TypeRef(tuple(segments), associated=True))
            else:
                refs.append(TypeRef(tuple(segments), absolute=True))
            continue
        if value in generics:
            if len(segments) > 1:
                refs.append(TypeRef(tuple(segments), associated=True))
            continue
        refs.append(TypeRef(tuple(segments)))
    return tuple(refs)


class _ItemParser:
    """Reads one struct, enum, union or type alias from its tokens."""

    OPENING = {"(": ")", "[": "]", "{": "}", "<": ">"}

    def __init__(self, tokens: tuple[Token, ...]) -> None:
        self.tokens = tokens
        self.index = 0

    def peek(self, ahead: int = 0) -> Token:
        at = self.index + ahead
        return self.tokens[at] if at < len(self.tokens) else ("end", "")

    def at(self, value: str, ahead: int = 0) -> bool:
        return self.peek(ahead) == ("punct", value)

    def attributes(self) -> list[tuple[Token, ...]]:
        found: list[tuple[Token, ...]] = []
        while self.at("#") and self.at("[", 1):
            self.index += 2
            start = self.index
            depth = 1
            while depth and self.index < len(self.tokens):
                if self.at("["):
                    depth += 1
                elif self.at("]"):
                    depth -= 1
                self.index += 1
            found.append(self.tokens[start : self.index - 1])
        return found

    def visibility(self) -> None:
        if self.peek() != ("ident", "pub"):
            return
        self.index += 1
        if self.at("(") and self.peek(1)[1] in {"crate", "self", "super", "in"}:
            self.group()

    def group(self) -> tuple[Token, ...]:
        """The tokens inside the delimiters that open here."""
        closer = self.OPENING[self.peek()[1]]
        opener = self.peek()[1]
        self.index += 1
        start = self.index
        depth = 1
        while self.index < len(self.tokens):
            if self.at(opener):
                depth += 1
            elif self.at(closer):
                depth -= 1
                if depth == 0:
                    break
            self.index += 1
        inner = self.tokens[start : self.index]
        self.index += 1
        return inner

    def until_comma(self, *, angles: bool = True) -> tuple[Token, ...]:
        """The tokens up to the next comma of this nesting level."""
        start = self.index
        stack: list[str] = []
        while self.index < len(self.tokens):
            kind, value = self.peek()
            if kind == "punct":
                if value == "," and not stack:
                    break
                if value in self.OPENING and (angles or value != "<"):
                    stack.append(self.OPENING[value])
                elif stack and value == stack[-1]:
                    stack.pop()
            self.index += 1
        found = self.tokens[start : self.index]
        self.index += 1
        return found


def serde_arguments(attributes: Iterable[tuple[Token, ...]]) -> list[tuple[str, str | None]]:
    """`key` and `key = "value"` of every `serde(..)` attribute, `cfg_attr`
    ones included."""
    found: list[tuple[str, str | None]] = []
    for attribute in attributes:
        for index, token in enumerate(attribute):
            if token != ("ident", "serde") or attribute[index + 1 : index + 2] != (("punct", "("),):
                continue
            parser = _ItemParser(attribute[index + 1 :])
            inner = _ItemParser(parser.group())
            while inner.index < len(inner.tokens):
                argument = inner.until_comma(angles=False)
                if not argument or argument[0][0] != "ident":
                    continue
                value = None
                if argument[1:2] == (("punct", "="),) and argument[2:3]:
                    value = argument[2][1] if argument[2][0] == "literal" else None
                found.append((argument[0][1], value))
    return found


def _attribute_text(attribute: tuple[Token, ...]) -> str:
    body = "".join(f'"{value}"' if kind == "literal" else value for kind, value in attribute)
    return f"#[{body}]"


def _outside_default_build(attributes: Iterable[tuple[Token, ...]]) -> bool:
    """Whether a field or variant is the upgrade harness's or a test's."""
    for attribute in attributes:
        text = _attribute_text(attribute)
        if text == SYNTHETIC_NEXT_CFG or _test_only_cfg(text):
            return True
    return False


@dataclass(frozen=True)
class ParsedShape:
    start: int
    end: int
    derives_serde: bool
    synthetic_next: bool
    refs: tuple[TypeRef, ...]
    # (`with` | `serialize_with` | `deserialize_with`, the path as written)
    adapters: tuple[tuple[str, str], ...]


@lru_cache(maxsize=None)
def parse_shape(text: str, offset: int) -> ParsedShape:
    """The types one struct, enum, union or alias serializes."""
    start = rust_item_start_with_attributes(text, offset, rust_outer_attribute_ranges(text))
    end = rust_item_end(text, offset)
    parser = _ItemParser(rust_tokens(text[start:end]))
    attributes = parser.attributes()
    derives_serde = any(
        attribute[:1] in ((("ident", "derive"),), (("ident", "cfg_attr"),))
        and any(
            kind == "ident" and value.startswith(("Serialize", "Deserialize"))
            for kind, value in attribute
        )
        for attribute in attributes
    )
    synthetic_next = any(_attribute_text(a) == SYNTHETIC_NEXT_CFG for a in attributes)
    refs: list[TypeRef] = []
    adapters: list[tuple[str, str]] = []

    parser.visibility()
    kind = parser.peek()[1]
    parser.index += 2
    generics: set[str] = set()
    if parser.at("<"):
        parameters = _ItemParser(parser.group())
        while parameters.index < len(parameters.tokens):
            parameter = [token for token in parameters.until_comma() if token != ("ident", "const")]
            if parameter and parameter[0][0] == "ident":
                generics.add(parameter[0][1])
    scope = frozenset(generics)

    def member(inner: _ItemParser, *, named: bool) -> None:
        field_attributes = inner.attributes()
        inner.visibility()
        if named:
            inner.index += 2
        tokens = inner.until_comma()
        arguments = serde_arguments(field_attributes)
        keys = {key for key, _ in arguments}
        if _outside_default_build(field_attributes):
            return
        if "skip" in keys or {"skip_serializing", "skip_deserializing"} <= keys:
            return
        refs.extend(type_refs(tokens, scope))
        adapters.extend(
            (key, value)
            for key, value in arguments
            if key in {"with", "serialize_with", "deserialize_with"} and value
        )

    def members(tokens: tuple[Token, ...], *, named: bool) -> None:
        inner = _ItemParser(tokens)
        while inner.index < len(inner.tokens):
            member(inner, named=named)

    for key, value in serde_arguments(attributes):
        if key in {"from", "try_from", "into"} and value:
            refs.extend(type_refs(rust_tokens(value), scope))

    if kind == "type":
        while parser.index < len(parser.tokens) and not parser.at("="):
            parser.index += 1
        parser.index += 1
        refs.extend(type_refs(parser.tokens[parser.index :], scope))
    elif kind == "enum":
        while parser.index < len(parser.tokens) and not parser.at("{"):
            parser.index += 1
        variants = _ItemParser(parser.group()) if parser.at("{") else _ItemParser(())
        while variants.index < len(variants.tokens):
            variant_attributes = variants.attributes()
            variants.index += 1
            body: tuple[Token, ...] = ()
            named = False
            if variants.at("(") or variants.at("{"):
                named = variants.at("{")
                body = variants.group()
            variants.until_comma(angles=False)
            keys = {key for key, _ in serde_arguments(variant_attributes)}
            if _outside_default_build(variant_attributes):
                continue
            if "skip" in keys or {"skip_serializing", "skip_deserializing"} <= keys:
                continue
            members(body, named=named)
    else:
        while parser.index < len(parser.tokens) and not (
            parser.at("{") or parser.at("(") or parser.at(";")
        ):
            parser.index += 1
        if parser.at("{") or parser.at("("):
            named = parser.at("{")
            members(parser.group(), named=named)
    return ParsedShape(
        start, end, derives_serde, synthetic_next, tuple(dict.fromkeys(refs)),
        tuple(dict.fromkeys(adapters)),
    )


def _expand_use(tree: str, prefix: tuple[str, ...] = ()) -> list[tuple[str, tuple[str, ...]]]:
    """`(bound name, path)` for every leaf of a `use` tree; a glob binds `*`."""
    tree = tree.strip()
    opening = tree.find("{")
    if opening >= 0:
        base = prefix + tuple(part.strip() for part in tree[:opening].split("::") if part.strip())
        inner = tree[opening + 1 : tree.rindex("}")]
        leaves: list[tuple[str, tuple[str, ...]]] = []
        depth = 0
        start = 0
        for index, char in enumerate(inner + ","):
            if char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
            elif char == "," and depth == 0:
                if inner[start:index].strip():
                    leaves.extend(_expand_use(inner[start:index], base))
                start = index + 1
        return leaves
    path, _, alias = re.sub(r"\s+", " ", tree).partition(" as ")
    segments = prefix + tuple(part.strip() for part in path.split("::") if part.strip())
    if not segments:
        return []
    if segments[-1] == "*":
        return [("*", segments[:-1])]
    if segments[-1] == "self":
        segments = segments[:-1]
    if not segments:
        return []
    return [(alias.strip() or segments[-1], segments)]


@dataclass(frozen=True)
class FileIndex:
    """What one Rust file declares, read without parsing its bodies."""

    # (name, kind, offset, enclosing inline modules)
    types: tuple[tuple[str, str, int, tuple[str, ...]], ...]
    # (bound name or `*`, path, is `pub use`)
    uses: tuple[tuple[str, tuple[str, ...], bool], ...]
    # (name, opening offset, end offset, enclosing inline modules)
    modules: tuple[tuple[str, int, int, tuple[str, ...]], ...]
    # `mod name;` -> the file its `#[path]` names, or "" for the usual place
    child_modules: tuple[tuple[str, str], ...]
    serde_impls: frozenset[str]
    macro_definitions: tuple[tuple[str, int], ...]
    macro_calls: tuple[tuple[str, int], ...]


def _indented_block_end(text: str, opening: re.Match[str]) -> int:
    """The end of a rustfmt-formatted block: its closing brace sits at the
    opening line's indentation."""
    if text.startswith("}", opening.end()):
        return opening.end() + 1
    closing = text.find(f"\n{opening.group(1)}}}", opening.end())
    return len(text) if closing < 0 else closing + len(opening.group(1)) + 2


def _gated_on_test(text: str, start: int) -> bool:
    """Whether the attribute lines directly above `start` gate on `test`."""
    while start > 0:
        line_start = text.rfind("\n", 0, start - 1) + 1
        line = text[line_start:start].strip()
        if not line.startswith(("#[", "//")):
            return False
        if _test_only_cfg(line):
            return True
        start = line_start
    return False


@lru_cache(maxsize=None)
def index_file(text: str) -> FileIndex:
    ranges: list[tuple[str, int, int]] = []
    excluded: list[tuple[int, int]] = []
    for opening in RUST_MODULE_OPENING.finditer(text):
        end = _indented_block_end(text, opening)
        if _gated_on_test(text, opening.start()):
            excluded.append((opening.start(), end))
        else:
            ranges.append((opening.group(2), opening.start(), end))

    def production(offset: int) -> bool:
        return not any(start <= offset < end for start, end in excluded)

    def enclosing(offset: int) -> tuple[str, ...]:
        return tuple(name for name, start, end in ranges if start < offset < end)

    types = []
    for match in RUST_TYPE_DEFINITION.finditer(text):
        chain = enclosing(match.start())
        # An item indented past its module is inside a function, impl or trait.
        if production(match.start()) and len(match.group(1)) == 4 * len(chain):
            types.append((match.group(3), match.group(2), match.start(), chain))
    uses = [
        (name, segments, match.group(1) is not None)
        for match in RUST_USE.finditer(text)
        if production(match.start())
        for name, segments in _expand_use(match.group(2))
    ]
    return FileIndex(
        tuple(types),
        tuple(uses),
        tuple((name, start, end, enclosing(start)) for name, start, end in ranges),
        tuple((m.group(2), m.group(1) or "") for m in RUST_MODULE_FILE.finditer(text)),
        frozenset(match.group(2) for match in RUST_SERDE_IMPL.finditer(text)),
        tuple((m.group(1), m.start()) for m in RUST_MACRO_DEFINITION.finditer(text)),
        tuple(
            (m.group(2), m.start())
            for m in RUST_MACRO_CALL.finditer(text)
            if production(m.start()) and len(m.group(1)) == 4 * len(enclosing(m.start()))
        ),
    )


@dataclass(frozen=True)
class Shape:
    """One member of a closure: a type definition, or the macro call that
    declares one."""

    path: str
    name: str
    offset: int
    kind: str
    modules: tuple[str, ...] = ()

    @property
    def label(self) -> str:
        return f"{self.name} ({self.path})"


@dataclass(frozen=True)
class Closure:
    """Everything a set of roots serializes."""

    entries: tuple[Entry, ...]
    depths: tuple[tuple[Shape, int], ...]
    # Each cycle as the shape names on it.
    cycles: tuple[tuple[str, ...], ...]
    # (what, why it is not followed)
    opaque: tuple[tuple[str, str], ...]
    problems: tuple[str, ...]

    @property
    def paths(self) -> frozenset[str]:
        return frozenset(path for path, _, _ in self.entries)


class _Unresolved(Exception):
    pass


# What resolving a standard-library or prelude name returns: nothing to
# follow, and, unlike an empty result, an answer.
_STANDARD: list[Shape] = []


def _is_test_source(path: str) -> bool:
    parts = path.split("/")
    return (
        "tests" in parts[:-1]
        or "benches" in parts[:-1]
        or parts[-1] in {"tests.rs", "test.rs"}
        or parts[-1].endswith("_tests.rs")
    )


class Reachability:
    """The closure walk over one tree."""

    def __init__(self, view: TreeView) -> None:
        self.view = view
        self._sources: dict[str, list[str]] | None = None
        self._crate_roots: tuple[str, ...] = ()
        # The name code writes for a crate -> its directory.
        self._crate_names: dict[str, str] = {}
        self._external: dict[str, set[str]] = {}
        self._indexed: dict[str, dict[str, FileIndex]] = {}
        self._types: dict[str, dict[str, list[Shape]]] = {}
        self._lookups: dict[
            tuple[str, tuple[str, ...]], tuple[list[Shape] | None, tuple[tuple[str, str], ...], str]
        ] = {}
        self._module_files: dict[str, dict[tuple[str, ...], str]] = {}
        self._macros: dict[str, list[tuple[str, str, int, bool, str]]] = {}
        self._crates: dict[str, str] = {}
        # crate -> bound name (or `*`) -> the `pub use` paths that bind it
        self._exports: dict[str, dict[str, list[tuple[str, tuple[str, ...]]]]] = {}
        self._macro_names: dict[str, tuple[dict[str, list[Shape]], dict[str, list[Shape]]]] = {}
        self._adapters: dict[
            tuple[str, str, str], tuple[list[Entry], list[tuple[str, str]], str]
        ] = {}
        self._edges: dict[
            Shape,
            tuple[
                tuple[Shape, ...], tuple[Entry, ...], tuple[tuple[str, str], ...], tuple[str, ...]
            ],
        ] = {}

    # -- the workspace -------------------------------------------------------

    def _load_workspace(self) -> None:
        if self._sources is not None:
            return
        manifests = self.view.matching_paths(CARGO_MANIFEST_PATTERNS)
        self.view.preload(manifests)
        roots: list[str] = []
        workspace_external: set[str] = set()
        pending: list[tuple[str, dict]] = []
        for manifest in manifests:
            text = self.view.content(manifest)
            try:
                document = tomllib.loads(text or "")
            except tomllib.TOMLDecodeError as error:
                raise CheckError(f"{self.view.label}: cannot read {manifest}: {error}") from error
            directory = manifest.rpartition("/")[0]
            if "package" in document:
                roots.append(directory)
                package = str(document["package"].get("name", ""))
                library = document.get("lib", {}).get("name") or package.replace("-", "_")
                self._crate_names.setdefault(library, directory)
            tables = [document.get("dependencies", {}), document.get("build-dependencies", {})]
            tables.append(document.get("workspace", {}).get("dependencies", {}))
            tables.extend(
                target.get("dependencies", {}) for target in document.get("target", {}).values()
            )
            for table in tables:
                pending.append((directory, table))
        for directory, table in pending:
            external = self._external.setdefault(directory, set())
            for name, spec in table.items():
                written = name.replace("-", "_")
                if isinstance(spec, dict) and "path" in spec:
                    target = "/".join(part for part in (directory, spec["path"]) if part)
                    parts: list[str] = []
                    for part in target.split("/"):
                        if part == "..":
                            parts.pop()
                        elif part != ".":
                            parts.append(part)
                    self._crate_names[written] = "/".join(parts)
                elif not (isinstance(spec, dict) and spec.get("workspace")):
                    external.add(written)
                    if directory == "":
                        workspace_external.add(written)
        for external in self._external.values():
            external.update(workspace_external)
        self._crate_roots = tuple(sorted(roots, key=len, reverse=True))
        sources: dict[str, list[str]] = {}
        for path in self.view.matching_paths((*RUST_SOURCE_PATTERNS, "runbooks/*.rs")):
            sources.setdefault(self.crate_of(path), []).append(path)
        self._sources = sources

    def crate_of(self, path: str) -> str:
        """The directory of the crate `path` belongs to."""
        if path not in self._crates:
            head, separator, _ = path.partition("/src/")
            self._crates[path] = next(
                (root for root in self._crate_roots if path.startswith(root + "/")),
                head if separator else path.rpartition("/")[0],
            )
        return self._crates[path]

    def _index(self, crate: str) -> dict[str, FileIndex]:
        """Every production source file of `crate`."""
        self._load_workspace()
        if crate not in self._indexed:
            assert self._sources is not None
            paths = [
                path
                for path in self._sources.get(crate, ())
                if path.startswith(crate + "/src/") and not _is_test_source(path)
            ]
            self.view.preload(paths)
            indexed: dict[str, FileIndex] = {}
            types: dict[str, list[Shape]] = {}
            for path in paths:
                content = self.view.projected(path)
                if content is None:
                    continue
                indexed[path] = index_file(content)
                for name, kind, offset, modules in indexed[path].types:
                    types.setdefault(name, []).append(Shape(path, name, offset, kind, modules))
            self._indexed[crate] = indexed
            self._types[crate] = types
        return self._indexed[crate]

    def _file(self, path: str) -> FileIndex:
        indexed = self._index(self.crate_of(path))
        if path not in indexed:
            # A root may live outside `src/` (a test-support fixture format).
            content = self.view.projected(path)
            if content is None:
                raise _Unresolved(f"{path} cannot be read")
            indexed[path] = index_file(content)
        return indexed[path]

    def _module_path(self, shape: Shape) -> tuple[str, ...]:
        crate = self.crate_of(shape.path)
        relative = shape.path[len(crate) + 1 :].removesuffix(".rs").split("/")
        if relative[:1] == ["src"]:
            relative = relative[1:]
        if relative[-1:] in (["mod"], ["lib"], ["main"]):
            relative = relative[:-1]
        return (*relative, *shape.modules)

    def _default_build(self, shapes: Iterable[Shape]) -> list[Shape]:
        kept = []
        for shape in shapes:
            content = self.view.projected(shape.path)
            if content is not None and not parse_shape(content, shape.offset).synthetic_next:
                kept.append(shape)
        return kept

    # -- resolution ----------------------------------------------------------

    def roots(self, path: str, name: str) -> list[Shape]:
        """The definitions of `name` in `path`."""
        try:
            index = self._file(path)
        except _Unresolved:
            return []
        direct = self._default_build(
            Shape(path, name, offset, kind, modules)
            for found, kind, offset, modules in index.types
            if found == name
        )
        if direct:
            return direct
        # Explicit roots and reachable references use the same declaration
        # lookup. Keep the path constraint: a macro call in another module
        # cannot satisfy a stale root, even when it declares the same name.
        return self._default_build(
            shape
            for shape in self._macro_declared(self.crate_of(path), name)
            if shape.path == path
        )

    def resolve(self, ref: TypeRef, origin: Shape, opaque: list[tuple[str, str]]) -> list[Shape]:
        """The definitions `ref` names where `origin` writes it. Nothing for a
        standard or opaque type; `_Unresolved` when the tree does not say."""
        if ref.associated:
            opaque.append((f"<..>::{ref.label}", f"an associated type, in {origin.label}"))
            return []
        if ref.absolute:
            return self._in_crate_named(ref.segments, origin, opaque, ())
        return self._resolve_path(ref.segments, origin.path, origin.modules, opaque, ())

    def _resolve_path(
        self,
        segments: tuple[str, ...],
        path: str,
        modules: tuple[str, ...],
        opaque: list[tuple[str, str]],
        trail: tuple[tuple[str, tuple[str, ...]], ...],
        *,
        strict: bool = False,
    ) -> list[Shape]:
        """`segments` as `path` sees them. `strict` keeps to what the file
        itself declares and imports, without the crate-wide search."""
        if (path, segments) in trail or len(trail) > 16:
            raise _Unresolved(f"{'::'.join(segments)} is imported in a circle")
        trail = (*trail, (path, segments))
        index = self._file(path)
        crate = self.crate_of(path)
        first = segments[0]
        if first in {"crate", "self", "super"}:
            base: tuple[str, ...] = ()
            if first != "crate":
                base = (*self._module_path(Shape(path, "", 0, "")), *modules)
            rest = segments[1:] if first in {"crate", "self"} else segments
            while rest[:1] == ("super",):
                base, rest = base[:-1], rest[1:]
            return self._in_module(crate, base, rest, opaque, trail, "::".join(segments))

        imported = [use for name, use, _ in index.uses if name == first and use != (first,)]
        if imported:
            found: dict[Shape, None] = {}
            failures: list[str] = []
            standard = False
            for use in dict.fromkeys(imported):
                try:
                    shapes = self._resolve_path(
                        (*use, *segments[1:]), path, modules, opaque, trail
                    )
                except _Unresolved as error:
                    failures.append(str(error))
                else:
                    standard = standard or shapes is _STANDARD
                    found.update(dict.fromkeys(shapes))
            if failures and not found and not standard:
                raise _Unresolved(failures[0])
            return list(found) if found or not standard else _STANDARD

        if len(segments) == 1:
            local = [
                Shape(path, name, offset, kind, chain)
                for name, kind, offset, chain in index.types
                if name == first
            ]
            local = self._default_build(local)
            if local:
                for wanted in (
                    lambda shape: shape.modules == modules,
                    lambda shape: shape.modules == modules[: len(shape.modules)],
                    lambda shape: True,
                ):
                    nearest = [shape for shape in local if wanted(shape)]
                    if nearest:
                        return nearest
            if first in RUST_PRELUDE_TYPES:
                return _STANDARD
            globbed: dict[Shape, None] = {}
            beyond: list[tuple[str, str]] = []
            for name, use, _ in index.uses:
                if name != "*":
                    continue
                try:
                    hidden: list[tuple[str, str]] = []
                    shapes = self._resolve_path((*use, first), path, modules, hidden, trail)
                except _Unresolved:
                    continue
                if shapes is _STANDARD:
                    return _STANDARD
                globbed.update(dict.fromkeys(shapes))
                beyond.extend(hidden)
            if globbed:
                return list(globbed)
            if beyond:
                opaque.extend(beyond)
                return []
            if strict:
                raise _Unresolved(f"{first} is not declared or imported in {path}")
            try:
                return self._in_crate(crate, segments, opaque, trail, first)
            except _Unresolved:
                declared = self._macro_declared(crate, first)
                if declared:
                    return declared
                raise

        if first in RUST_STANDARD_CRATES:
            return _STANDARD
        if first in self._crate_names or first in self._external.get(crate, ()):
            return self._in_crate_named(segments, Shape(path, "", 0, "", modules), opaque, trail)
        # A module of this crate, written relative to the file.
        base = (*self._module_path(Shape(path, "", 0, "")), *modules)
        return self._in_module(crate, base, segments, opaque, trail, "::".join(segments))

    def _in_crate_named(
        self,
        segments: tuple[str, ...],
        origin: Shape,
        opaque: list[tuple[str, str]],
        trail: tuple[tuple[str, tuple[str, ...]], ...],
    ) -> list[Shape]:
        self._load_workspace()
        first = segments[0]
        if first in RUST_STANDARD_CRATES:
            return _STANDARD
        if first not in self._crate_names:
            opaque.append(("::".join(segments), f"a type of package `{first}`"))
            return []
        if len(segments) == 1:
            return []
        return self._in_module(
            self._crate_names[first], (), segments[1:], opaque, trail, "::".join(segments)
        )

    def _module_file(self, crate: str, module: tuple[str, ...]) -> str | None:
        indexed = self._index(crate)
        if crate not in self._module_files:
            self._module_files[crate] = {
                self._module_path(Shape(path, "", 0, "")): path
                for path in sorted(indexed, reverse=True)
            }
        return self._module_files[crate].get(module)

    def _in_module(
        self,
        crate: str,
        base: tuple[str, ...],
        rest: tuple[str, ...],
        opaque: list[tuple[str, str]],
        trail: tuple[tuple[str, tuple[str, ...]], ...],
        written: str,
    ) -> list[Shape]:
        """`rest` as module `base` of `crate` sees it: what the crate declares
        or re-exports there, then what that module itself imports (a child's
        `use super::*` brings its parent's imports with it)."""
        if not rest:
            return []
        named = self._module_file(crate, (*base, *rest[:-1]))
        if named is not None:
            try:
                return self._resolve_path(rest[-1:], named, (), opaque, trail, strict=True)
            except _Unresolved:
                pass
        try:
            return self._in_crate(crate, (*base, *rest), opaque, trail, written)
        except _Unresolved as error:
            failure = error
        for depth in range(len(rest) - 1, -1, -1):
            module = (*base, *rest[:depth])
            source = self._module_file(crate, module)
            while source is None and depth == 0 and module:
                module = module[:-1]
                source = self._module_file(crate, module)
            if source is not None:
                try:
                    return self._resolve_path(rest[depth:], source, (), opaque, trail)
                except _Unresolved:
                    break
        declared = self._macro_declared(crate, rest[-1])
        if declared:
            return declared
        raise failure

    def _in_crate(
        self,
        crate: str,
        segments: tuple[str, ...],
        opaque: list[tuple[str, str]],
        trail: tuple[tuple[str, tuple[str, ...]], ...],
        written: str,
    ) -> list[Shape]:
        """`segments` looked up in `crate`: its own definitions, then what it
        re-exports."""
        if not segments:
            return []
        key = (crate, segments)
        if key not in self._lookups:
            # A re-export that leads back here names nothing new.
            self._lookups[key] = (None, (), f"{written} is re-exported in a circle")
            hidden: list[tuple[str, str]] = []
            try:
                shapes = self._search_crate(crate, segments, hidden, trail, written)
            except _Unresolved as error:
                self._lookups[key] = (None, (), str(error))
            else:
                self._lookups[key] = (shapes, tuple(hidden), "")
        shapes, hidden, failure = self._lookups[key]
        if shapes is None:
            raise _Unresolved(failure)
        opaque.extend(hidden)
        return shapes if shapes is _STANDARD else list(shapes)

    def _search_crate(
        self,
        crate: str,
        segments: tuple[str, ...],
        opaque: list[tuple[str, str]],
        trail: tuple[tuple[str, tuple[str, ...]], ...],
        written: str,
    ) -> list[Shape]:
        indexed = self._index(crate)
        name, hints = segments[-1], segments[:-1]
        candidates = self._default_build(self._types[crate].get(name, ()))
        if candidates:
            if hints:
                hinted = [
                    shape
                    for shape in candidates
                    if self._module_path(shape)[-len(hints) :] == hints
                ]
                exact = [shape for shape in hinted if self._module_path(shape) == hints]
                candidates = exact or hinted or candidates
            groups = {(shape.path, shape.modules) for shape in candidates}
            if len(groups) > 1:
                raise _Unresolved(
                    f"{written} is ambiguous: "
                    + ", ".join(sorted(f"{path}" for path, _ in groups))
                )
            return candidates

        # Not declared in the crate: a re-export, or a macro's declaration.
        found: dict[Shape, None] = {}
        hidden: list[tuple[str, str]] = []
        if crate not in self._exports:
            exports: dict[str, list[tuple[str, tuple[str, ...]]]] = {}
            for path, index in indexed.items():
                for bound, use, public in index.uses:
                    if public:
                        exports.setdefault(bound, []).append((path, use))
            self._exports[crate] = exports
        exports = self._exports[crate]
        for bound in (name, "*"):
            for path, use in exports.get(bound, ()):
                target = (*use, name) if bound == "*" else use
                if target[0] not in {"crate", "self", "super"} and (
                    target[0] not in self._crate_names
                    and target[0] not in self._external.get(crate, ())
                    and target[0] not in RUST_STANDARD_CRATES
                ):
                    # `pub use module::Name` beside `mod module;`.
                    target = ("self", *target)
                try:
                    shapes = self._resolve_path(target, path, (), hidden, trail)
                except _Unresolved:
                    continue
                if shapes is _STANDARD:
                    return _STANDARD
                found.update(dict.fromkeys(shapes))
        if found:
            return list(found)
        exported = [entry for entry in hidden if entry[0].rpartition("::")[2] == name]
        if exported:
            opaque.append(exported[0])
            return []
        raise _Unresolved(f"{written} names no type of {crate}")

    def _macro_texts(self, crate: str) -> list[tuple[str, str, int, bool, str]]:
        """`(path, macro, offset, is the definition, text)` for the crate's
        `macro_rules!` definitions and its calls of them."""
        if crate not in self._macros:
            indexed = self._index(crate)
            macros = {macro for index in indexed.values() for macro, _ in index.macro_definitions}
            texts = []
            for path, index in indexed.items():
                content = self.view.projected(path) or ""
                for defines, entries in (
                    (True, index.macro_definitions),
                    (False, index.macro_calls),
                ):
                    for macro, offset in entries:
                        if macro in macros:
                            body = _item_text(content, offset)
                            texts.append((path, macro, offset, defines, body))
            self._macros[crate] = texts
        return self._macros[crate]

    def _macro_declared(self, crate: str, name: str) -> list[Shape]:
        """The crate's `macro_rules!` definitions that declare `name`, and
        otherwise its calls of its own macros that mention it."""
        if crate not in self._macro_names:
            declared: dict[str, list[Shape]] = {}
            mentioned: dict[str, list[Shape]] = {}
            for path, macro, offset, defines, body in self._macro_texts(crate):
                if defines:
                    names = set(re.findall(r"\b(?:struct|enum)\s+([A-Za-z_][A-Za-z0-9_]*)", body))
                    table = declared
                else:
                    names = {value for kind, value in rust_tokens(body) if kind == "ident"}
                    table = mentioned
                for found in names:
                    table.setdefault(found, []).append(Shape(path, found, offset, f"macro {macro}"))
            self._macro_names[crate] = (declared, mentioned)
        declared, mentioned = self._macro_names[crate]
        return declared.get(name) or mentioned.get(name) or []

    # -- adapters ------------------------------------------------------------

    def _adapter(
        self, kind: str, written: str, origin: Shape, opaque: list[tuple[str, str]]
    ) -> list[Entry]:
        """The text of a `with` module or a `*_with` function."""
        segments = tuple(part.strip() for part in written.split("::") if part.strip())
        if not segments:
            raise _Unresolved(f"{kind} = {written!r} names nothing")
        if (
            kind == "deserialize_with"
            and segments == ("Option", "deserialize")
            and self._resolve_path(("Option",), origin.path, origin.modules, [], ()) is _STANDARD
        ):
            # Serde's standard Option implementation uses the field's normal
            # representation. The attribute still changes missing-field
            # admission, so it and the field's payload remain guarded.
            opaque.append((written, "a deserialize_with adapter of the standard library"))
            return []
        function = None if kind == "with" else segments[-1]
        module = segments if kind == "with" else segments[:-1]
        if function is not None and not module:
            content = self.view.projected(origin.path) or ""
            local = named_rust_items(content, [function])
            if local:
                return [(origin.path, name, value) for name, value in local.items()]
            imported = [use for name, use, _ in self._file(origin.path).uses if name == function]
            if not imported:
                raise _Unresolved(f"{kind} = {written!r} names no function of {origin.path}")
            module, function = imported[0][:-1], imported[0][-1]
        hidden: list[tuple[str, str]] = []
        texts = self._module_texts(module, origin.path, hidden, 0)
        if hidden:
            opaque.append((written, f"a {kind} adapter of {hidden[0][1]}"))
            return []
        found: list[Entry] = []
        for path, label, text in texts:
            if function is None:
                found.append((path, label, strip_rust_trivia(text)))
            else:
                found.extend(
                    (path, name, value)
                    for name, value in named_rust_items(text, [function]).items()
                )
        if not found and kind == "with":
            # A `with` path may also name a type that carries a `remote` derive.
            try:
                found = [
                    self.entry(shape)
                    for shape in self._resolve_path(segments, origin.path, origin.modules, [], ())
                ]
            except _Unresolved:
                found = []
        if not found:
            raise _Unresolved(f"{kind} = {written!r} names no adapter this tree declares")
        return found

    def _module_texts(
        self, module: tuple[str, ...], path: str, hidden: list[tuple[str, str]], depth: int
    ) -> list[tuple[str, str, str]]:
        """`(path, label, text)` of the module `module` names where `path`
        writes it: an inline `mod`, or a module's file."""
        if depth > 8 or not module:
            return []
        crate = self.crate_of(path)
        index = self._file(path)
        first = module[0]
        if first in RUST_STANDARD_CRATES:
            hidden.append((first, "the standard library"))
            return []
        if first in {"crate", "self", "super"}:
            module = tuple(part for part in module if part not in {"crate", "self", "super"})
            return self._module_in_crate(crate, module, path, hidden, depth)
        imported = [use for name, use, _ in index.uses if name == first and use != (first,)]
        if imported:
            return self._module_texts((*imported[0], *module[1:]), path, hidden, depth + 1)
        found = self._module_in_crate(crate, module, path, hidden, depth)
        if found or hidden:
            return found
        if first in self._crate_names and module[1:]:
            target = self._crate_names[first]
            return self._module_in_crate(target, module[1:], "", hidden, depth)
        if first in self._external.get(crate, ()):
            hidden.append((first, f"package `{first}`"))
        return []

    def _module_in_crate(
        self,
        crate: str,
        module: tuple[str, ...],
        origin: str,
        hidden: list[tuple[str, str]],
        depth: int,
    ) -> list[tuple[str, str, str]]:
        indexed = self._index(crate)
        if origin and origin not in indexed and self.crate_of(origin) == crate:
            indexed = {origin: self._file(origin), **indexed}
        found: list[tuple[str, str, str]] = []
        label = f"mod {module[-1]}"
        for path in sorted(indexed, key=lambda candidate: candidate != origin):
            index = indexed[path]
            content = self.view.projected(path)
            if content is None:
                continue
            file_modules = self._module_path(Shape(path, "", 0, ""))
            for name, start, end, chain in index.modules:
                full = (*file_modules, *chain, name)
                if name == module[-1] and (
                    full[-len(module) :] == module or (path == origin and len(module) == 1)
                ):
                    found.append((path, label, content[start:end]))
            for name, relocated in index.child_modules:
                if name == module[-1] and relocated and (path == origin or len(module) == 1):
                    target = "/".join((*path.split("/")[:-1], relocated))
                    text = self.view.projected(target)
                    if text is not None:
                        found.append((target, label, text))
            if file_modules and file_modules[-len(module) :] == module:
                found.append((path, label, content))
            if found and path == origin:
                return found
        if found:
            return found
        # A module the crate root re-exports from elsewhere.
        root = self._module_file(crate, ())
        if root is not None and root != origin:
            for name, use, _ in indexed[root].uses:
                if name == module[0] and use != (module[0],):
                    return self._module_texts((*use, *module[1:]), root, hidden, depth + 1)
        return []

    # -- the walk ------------------------------------------------------------

    def entry(self, shape: Shape) -> Entry:
        content = self.view.projected(shape.path) or ""
        if shape.kind.startswith("macro "):
            return (shape.path, shape.name, strip_rust_trivia(_item_text(content, shape.offset)))
        parsed = parse_shape(content, shape.offset)
        return (
            shape.path,
            shape.name,
            strip_rust_trivia(normalize_rust_derive_lists(content[parsed.start : parsed.end])),
        )

    def _expand(self, shape: Shape):
        """What `shape` leads to: the shapes it serializes, the text guarded
        beside it, what is opaque under it, and what could not be resolved."""
        key = shape
        if key in self._edges:
            return self._edges[key]
        targets: dict[Shape, None] = {}
        extra: list[Entry] = []
        opaque: list[tuple[str, str]] = []
        problems: list[str] = []
        crate = self.crate_of(shape.path)
        indexed = self._index(crate)
        content = self.view.projected(shape.path) or ""
        hand_written = False
        for path, index in indexed.items():
            if shape.name not in index.serde_impls:
                continue
            text = self.view.projected(path) or ""
            impls = named_rust_serde_impls(
                text, (f"Serialize for {shape.name}", f"Deserialize for {shape.name}")
            )
            extra.extend((path, name, value) for name, value in impls.items())
            hand_written = hand_written or bool(impls)
        if shape.kind.startswith("macro "):
            macro = shape.kind.partition(" ")[2]
            extra.extend(
                (path, f"{macro}!", strip_rust_trivia(body))
                for path, name, _, _, body in self._macro_texts(crate)
                if name == macro
            )
            opaque.append(
                (shape.label, f"declared by `{macro}!`; the macro and its calls are guarded")
            )
            result = ((), tuple(extra), tuple(opaque), ())
            self._edges[key] = result
            return result

        parsed = parse_shape(content, shape.offset)
        if hand_written and not parsed.derives_serde:
            opaque.append(
                (shape.label, "its Serde impls are hand-written; the impls are guarded")
            )
        elif not parsed.derives_serde and shape.kind != "type":
            opaque.append((shape.label, "not a Serde type; its fields are not followed"))
        else:
            for ref in parsed.refs:
                try:
                    for target in self.resolve(ref, shape, opaque):
                        targets[target] = None
                except _Unresolved as error:
                    problems.append(f"{shape.label} names {ref.label}: {error}")
            for kind, written in parsed.adapters:
                key = (kind, written, shape.path)
                if key not in self._adapters:
                    hidden: list[tuple[str, str]] = []
                    try:
                        found = self._adapter(kind, written, shape, hidden)
                        self._adapters[key] = (found, hidden, "")
                    except _Unresolved as error:
                        self._adapters[key] = ([], [], str(error))
                entries, hidden, failure = self._adapters[key]
                extra.extend(entries)
                opaque.extend(hidden)
                if failure:
                    problems.append(f"{shape.label}: {failure}")
        result = (tuple(targets), tuple(extra), tuple(dict.fromkeys(opaque)), tuple(problems))
        self._edges[key] = result
        return result

    def closure(self, roots: Iterable[Shape]) -> Closure:
        depths: dict[Shape, int] = {}
        queue: list[Shape] = []
        for root in roots:
            if root not in depths:
                depths[root] = 0
                queue.append(root)
        entries: dict[Entry, None] = {}
        opaque: dict[tuple[str, str], None] = {}
        problems: dict[str, None] = {}
        graph: dict[Shape, tuple[Shape, ...]] = {}
        cursor = 0
        while cursor < len(queue):
            shape = queue[cursor]
            cursor += 1
            targets, extra, hidden, failed = self._expand(shape)
            graph[shape] = targets
            entries[self.entry(shape)] = None
            entries.update(dict.fromkeys(extra))
            opaque.update(dict.fromkeys(hidden))
            problems.update(dict.fromkeys(failed))
            for target in targets:
                if target not in depths:
                    depths[target] = depths[shape] + 1
                    queue.append(target)
        return Closure(
            tuple(sorted(entries)),
            tuple(depths.items()),
            _cycles(graph),
            tuple(sorted(opaque)),
            tuple(problems),
        )


@lru_cache(maxsize=None)
def _item_text(text: str, offset: int) -> str:
    return text[offset : rust_item_end(text, offset)]


def _cycles(graph: dict[Shape, tuple[Shape, ...]]) -> tuple[tuple[str, ...], ...]:
    """The strongly connected components of the closure that hold a cycle."""
    order: dict[Shape, int] = {}
    low: dict[Shape, int] = {}
    stack: list[Shape] = []
    on_stack: set[Shape] = set()
    found: list[tuple[str, ...]] = []
    for start in graph:
        if start in order:
            continue
        work: list[tuple[Shape, int]] = [(start, 0)]
        while work:
            node, edge = work.pop()
            if edge == 0:
                order[node] = low[node] = len(order)
                stack.append(node)
                on_stack.add(node)
            targets = graph.get(node, ())
            if edge < len(targets):
                work.append((node, edge + 1))
                target = targets[edge]
                if target not in order:
                    work.append((target, 0))
                elif target in on_stack:
                    low[node] = min(low[node], order[target])
                continue
            for target in targets:
                if target in on_stack:
                    low[node] = min(low[node], low[target])
            if low[node] == order[node]:
                component: list[Shape] = []
                while True:
                    member = stack.pop()
                    on_stack.discard(member)
                    component.append(member)
                    if member == node:
                        break
                if len(component) > 1 or node in graph.get(node, ()):
                    found.append(tuple(sorted(shape.name for shape in component)))
    return tuple(sorted(found))


def reachability(view: TreeView) -> Reachability:
    if view._reachability is None:
        view._reachability = Reachability(view)
    return view._reachability


def roots_closure(view: TreeView, guard: Guard) -> tuple[Closure, list[str]]:
    """The closure of a `roots(..)` guard's roots, or of every shape a
    `shapes(..)` guard sweeps, and the roots the tree does not have."""
    if guard.kind == "records":
        from durable_surfaces import closure
        return closure(view, guard)
    reach = reachability(view)
    shapes: list[Shape] = []
    found: set[str] = set()
    for path in view.matching_paths(guard.paths):
        names: Iterable[str] = guard.symbols
        if guard.kind == "shapes":
            names = [name.partition("#")[0] for name in serde_shapes(view.projected(path) or "")]
        for name in dict.fromkeys(names):
            roots = reach.roots(path, name)
            if roots:
                found.add(name)
            shapes.extend(roots)
    return reach.closure(shapes), sorted(set(guard.symbols) - found)


def guard_signature(
    view: TreeView,
    guard: Guard,
    *,
    enforce_presence: bool,
    base_signature: tuple[Entry, ...] | None = None,
) -> tuple[Entry, ...]:
    if guard.kind == "step":
        from durable_surfaces import step_signature
        return step_signature(view, guard)
    if guard.kind in {"roots", "records"}:
        return roots_signature(view, guard, enforce_presence=enforce_presence)
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

    if guard.kind == "shapes":
        # A sweep's shapes are roots too: what they serialize is guarded.
        signature.extend(roots_signature(view, guard, enforce_presence=enforce_presence))
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
    return tuple(sorted(set(signature)))


def closure_of(view: TreeView, guard: Guard) -> tuple[Closure, list[str]]:
    if guard not in view._closures:
        view._closures[guard] = roots_closure(view, guard)
    return view._closures[guard]


def roots_signature(
    view: TreeView, guard: Guard, *, enforce_presence: bool
) -> tuple[Entry, ...]:
    """Every shape reachable from the guard's roots. At the head a root the
    tree does not have, and a reachable type it cannot resolve, are errors."""
    closure, missing = closure_of(view, guard)
    if enforce_presence:
        where = f"{view.label}: {guard.label}"
        if missing:
            raise CheckError(f"{where} does not find " + ", ".join(sorted(missing)))
        if closure.problems:
            shown = closure.problems[:MAX_REPORTED_PROBLEMS]
            more = len(closure.problems) - len(shown)
            raise CheckError(
                f"{where} reaches {len(closure.problems)} type(s) it cannot resolve: "
                + "; ".join(shown)
                + (f"; and {more} more" if more else "")
            )
    return closure.entries


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
    (`"lashlang-vm-abi-v14"`), or a typed plugin `FormatVersion`.
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
        typed = re.fullmatch(
            r"(?:[A-Za-z_][A-Za-z0-9_]*::)*FormatVersion::"
            r"(?:ONE|new\(([0-9][0-9_]*)\)\.unwrap\(\))",
            strip_rust_trivia(value),
        )
        if typed is not None:
            version = 1 if typed[1] is None else int(typed[1].replace("_", ""))
            if 0 < version <= 0xFFFF_FFFF:
                return version
        tagged = re.fullmatch(r'"[^"\\]*?v([0-9]+)(?:[/:-][^"\\]*)?"', value)
        if tagged is not None:
            return int(tagged.group(1))
        if re.fullmatch(r"[A-Z][A-Z0-9_]*", value) is None:
            break
        name = value
    raise CheckError(
        f"{where}: {surface.constant} does not resolve to an integer version"
    )


def surface_of(raw: dict, location: str) -> Surface:
    """One discovered source declaration."""
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


CATALOG_TABLE = (
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:const|static)[ \t]+{name}\b"
)
RUST_STRING = re.compile(r'"(?:[^"\\]|\\.)*"')
CATALOG_STEP = re.compile(
    r"\bfrom(?:_version)?:([0-9_]+),.*?\bto(?:_version)?:([0-9_]+)[,}]"
)


def catalog_steps(view: TreeView, catalog: Catalog) -> set[tuple[int, int]]:
    """`(from, to)` for every row of the default build's migration catalog.

    A row is a struct literal with `from` and `to` (or `from_version` and
    `to_version`) integer fields, in that order. Rows under the
    `synthetic-next` cfg are the upgrade harness's and are left out, and
    `rows` keeps only the rows that contain its text.
    """
    where = f"{view.label}: {catalog.label}"
    content = view.content(catalog.path)
    if content is None:
        raise CheckError(f"{where} cannot be read")
    attribute_ranges = rust_outer_attribute_ranges(content)
    excluded = test_only_module_ranges(content, attribute_ranges)
    tables: list[str] = []
    for match in re.finditer(
        CATALOG_TABLE.format(name=re.escape(catalog.table)), content
    ):
        if any(start <= match.start() < end for start, end in excluded):
            continue
        start = rust_item_start_with_attributes(content, match.start(), attribute_ranges)
        if SYNTHETIC_NEXT_CFG in strip_rust_trivia(content[start : match.start()]):
            continue
        item = content[match.start() : rust_item_end(content, match.start())]
        tables.append(strip_rust_trivia(item))
    if len(tables) != 1:
        raise CheckError(
            f"{where}: expected one default-build definition, found {len(tables)}"
        )
    table = tables[0]
    opening = table.find("=&[")
    if opening < 0 or not table.endswith("];"):
        raise CheckError(
            f"{where} must be an inline array of migration rows so its steps "
            "can be read"
        )
    elements = _rust_top_level_items(table, opening + 3, len(table) - 2)
    if elements is None:
        raise CheckError(f"{where} has unbalanced rows")
    selector = None if catalog.rows is None else strip_rust_trivia(catalog.rows)
    steps: set[tuple[int, int]] = set()
    for start, end in elements:
        row = table[start:end]
        if not row or row.startswith(SYNTHETIC_NEXT_CFG):
            continue
        # A step is read from the row's own fields, never from text inside
        # one of its string literals.
        step = CATALOG_STEP.search(RUST_STRING.sub('""', row))
        if row.startswith("#[") or step is None:
            raise CheckError(
                f"{where}: keep each row as {{ from: N, to: N, .. }} (or "
                "from_version and to_version), under no cfg but synthetic-next"
            )
        if selector is None or selector in row:
            steps.add(
                (int(step.group(1).replace("_", "")), int(step.group(2).replace("_", "")))
            )
    return steps


def require_catalog(surface: Surface, declaration: Declaration) -> None:
    """A surface that guards SQL DDL is a DDL stamp, and names its catalog.

    The guard itself says so: a whole-file guard over a `.sql` file, or a
    guard that elides idempotent index statements. Without the catalog a
    stamp's bump would be its own evidence.
    """
    guards_ddl = any(
        guard.elide == "sql_idempotent_index"
        or (guard.kind == "file" and any(path.endswith(".sql") for path in guard.paths))
        for guard in declaration.guards
    )
    if guards_ddl and not declaration.catalogs:
        raise CheckError(
            f"{surface.constant} guards SQL DDL and declares no migration "
            f"catalog: add catalog(path = \"<file>\", <TABLE>) to its {MARKER} marker"
        )


def missing_catalog_step(
    steps: set[tuple[int, int]], base_version: int, head_version: int
) -> int | None:
    """The first version the catalog has no step forward from, walking from
    the base's version to the head's as a migrator would."""
    at = base_version
    while at < head_version:
        reached = [to for start, to in steps if start == at and at < to <= head_version]
        if not reached:
            return at
        at = max(reached)
    return None


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
    in_place: tuple[Finding, ...] = ()


def _declaration(view: TreeView, surface: Surface) -> Declaration | None:
    content = view.content(surface.constant_path)
    if content is None:
        raise CheckError(
            f"{view.label}: cannot read {surface.constant_path} for {surface.constant}"
        )
    from durable_surfaces import guards
    declared = declaration_in(content, surface)
    owned = guards(view, surface.constant)
    if not owned:
        return declared
    return Declaration((declared.guards if declared else ()) + owned,
                       None, declared.catalogs if declared else ())


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


def check_surfaces(
    repo: Path, base: str, head: str, *, strict: bool = STRICT_VERSION_BUMPS
) -> CheckResult:
    return check_views(
        RevisionView(repo, resolve_revision(repo, base)),
        RevisionView(repo, resolve_revision(repo, head)),
        strict=strict,
    )


def check_views(
    base_view: TreeView,
    head_view: TreeView,
    only: frozenset[str] | None = None,
    *,
    strict: bool = STRICT_VERSION_BUMPS,
) -> CheckResult:
    """Compare two trees. `only` names the surface keys to evaluate; the
    command evaluates every surface."""
    head_registry = head_view.content(REGISTRY)
    if head_registry is None:
        raise CheckError(f"{head_view.label}: cannot read {REGISTRY}")
    from discover_version_surfaces import surfaces as discover_surfaces
    surfaces = discover_surfaces(head_view)
    base_registry = base_view.content(REGISTRY)
    base_surfaces = (
        {}
        if base_registry is None
        else {
            surface.key: surface
            for surface in discover_surfaces(base_view, enforce=False)
        }
    )
    head_keys = {surface.key for surface in surfaces}

    evaluated: list[Surface] = []
    unshaped: list[Surface] = []
    failures: list[Finding] = []
    errors: list[Finding] = []
    bumped: list[Finding] = []
    registered: list[Surface] = []
    in_place: list[Finding] = []
    continued: set[str] = set()
    lifts: set[tuple[str, int]] | None = None

    for surface in surfaces:
        if only is not None and surface.key not in only:
            continue
        try:
            declaration = _declaration(head_view, surface)
            if declaration is None:
                raise CheckError(
                    f"{surface.constant} declares no {MARKER} marker: name the "
                    f"shapes it guards on the constant, or state "
                    f'{MARKER}(unshaped = "<reason>")'
                )
            head_version = version_at(head_view, surface)
            require_catalog(surface, declaration)
            # A catalog that cannot be read is an error whether or not this
            # change bumps: the evidence it would owe could not be checked.
            catalogs = {
                catalog: catalog_steps(head_view, catalog)
                for catalog in declaration.catalogs
            }
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
                    (failures if strict else in_place).append(
                        Finding(
                            surface,
                            f"{surface.constant} is {head_version} on both sides but "
                            f"its guarded shape changed ({', '.join(changed)}; head "
                            f"{surface_fingerprint(head_entries)})."
                            + (
                                f" Bump {surface.constant} strictly past {base_version}."
                                if strict
                                else " Pre-1.0 version freeze: changed in place."
                            ),
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
                    # The base's catalogs bind too: dropping the declaration
                    # in the commit that bumps does not excuse the step.
                    if base_declaration is not None:
                        for catalog in base_declaration.catalogs:
                            if catalog not in catalogs:
                                catalogs[catalog] = catalog_steps(head_view, catalog)
                    unstepped = [
                        (catalog, version)
                        for catalog, steps in catalogs.items()
                        if (
                            version := missing_catalog_step(
                                steps, base_version, head_version
                            )
                        )
                        is not None
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
                    elif unstepped:
                        failures.append(
                            Finding(
                                surface,
                                f"{surface.constant} moved {base_version} to "
                                f"{head_version} without its upgrade evidence: "
                                + "; ".join(
                                    f"add a step to {catalog.label} from version "
                                    f"{version}"
                                    for catalog, version in unstepped
                                ),
                            )
                        )
                    else:
                        evidence = (
                            "lift rows registered"
                            if surface.decoder_laws
                            else "catalog steps registered"
                            if catalogs
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
        if key in head_keys or key in continued or only is not None:
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
        tuple(in_place),
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
            require_catalog(surface, declaration)
            for guard in declaration.guards:
                guard_signature(view, guard, enforce_presence=True)
            for catalog in declaration.catalogs:
                catalog_steps(view, catalog)
        except CheckError as error:
            problems.append(f"{surface.key} has a guard that cannot be evaluated: {error}")
    return problems


def guarded_path_patterns(repo: Path) -> frozenset[str]:
    """Journal owners and every path a registered surface can touch, as the working
    tree declares it: the registry, each constant's file, each guard's paths
    (globs included) and each catalog's file. CI selects the gate, the
    rolling-upgrade gate and the release-journal replay with it."""
    view = WorktreeView(repo)
    registry = view.content(REGISTRY)
    if registry is None:
        raise CheckError(f"cannot read {REGISTRY}")
    patterns = {REGISTRY, UPCASTER_REGISTRY, *JOURNAL_LOGIC_PATHS}
    from discover_version_surfaces import surfaces as discover_surfaces
    for surface in discover_surfaces(view):
        patterns.add(surface.constant_path)
        declaration = _declaration(view, surface)
        if declaration is not None:
            for guard in declaration.guards:
                patterns.update(guard.paths)
                if guard.kind in {"roots", "shapes", "records"}:
                    patterns.update(closure_of(view, guard)[0].paths)
            patterns.update(catalog.path for catalog in declaration.catalogs)
    return frozenset(patterns)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--base", required=True, help="baseline commit")
    parser.add_argument("--head", required=True, help="candidate commit")
    parser.add_argument(
        "--strict", action="store_true", default=STRICT_VERSION_BUMPS,
        help="require version bumps for shape changes (mandatory after the 1.0 cut)",
    )
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


JOURNAL_LOGIC_SOURCE = "crates/lash-restate/src/process/admission.rs"
JOURNAL_LOGIC_PATHS = (
    "crates/lash-restate/src/*.rs",
    "crates/lash-core/src/runtime/*.rs",
    "crates/lash-core-execution/src/runtime/*.rs",
    "crates/lash-core-store/src/tool_run/*.rs",
    "crates/lash-core-execution/src/tool_dispatch.rs",
    "crates/lash-core-execution/src/tool_dispatch/*.rs",
    "crates/lash-core-execution/src/plugin/transition.rs",
    "crates/lash-core-execution/src/plugin/state/publication.rs",
)


def journal_logic_source(text: str) -> str:
    """The ordering of suspension commands and their control-flow decisions.

    Shapes, constants, signatures, and work inside synchronous codecs belong
    to surface guards. This tripwire records async function boundaries,
    awaited callees, journal commands, and branch/loop decisions only.
    """
    ranges = test_only_module_ranges(text, rust_outer_attribute_ranges(text))
    for start, end in reversed(ranges):
        text = text[:start] + text[end:]
    projection = []
    commands = {"run", "run_json_send", "run_json_or_retry_send",
                "run_json_eager_or_retry_send", "run_json_schedule_or_retry_send",
                "call", "send", "sleep", "select", "join", "try_join"}
    functions = []
    for match in re.finditer(r"\bfn\s+(\w+)\s*(?:<[^{};]*>)?\s*\(", text):
        try:
            end = rust_item_end(text, match.start())
        except CheckError:
            continue
        item = text[match.end():end]
        body = item[item.find("{"):]
        if ".await" in body or any(re.search(r"\.\s*" + command + r"\b", body) for command in commands):
            functions.append((match[1], body))
    for name, body in functions:
        projection.append(("function", name))
        tokens = rust_tokens(body)
        projection.extend(_journal_control_tokens(tokens, commands))
    return repr(projection)


def _journal_control_tokens(tokens, commands):
    projection = []
    stack = []
    closes = {}
    for index, (_, token) in enumerate(tokens):
        if token == "{":
            stack.append(index)
        elif token == "}" and stack:
            closes[stack.pop()] = index
    boundaries = {}
    for index, (_, token) in enumerate(tokens):
        if token in {"if", "match", "while", "for", "else", "loop"}:
            start = index + 1
            while start < len(tokens) and tokens[start][1] != "{":
                start += 1
            if start in closes:
                boundaries.setdefault(closes[start], []).append(token)
    for index, (_, token) in enumerate(tokens):
        projection.extend(("end", branch) for branch in boundaries.get(index, ()))
        if token in commands and index and tokens[index - 1][1] in {".", "::"}:
            projection.append(("command", token))
        elif token == "await":
            cursor = index - 2  # skip the dot
            if cursor >= 0 and tokens[cursor][1] == ")":
                depth = 1
                cursor -= 1
                while cursor >= 0 and depth:
                    if tokens[cursor][1] == ")": depth += 1
                    if tokens[cursor][1] == "(": depth -= 1
                    cursor -= 1
            projection.append(("await", tokens[cursor][1] if cursor >= 0 else ""))
        elif token in {"if", "match", "while", "for"}:
            end = index + 1
            while end < len(tokens) and tokens[end][1] != "{":
                end += 1
            projection.append((token, tuple(value for _, value in tokens[index + 1:end])))
        elif token == "=>":
            cursor = index - 1
            depth = 0
            while cursor >= 0:
                value = tokens[cursor][1]
                if value in {")", "}", "]"}: depth += 1
                elif value in {"(", "{", "["}:
                    if depth == 0: break
                    depth -= 1
                elif value == "," and depth == 0: break
                cursor -= 1
            projection.append(("arm", tuple(value for _, value in tokens[cursor + 1:index])))
        elif token == "?":
            projection.append(("flow", "try"))
        elif token in {"else", "loop", "break", "continue", "return", "select"}:
            projection.append(("flow", token))
    return projection


def journal_lane_refusal(base: TreeView, head: TreeView) -> str | None:
    """Handler logic needs both generation lanes, even during format freeze.

    This compares command ordering and branch/loop decisions in the journal
    owners; serialized shapes and step kinds have their own surface guards. Source
    text is not a substitute for replay proof of the resulting generation.
    """
    old = base.content(JOURNAL_LOGIC_SOURCE)
    if old is None:
        return None
    paths = sorted(set(base.matching_paths(JOURNAL_LOGIC_PATHS)) |
                   set(head.matching_paths(JOURNAL_LOGIC_PATHS)))
    paths = [path for path in paths if not any(
        part == "tests" or part == "testing" or part.endswith("_tests.rs") or
        part in ("tests.rs", "testing.rs") for part in Path(path).parts)]
    base.preload(paths)
    head.preload(paths)
    changed = [path for path in paths if journal_logic_source(base.content(path) or "") !=
               journal_logic_source(head.content(path) or "")]
    if not changed:
        return None
    pattern = r"pub const JOURNAL_LOGIC_EPOCH: u32 = (\d+);"
    before = tuple(map(int, re.findall(pattern, old)))
    after = tuple(map(int, re.findall(pattern, head.content(JOURNAL_LOGIC_SOURCE) or "")))
    if (len(before) == len(after) == 2 and after[0] > before[0] and
            after[1] > before[1] and after[1] == after[0] + 1):
        return None
    return ("journal logic changed: move JOURNAL_LOGIC_EPOCH and its synthetic-next "
            f"counterpart together; base {before}, head {after}; changed " + ", ".join(changed))


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        base = resolve_revision(args.repo, args.base)
        head = resolve_revision(args.repo, args.head)
        lane_refusal = journal_lane_refusal(RevisionView(args.repo, base), RevisionView(args.repo, head))
        if lane_refusal:
            print(lane_refusal, file=sys.stderr)
            return 1
        result = check_surfaces(args.repo, base, head, strict=args.strict)
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
    for finding in result.in_place:
        print(f"in-place shape change: {finding.surface.key}: {finding.detail}")
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
