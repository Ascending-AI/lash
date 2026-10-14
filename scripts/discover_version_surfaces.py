#!/usr/bin/env python3
"""Shared offset-preserving Rust inspection and source-declared inventory.

This module depends only on the standard library. Commands project their
release, guard and manifest policies from the same inspected source.

A constant declares its upgrade policy with `/// version_surface = "coexist"`
and its grammar with the existing `version_guard` marker. No registry row is
needed. `format_manifest` names a DurableFormat variant or engine:<id>;
`format_outside_manifest` states why a surface is not in those manifests.
`version_unguarded` explains a migrate surface outside the decoder-law table.
Identity and key grammars without manifest markers are outside the manifest.
String versions must be scalar constants so the cut can reset them.
Reservation arrays may declare `version_reservations = "<reason>"`; those
historical names are immutable and are not current writer versions.
"""
from __future__ import annotations

from bisect import bisect_left, bisect_right
from dataclasses import dataclass
import fnmatch
from functools import lru_cache
import os
from pathlib import Path
import re
import tomllib
from typing import Iterable

REGISTRY = "scripts/versioned-surfaces.toml"
RUST_SOURCE_PATTERNS = ("crates/*.rs", "examples/*.rs")


class SourceError(RuntimeError):
    """Source inspection cannot establish the declared version inventory."""


class WorktreeSources:
    """The files on disk, so the registry check reads what a commit would."""

    label = "working tree"

    def __init__(self, repo: Path) -> None:
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

    def preload(self, paths: Iterable[str]) -> None:
        for path in paths:
            self.content(path)


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



@dataclass(frozen=True)
class Lexeme:
    kind: str
    start: int
    end: int
    value: str


# Literals/comments are indivisible; every consumer uses these same offsets.
_LEXEME = re.compile(
    r"\s+|//[^\n]*|/\*|b?r\#*\"|b?\"|b?'|[A-Za-z_][A-Za-z0-9_]*|"
    r"[0-9][0-9A-Za-z_]*(?:\.[0-9][0-9A-Za-z_]*)?|::|->|=>|.", re.DOTALL,
)


@lru_cache(maxsize=8192)
def rust_lexemes(text: str) -> tuple[Lexeme, ...]:
    result = []
    index = 0
    while match := _LEXEME.match(text, index):
        start, end = match.span()
        value = match[0]
        kind = "punct"
        if value.isspace():
            kind = "space"
        elif value.startswith("//"):
            kind = "comment"
        elif value == "/*":
            depth = 1
            while depth and end < len(text):
                token = re.search(r"/\*|\*/", text[end:])
                if token is None:
                    break
                end += token.end()
                depth += 1 if token[0] == "/*" else -1
            kind = "comment" if not depth else "malformed"
            if depth:
                end = len(text)
            value = "unterminated Rust block comment"
        elif raw := _raw_string_start(text, start):
            content_start, closer = raw
            closing = text.find(closer, content_start)
            kind = "literal" if closing >= 0 else "malformed"
            end = closing + len(closer) if closing >= 0 else len(text)
            value = text[content_start:closing] if closing >= 0 else "unterminated Rust raw string"
        elif value in ('"', 'b"'):
            content_start = end
            while end < len(text) and text[end] != '"':
                end += 2 if text[end] == "\\" else 1
            kind = "literal" if end < len(text) else "malformed"
            value = text[content_start:end] if kind == "literal" else "unterminated Rust string"
            end = min(end + 1, len(text))
        elif char_end := _char_literal_end(text, start):
            kind, end = "char", char_end
            value = text[start:end]
        elif value == "'" and (name := re.match(r"[A-Za-z_][A-Za-z0-9_]*", text[end:])):
            kind, value = "lifetime", name[0]
            end += len(value)
        elif value[0].isalpha() or value[0] == "_":
            kind = "ident"
        elif value[0].isdigit():
            kind = "number"
        result.append(Lexeme(kind, start, end, value))
        index = end
    return tuple(result)


def lexemes_from(text: str, start: int = 0):
    lexemes = rust_lexemes(text)
    index = bisect_left(lexemes, start, key=lambda token: token.start)
    return (lexemes[at] for at in range(index, len(lexemes)))


@lru_cache(maxsize=8192)
def without_comments(text: str) -> str:
    """Mask comments, preserving literal contents, newlines and source offsets."""
    result = list(text)
    for token in rust_lexemes(text):
        if token.kind == "malformed":
            raise SourceError(token.value)
        if token.kind == "comment":
            result[token.start:token.end] = ["\n" if c == "\n" else " "
                                              for c in text[token.start:token.end]]
    return "".join(result)


@lru_cache(maxsize=8192)
def strip_rust_trivia(text: str) -> str:
    tokens = rust_lexemes(text)
    for token in tokens:
        if token.kind == "malformed":
            raise SourceError(token.value)
    return "".join(text[t.start:t.end] for t in tokens if t.kind not in {"space", "comment"})


Token = tuple[str, str]


@lru_cache(maxsize=8192)
def rust_tokens(text: str) -> tuple[Token, ...]:
    tokens = rust_lexemes(text)
    for token in tokens:
        if token.kind == "malformed":
            raise SourceError(token.value)
    return tuple((t.kind, t.value) for t in tokens if t.kind not in {"space", "comment"})


def rust_item_end(text: str, start: int) -> int:
    """Find an item's terminator outside literals and nested delimiters."""
    depths = {"{": 0, "[": 0, "(": 0}
    pairs = {"}": "{", "]": "[", ")": "("}
    saw_brace = False
    for token in lexemes_from(text, start):
        if token.kind == "malformed":
            raise SourceError(token.value + " while extracting guarded item")
        if token.kind != "punct":
            continue
        char = token.value
        if char in depths:
            if char == "{" and not any(depths.values()):
                saw_brace = True
            depths[char] += 1
        elif char in pairs:
            depths[pairs[char]] -= 1
            if char == "}" and saw_brace and depths["{"] == 0:
                return token.end
        elif char == ";" and not any(depths.values()):
            return token.end
    raise SourceError("unterminated Rust item while extracting guarded shape")


def rust_attribute_end(text: str, start: int) -> int:
    if not text.startswith("#[", start):
        raise SourceError("Rust attribute extraction did not start at #[")
    depth = 1
    for token in lexemes_from(text, start + 2):
        if token.kind == "malformed":
            raise SourceError(token.value + " while extracting attribute")
        if token.kind == "punct":
            if token.value == "[":
                depth += 1
            elif token.value == "]":
                depth -= 1
                if depth == 0:
                    return token.end
    raise SourceError("unterminated Rust outer attribute")


@lru_cache(maxsize=8192)
def rust_outer_attribute_ranges(text: str) -> tuple[tuple[int, int], ...]:
    ranges = []
    cursor = 0
    for token in rust_lexemes(text):
        if token.start < cursor:
            continue
        if token.kind == "malformed":
            # Malformed trivia cannot prove a region test-only. Recover visible
            # attributes conservatively so their cfg grammar is still checked.
            at = token.start + 2
            while (at := text.find("#[", at)) >= 0:
                try:
                    end = rust_attribute_end(text[at:], 0) + at
                except SourceError:
                    at += 2
                else:
                    ranges.append((at, end))
                    at = end
            break
        if token.kind == "punct" and text.startswith("#[", token.start):
            cursor = rust_attribute_end(text, token.start)
            ranges.append((token.start, cursor))
    return tuple(ranges)


def _rust_trivia_end(text: str, start: int) -> int | None:
    tokens = rust_lexemes(text)
    index = bisect_right(tokens, start, key=lambda token: token.end)
    cursor = start
    for at in range(index, len(tokens)):
        token = tokens[at]
        if token.kind == "malformed":
            return None
        if token.kind not in {"space", "comment"}:
            break
        cursor = token.end
    return cursor


def _rust_group_end(text: str, open_index: int) -> int | None:
    if open_index >= len(text) or text[open_index] != "(":
        return None
    depth = 1
    for token in lexemes_from(text, open_index + 1):
        if token.kind == "malformed":
            return None
        if token.kind == "punct":
            if token.value == "(":
                depth += 1
            elif token.value == ")":
                depth -= 1
                if not depth:
                    return token.end
    return None


def _rust_top_level_items(text: str, start: int, end: int) -> tuple[tuple[int, int], ...] | None:
    items, delimiters = [], []
    pairs = {"(": ")", "[": "]", "{": "}"}
    item_start = start
    for token in lexemes_from(text, start):
        if token.start >= end:
            break
        if token.kind == "malformed" or token.end > end:
            return None
        if token.kind != "punct":
            continue
        char = token.value
        if char in pairs:
            delimiters.append(pairs[char])
        elif char in ")]}":
            if not delimiters or delimiters.pop() != char:
                return None
        elif char == "," and not delimiters:
            items.append((item_start, token.start))
            item_start = token.end
    if delimiters:
        return None
    return tuple((*items, (item_start, end)))


def _is_test_source(path: str) -> bool:
    parts = path.split("/")
    return ("tests" in parts[:-1] or "benches" in parts[:-1]
            or parts[-1] in {"tests.rs", "test.rs"} or parts[-1].endswith("_tests.rs"))


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
        trivia_end = _rust_trivia_end(text, end)
        if trivia_end is None or trivia_end < cursor:
            break
        cursor = start
        eligible.pop()
    return cursor


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
        except SourceError:
            continue
        if text[item_end - 1] == "}":
            ranges.append((module.end(), item_end - 1))
    return tuple(ranges)


CONST = re.compile(
    r'(?m)^[ \t]*(?P<attrs>(?:#\[[^\]]*\]\s*)*)'
    r'(?:pub(?:\([^)]*\))?\s+)?const\s+(?P<name>[A-Z][A-Z0-9_]*)'
    r'\s*:\s*(?P<type>[^=;]+)(?:=\s*(?P<value>[^;]+))?;'
)


def definitions(text: str):
    """Constant initializers and their attributes for release-tier resolution."""
    return [match for match in CONST.finditer(without_comments(text))
            if match["value"] is not None]


TAG = re.compile(r'^[A-Za-z][A-Za-z0-9_.:/-]*?[/:-]v[0-9]+(?:[/:-][A-Za-z0-9_./:{}-]*)?$')
DECLARATION = re.compile(
    r'(?m)^\s*///\s*(version_surface|format_manifest|format_outside_manifest|version_unguarded)'
    r'\s*=\s*(.*?)\s*$'
)
RESERVATIONS = re.compile(r'(?m)^\s*///\s*version_reservations\s*=\s*"([^"\n]+)"\s*$')


def production_path(path):
    parts = path.split('/')
    return (not _is_test_source(path)
            and not any(part.endswith('_tests') or part == 'testing' for part in parts)
            and not path.startswith(('crates/lash-conformance/', 'crates/lash-sim/', 'crates/lash-perf/'))
            and '/examples/' not in path and '/benches/' not in path)


def production_text(text):
    """Mask comments, test modules and attributes, preserving source offsets."""
    attributes = rust_outer_attribute_ranges(text)
    masked = list(without_comments(text))
    for start, end in (*attributes, *test_only_module_ranges(text, attributes)):
        masked[start:end] = ' ' * (end - start)
    return ''.join(masked)


def inspect(view, *, unregistered=None):
    registry = tomllib.loads(view.content(REGISTRY) or '')
    if 'surface' in registry:
        raise SourceError('[[surface]] declarations are unsupported; declare version_surface in source')
    excluded = {f'{row["constant_path"]}:{row["constant"]}'
                for row in registry.get('unregistered', [])}
    if unregistered is not None:
        excluded = set(unregistered)
    excluded = frozenset(excluded)
    rows, problems, version_constants = {}, [], set()
    paths = view.matching_paths(RUST_SOURCE_PATTERNS)
    view.preload(paths)
    for path in paths:
        found, errors, constants = _inspect_file(path, view.content(path), excluded)
        version_constants.update(constants)
        for row in found:
            key = f'{row["constant_path"]}:{row["constant"]}'
            if key in rows and rows[key] != row:
                problems.append(f'{key}: conflicting source declarations')
            rows[key] = row.copy()
        problems.extend(errors)
    return Inventory(tuple(rows.values()), tuple(problems), frozenset(version_constants))


@dataclass(frozen=True)
class Inventory:
    surfaces: tuple[dict, ...]
    problems: tuple[str, ...]
    # The format policy includes examples and support crates that the release
    # policy does not police for inline writer tags. Both use the same scan.
    version_constants: frozenset[str]


def discover(view, *, unregistered=None, enforce=True):
    inventory = inspect(view, unregistered=unregistered)
    if enforce and inventory.problems:
        raise SourceError("\n".join(inventory.problems))
    return inventory.surfaces, list(inventory.problems)


@lru_cache(maxsize=8192)
def _inspect_file(path, text, excluded):
    # Mutant trees used by the guard laws change one file. Reuse the scan of
    # each unchanged file while still scanning every tree for new versions.
    rows, problems = {}, []
    masked = production_text(text)
    declarations = list(CONST.finditer(masked))
    constants = [match for match in declarations if match["value"] is not None]
    version_constants = frozenset(
        f'{path}:{match["name"]}' for match in declarations
        if not _is_test_source(path)
        and re.fullmatch(r'[A-Z][A-Z0-9_]*(?:_VERSION|_EPOCH|_FORMAT)', match["name"])
    )
    attributes = rust_outer_attribute_ranges(text)

    def block_for(match):
        start = rust_item_start_with_attributes(text, match.start(), attributes)
        while start > 0:
            line_start = text.rfind("\n", 0, start - 1) + 1
            if not text[line_start:start].lstrip().startswith("//"):
                break
            start = line_start
        return text[start:match.start()]

    reserved = []
    for match in constants:
        name = match['name']
        key = f'{path}:{name}'
        if match['value'].strip() == 'env!("CARGO_PKG_VERSION")':
            continue
        block = block_for(match)
        metadata = {}
        invalid = False
        for marker in DECLARATION.finditer(block):
            field, expression = marker.groups()
            try:
                value = tomllib.loads('value = ' + expression)['value']
                if not isinstance(value, str) or not value.strip():
                    raise ValueError('expected a non-empty string')
                if field == 'version_surface' and value not in ('migrate', 'drain', 'coexist'):
                    raise ValueError('expected migrate, drain or coexist')
                if field in metadata and metadata[field] != value:
                    raise ValueError('conflicting source declarations')
                metadata[field] = value
            except (tomllib.TOMLDecodeError, KeyError, ValueError) as error:
                problems.append(f'{key}: invalid {field}: {error}')
                invalid = True
        if invalid:
            continue
        reasons = [m[1] for m in RESERVATIONS.finditer(block)]
        if reasons:
            if match['type'].strip() != '&[&str]' or not all(r.strip() for r in reasons):
                problems.append(f'{key}: version reservations must name a string array and a reason')
            else:
                reserved.append(match.span('value'))
            continue
        if 'version_surface' in metadata:
            row = dict(constant_path=path, constant=name, upgrade=metadata['version_surface'])
            for marker, field in (('format_manifest', 'manifest'),
                                  ('format_outside_manifest', 'outside_manifest'),
                                  ('version_unguarded', 'unguarded')):
                if marker in metadata:
                    row[field] = metadata[marker]
            if not any(field in row for field in ('manifest', 'outside_manifest')):
                row['outside_manifest'] = 'source-declared identity or key grammar'
            if key in rows and rows[key] != row:
                problems.append(f'{key}: conflicting source declarations')
            rows[key] = row
        elif metadata:
            problems.append(f'{key}: format metadata needs version_surface')
        elif (production_path(path) and re.search(r'(?:^|_)(?:VERSION|EPOCH|FORMAT|V[0-9]+)$', name) and key not in excluded):
            problems.append(f'{key} is a version-shaped constant the registry does not know: '
                            'declare version_surface and version_guard')
    if not production_path(path):
        return tuple(rows.values()), tuple(problems), version_constants
    for call in re.finditer(
        r'\b(?:IdentityEncoder::new|rendered_hash)\(\s*"(?:\\.|[^"\\])*"\s*,\s*([0-9][0-9_]*(?:u8|u16|u32)?)\b',
        masked,
    ):
        problems.append(f'{path}:{text.count(chr(10), 0, call.start()) + 1}: '
                        f'unregistered inline family version {call[1]}; use a source-declared constant')
    owned = rows.keys() | excluded
    for literal in rust_lexemes(text):
        if literal.kind != "literal" or masked[literal.start:literal.end] != text[literal.start:literal.end]:
            continue
        raw, value = text[literal.start:literal.end], literal.value
        # A byte string is not a scalar string version that the release
        # resolver can reset; retain the same refusal as the source grammar.
        if raw.startswith(('b"', 'br')):
            raw = raw[1:]
        if (not TAG.fullmatch(value) or value.startswith(('http:', 'https:'))
                or not (re.search(r'[/ :]v[0-9]', value) or value.startswith('lash'))):
            continue
        if any(start <= literal.start < end for start, end in reserved):
            continue
        owner = next((m for m in constants if m.start('value') <= literal.start < m.end('value')), None)
        if owner is not None and f'{path}:{owner["name"]}' in owned:
            # A scalar string can be resolved and reset. Arrays of active
            # writer tags cannot hide behind one numeric counter.
            if owner['value'].strip() == raw:
                continue
        problems.append(f'{path}:{text.count(chr(10), 0, literal.start) + 1}: '
                        f'unregistered version literal {value!r}; use a source-declared scalar constant')
    return tuple(rows.values()), tuple(problems), version_constants
