"""Discover ignored Rust tests by their ignore reason, from Rust modules and
Cargo target roots.

Generators declare #[ignore = "regenerates <repository-relative path>"] and
require LASH_REGENERATE=1. Normal fixture checks remain separate tests.
"""

from pathlib import Path
import json
import re
import tomllib

import release_baseline as baseline

REGENERATES = re.compile(r'#\[ignore\s*=\s*"regenerates ([^"\n]+)"\]')
TOKEN = re.compile(r'''r(?P<hashes>\#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"|'(?:\\(?:u\{[0-9a-fA-F]+\}|x[0-9a-fA-F]{2}|.)|[^'\\])'|//[^\n]*|/\*|[A-Za-z_][A-Za-z_0-9]*|\S''', re.DOTALL)


def without_comments(text: str) -> str:
    result = list(text)
    at = 0
    while match := TOKEN.search(text, at):
        at = match.end()
        if match[0] == "/*":
            depth = 1
            while depth:
                token = re.search(r"/\*|\*/", text[at:])
                if token is None:
                    raise baseline.BaselineError("unterminated Rust block comment")
                at += token.end()
                depth += 1 if token[0] == "/*" else -1
        elif not match[0].startswith("//"):
            continue
        result[match.start():at] = " " * (at - match.start())
    return "".join(result)


def ignored_tests(repo: Path, reason: re.Pattern) -> list[dict]:
    """Every test whose `#[ignore = ...]` attribute matches `reason`, as
    `{target, law, source, reason}` with `reason` the attribute's match."""
    found = []
    visited = set()

    def file(path, modules, target, module_dir):
        key = (path, tuple(modules), target)
        if key in visited or not path.is_file():
            return
        visited.add(key)
        source = without_comments(path.read_text())
        tokens = list(TOKEN.finditer(source))

        def block(start, end, names, directory, attribute_dir):
            attrs = []
            at = start
            while at < end:
                value = tokens[at][0]
                if value == "#" and at + 1 < end and tokens[at + 1][0] == "[":
                    stop = matching(at + 1, "[", "]")
                    attrs.append(source[tokens[at].start():tokens[stop].end()])
                    at = stop + 1
                    continue
                if value == "pub" and at + 1 < end and tokens[at + 1][0] == "(":
                    at = matching(at + 1, "(", ")") + 1
                    continue
                if value == "mod" and at + 2 < end:
                    name, following = tokens[at + 1][0], tokens[at + 2][0]
                    if following == "{":
                        stop = matching(at + 2, "{", "}")
                        block(at + 3, stop, [*names, name], directory / name, directory / name)
                        at = stop + 1
                    elif following == ";":
                        override = re.search(r'#\[path\s*=\s*("[^"\n]+")\]', "\n".join(attrs))
                        if override:
                            child = attribute_dir / json.loads(override[1])
                        else:
                            child = directory / f"{name}.rs"
                            if not child.is_file():
                                child = directory / name / "mod.rs"
                        child_dir = child.parent if child.name == "mod.rs" else child.with_suffix("")
                        file(child, [*names, name], target, child_dir)
                        at += 3
                    else:
                        at += 1
                    attrs = []
                    continue
                if value == "fn" and at + 1 < end:
                    declaration = "\n".join(attrs)
                    ignore = reason.search(declaration)
                    if ignore:
                        if not re.search(r'#\[(?:tokio::)?test(?:\(|\])', declaration):
                            raise baseline.BaselineError(f"{ignore[0]} is not on a test: {path}")
                        found.append(dict(target=target, law="::".join([*names, tokens[at + 1][0]]),
                                          source=str(path.relative_to(repo)), reason=ignore))
                    attrs = []
                elif value not in ("pub", "async", "unsafe"):
                    attrs = []
                if value in ("{", "(", "["):
                    at = matching(at, value, {"{": "}", "(": ")", "[": "]"}[value]) + 1
                else:
                    at += 1

        def matching(start, opening, closing):
            depth = 1
            for at in range(start + 1, len(tokens)):
                if tokens[at][0] == opening:
                    depth += 1
                elif tokens[at][0] == closing:
                    depth -= 1
                    if depth == 0:
                        return at
            raise baseline.BaselineError(f"unclosed {opening} in {path}")

        block(0, len(tokens), modules, module_dir, path.parent)

    for manifest in sorted((repo / "crates").glob("*/Cargo.toml")):
        package = manifest.parent
        if not any(reason.search(path.read_text()) for path in package.rglob("*.rs")):
            continue
        config = tomllib.loads(manifest.read_text())
        lib = package / config.get("lib", {}).get("path", "src/lib.rs")
        label = f"//{package.relative_to(repo)}:"
        file(lib, [], label + package.name + "__unit_test", lib.parent)
        # A binary-only package's unit tests are its main.rs's.
        main = package / "src/main.rs"
        if not lib.is_file() and main.is_file():
            file(main, [], label + package.name + "__unit_test", main.parent)
        tests = {path.stem: path for path in (package / "tests").glob("*.rs")}
        tests.update({test["name"]: package / test.get("path", f'tests/{test["name"]}.rs')
                      for test in config.get("test", [])})
        for name, path in sorted(tests.items()):
            file(path, [], label + name + "__test", path.parent)
    return sorted(found, key=lambda row: (row["target"], row["law"]))


def discover(repo: Path) -> list[dict]:
    generators = []
    for test in ignored_tests(repo, REGENERATES):
        output = test.pop("reason")[1]
        if Path(output).is_absolute() or ".." in Path(output).parts:
            raise baseline.BaselineError(f"invalid regenerator output: {output}")
        generators.append(dict(test, output=output, environment={"LASH_REGENERATE": "1"}))
    return generators
