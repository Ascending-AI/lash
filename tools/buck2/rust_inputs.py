"""Conservative Rust module/include closure for opted-in integration tests.

All cfg branches are inputs. Unsupported computed paths retain the package
fallback rather than silently dropping a compiler input. Runtime reads are
declared separately in source-ownership.json's test_data.
"""

from __future__ import annotations

import ast
import pathlib
import re
from dataclasses import dataclass, field


class Underivable(ValueError):
    pass


@dataclass
class Inputs:
    sources: set[str] = field(default_factory=set)
    includes: set[str] = field(default_factory=set)


def tokens(source: str) -> list[str]:
    """Ignore comments, keep literals atomic (including raw Rust strings)."""
    result = []
    index = 0
    while index < len(source):
        tail = source[index:]
        if tail.startswith("//"):
            end = source.find("\n", index)
            index = len(source) if end < 0 else end + 1
            continue
        if tail.startswith("/*"):
            depth = 1
            index += 2
            while depth and index < len(source):
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            if depth:
                raise Underivable("unterminated block comment")
            continue
        raw = re.match(r'(?:br|cr|r)(#*)"', tail)
        if raw:
            closing = '"' + raw[1]
            end = source.find(closing, index + raw.end())
            if end < 0:
                raise Underivable("unterminated raw string")
            result.append(source[index:end + len(closing)])
            index = end + len(closing)
            continue
        literal = re.match(r'''(?:b|c)?"(?:\\.|[^"\\])*"|'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])' ''', tail, re.X | re.S)
        word = re.match(r"(?:r#)?[A-Za-z_][A-Za-z_0-9]*", tail)
        match = literal or word
        if match:
            result.append(match[0])
            index += match.end()
        else:
            if not source[index].isspace():
                result.append(source[index])
            index += 1
    return result


def string(token: str) -> str:
    if token.startswith('"'):
        try:
            value = ast.literal_eval(token)
        except (ValueError, SyntaxError) as error:
            raise Underivable("unsupported escaped path") from error
        if isinstance(value, str):
            return value
    raw = re.fullmatch(r'r(#+)?"(.*)"\1', token, re.S)
    if raw:
        return raw[2]
    if token.startswith('r"') and token.endswith('"'):
        return token[2:-1]
    raise Underivable("non-literal path")


def closure(package: pathlib.Path, root: str, *, token_cache: dict | None = None) -> Inputs:
    package = package.resolve()
    found = Inputs()
    visited = set()
    cache = {} if token_cache is None else token_cache

    def relative(path: pathlib.Path) -> str:
        try:
            return path.resolve().relative_to(package).as_posix()
        except ValueError as error:
            raise Underivable("module/include leaves the package") from error

    def read(path: pathlib.Path, module_dir: pathlib.Path | None, path_dir: pathlib.Path | None) -> None:
        name = relative(path)
        key = (name, module_dir, path_dir)
        if key in visited:
            return
        visited.add(key)
        if not path.is_file():
            raise Underivable(f"missing module/include {name}")
        found.sources.add(name)
        if path not in cache:
            cache[path] = tokens(path.read_text())
        scan(cache[path], path, module_dir, path_dir)

    def scan(items: list[str], path: pathlib.Path, module_dir: pathlib.Path | None, path_dir: pathlib.Path | None) -> None:
        index = 0
        attributes: list[str] = []
        while index < len(items):
            token = items[index]
            if token == "#" and items[index + 1:index + 2] == ["["]:
                start = index + 2
                index = end_group(items, index + 1)
                attribute = items[start:index - 1]
                # #[doc = include_str!(...)] is a compiler input too.
                scan(attribute, path, module_dir, path_dir)
                attributes.extend(attribute)
                continue
            if token == "pub" and items[index + 1:index + 2] == ["("]:
                # Restricted visibility may name any ancestor module. Keep
                # preceding #[path] attributes across its whole token group.
                index = end_group(items, index + 1)
                continue
            if token == "mod" and items[index + 2:index + 3] in ([";"], ["{"]):
                name = items[index + 1].removeprefix("r#")
                if not name.isidentifier():
                    raise Underivable("macro-generated module")
                paths = []
                for pos, item in enumerate(attributes):
                    if item == "path" and attributes[pos + 1:pos + 2] == ["="]:
                        if path_dir is None or "cfg_attr" in attributes:
                            raise Underivable("conditional or macro-relative module path")
                        paths.append(path_dir / string(attributes[pos + 2]))
                if items[index + 2] == ";":
                    if not paths:
                        if module_dir is None:
                            raise Underivable("macro-relative module")
                        paths = [p for p in (module_dir / f"{name}.rs", module_dir / name / "mod.rs") if p.is_file()]
                        if len(paths) != 1:
                            raise Underivable(f"ambiguous or missing module {name}")
                    for child in paths:
                        child = child.resolve()
                        child_dir = child.parent if child.name == "mod.rs" else child.with_suffix("")
                        read(child, child_dir, child.parent)
                    index += 3
                else:
                    end = end_group(items, index + 2)
                    dirs = paths or [module_dir / name if module_dir else None]
                    for child_dir in dirs:
                        scan(items[index + 3:end - 1], path, child_dir, child_dir)
                    index = end
                attributes = []
                continue
            if token == "mod" and items[index + 1:index + 2] == ["$"]:
                # Backend macros generate inline test modules. Their bodies
                # still contribute literal includes, but an out-of-line
                # module below an unknown name cannot be resolved safely.
                if items[index + 3:index + 4] != ["{"]:
                    raise Underivable("macro-generated external module")
                end = end_group(items, index + 3)
                scan(items[index + 4:end - 1], path, None, None)
                index = end
                attributes = []
                continue
            if token in {"include", "include_str", "include_bytes"} and items[index + 1:index + 2] == ["!"]:
                start = index + 2
                end = end_group(items, start)
                arguments = items[start + 1:end - 1]
                if arguments[-1:] == [","]:
                    arguments.pop()
                if len(arguments) != 1:
                    raise Underivable("computed include path")
                child = (path.parent / string(arguments[0])).resolve()
                name = relative(child)
                if not child.is_file():
                    raise Underivable(f"missing include {name}")
                if token == "include":
                    # Module resolution inside an included token stream can
                    # depend on expansion context. Keep literal includes,
                    # fall back if it introduces external modules.
                    read(child, None, None)
                else:
                    found.includes.add(name)
                index = end
                continue
            # Visibility may intervene between an attribute and its module.
            if token not in {"pub", "(", ")", "crate", "super", "self", "in", ":"}:
                attributes = []
            index += 1

    path = (package / root).resolve()
    # Crate roots own modules beside the root, regardless of the root's name.
    read(path, path.parent, path.parent)
    return found


def end_group(items: list[str], start: int) -> int:
    pairs = {"(": ")", "[": "]", "{": "}"}
    if start >= len(items) or items[start] not in pairs:
        raise Underivable("unsupported token group")
    stack = [pairs[items[start]]]
    index = start + 1
    while stack and index < len(items):
        token = items[index]
        if token in pairs:
            stack.append(pairs[token])
        elif token == stack[-1]:
            stack.pop()
        index += 1
    if stack:
        raise Underivable("unclosed token group")
    return index
