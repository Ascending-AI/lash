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

Only the Python standard library is used so the check can run before the Rust
toolchain is installed.  Pull-request CI passes the PR merge-base explicitly.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import fnmatch
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Iterable


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG = Path(__file__).with_name("versioned-surfaces.toml")


@dataclass(frozen=True)
class Guard:
    kind: str
    paths: tuple[str, ...]
    symbols: tuple[str, ...] = ()
    must_cover: tuple[str, ...] = ()


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


@dataclass(frozen=True)
class SurfaceError:
    surface: Surface
    detail: str


@dataclass(frozen=True)
class CheckResult:
    failures: tuple[Failure, ...]
    errors: tuple[SurfaceError, ...]


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
            guards.append(
                Guard(kind, tuple(paths), tuple(symbols), tuple(must_cover))
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
                    "unterminated Rust raw string while finding outer attributes"
                )
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
    for match in RUST_SERDE_SHAPE.finditer(text):
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
) -> tuple[tuple[str, str], ...]:
    paths = view.matching_paths(revision, guard.paths)
    signature: list[tuple[str, str]] = []
    found_symbols: set[str] = set()
    covered_shapes: set[str] = set()
    covered_markers: set[str] = set()
    for path in paths:
        content = view.content(revision, path)
        if content is None:
            continue
        if guard.kind == "file":
            signature.append((path, content))
            covered_markers.update(
                marker for marker in guard.must_cover if marker in content
            )
        elif guard.kind == "rust_items":
            items = named_rust_items(content, guard.symbols)
            found_symbols.update(items)
            signature.extend((f"{path}:{name}", value) for name, value in items.items())
        else:
            items = serde_shapes(content)
            covered_shapes.update(name.partition("#")[0] for name in items)
            signature.extend((f"{path}:{name}", value) for name, value in items.items())

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


def check_surfaces(
    repo: Path, base: str, head: str, surfaces: Iterable[Surface]
) -> CheckResult:
    base_revision = resolve_revision(repo, base)
    head_revision = resolve_revision(repo, head)
    view = RepositoryView(repo)
    failures: list[Failure] = []
    errors: list[SurfaceError] = []
    for surface in surfaces:
        try:
            head_version = version_at(view, head_revision, surface)
        except CheckError as error:
            errors.append(SurfaceError(surface, str(error)))
            continue
        changed_guards: list[str] = []
        for guard in surface.guards:
            try:
                base_signature = guard_signature(
                    view, base_revision, guard, enforce_presence=False
                )
                head_signature = guard_signature(
                    view, head_revision, guard, enforce_presence=True
                )
            except CheckError as error:
                errors.append(
                    SurfaceError(
                        surface,
                        f"guard {', '.join(guard.paths)}: {error}",
                    )
                )
                continue
            if base_signature != head_signature:
                changed_guards.extend(guard.paths)
        if not changed_guards:
            continue
        try:
            base_version = version_at(view, base_revision, surface)
        except CheckError as error:
            errors.append(SurfaceError(surface, str(error)))
            continue
        if head_version <= base_version:
            failures.append(
                Failure(
                    surface=surface,
                    base_version=base_version,
                    head_version=head_version,
                    changed_guards=tuple(dict.fromkeys(changed_guards)),
                )
            )
    return CheckResult(tuple(failures), tuple(errors))


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
        result = check_surfaces(args.repo, base, head, surfaces)
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

    if result.failures:
        print(
            f"version-bump check failed against merge-base {base[:12]}:",
            file=sys.stderr,
        )
        for failure in result.failures:
            paths = ", ".join(failure.changed_guards)
            print(
                f"- {failure.surface.constant} is {failure.head_version}; merge-base "
                f"value is {failure.base_version}. Guarded shape changed ({paths}). Bump "
                f"{failure.surface.constant} strictly past {failure.base_version}.",
                file=sys.stderr,
            )
    if result.errors:
        return 2
    if result.failures:
        return 1

    print(
        f"version-bump check passed: {len(surfaces)} surfaces against "
        f"merge-base {base[:12]}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
