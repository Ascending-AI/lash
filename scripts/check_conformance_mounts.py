#!/usr/bin/env python3
"""Derive conformance mounts from Rust test bodies and registration macros."""

from collections import defaultdict
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
IDENT = re.compile(r"\b[A-Za-z_][A-Za-z_0-9]*\b")
ITEM = re.compile(r"\b(?:fn\s+|macro_rules!\s*)([A-Za-z_][A-Za-z_0-9]*)")


def code_only(source):
    """Mask comments and literals, retaining offsets, attributes and identifiers."""
    pattern = re.compile(
        r'//[^\n]*|/\*.*?\*/|(?:br|r)(?P<hash>\#*)".*?"(?P=hash)'
        r'|b?"(?:\\.|[^"\\])*"|b?\'(?:\\.|[^\'\\])\'',
        re.S,
    )
    return pattern.sub(lambda match: " " * len(match.group()), source)


def closing_brace(code, start):
    depth = 1
    for index in range(start + 1, len(code)):
        depth += (code[index] == "{") - (code[index] == "}")
        if depth == 0:
            return index + 1
    raise ValueError("unclosed Rust body")


def source_paths(root):
    """Follow canonical Cargo targets and Rust module declarations."""
    pending = []
    for manifest in (root / "crates").rglob("Cargo.toml"):
        package = tomllib.loads(manifest.read_text())
        directory = manifest.parent
        pending.append(directory / package.get("lib", {}).get("path", "src/lib.rs"))
        for kind in ("bin", "test"):
            for target in package.get(kind, []):
                default = f"src/bin/{target['name']}.rs" if kind == "bin" else f"tests/{target['name']}.rs"
                pending.append(directory / target.get("path", default))
        pending.append(directory / "src/main.rs")
        if package.get("package", {}).get("autobins", True):
            pending.extend((directory / "src/bin").glob("*.rs"))
            pending.extend((directory / "src/bin").glob("*/main.rs"))
        if package.get("package", {}).get("autotests", True):
            pending.extend((directory / "tests").glob("*.rs"))
            pending.extend((directory / "tests").glob("*/main.rs"))
    seen = set()
    while pending:
        path = pending.pop().resolve()
        if path in seen or not path.is_file():
            continue
        seen.add(path)
        source = path.read_text()
        code = code_only(source)
        for inclusion in re.finditer(r'\binclude!\s*\(\s*"([^"]+)"\s*\)', source):
            if code[inclusion.start():].startswith("include!"):
                pending.append(path.parent / inclusion.group(1))
        base = path.parent if path.stem in {"lib", "main", "mod"} or path.parent.name == "tests" else path.with_suffix("")
        inline = []
        for module in re.finditer(r"\bmod\s+(\w+)\s*([;{])", code):
            inline = [(name, end) for name, end in inline if end > module.start()]
            directory = base.joinpath(*(name for name, _ in inline))
            if module.group(2) == "{":
                inline.append((module.group(1), closing_brace(code, module.end() - 1)))
                continue
            prefix_start = max(code.rfind(";", 0, module.start()), code.rfind("}", 0, module.start())) + 1
            attributes = source[prefix_start:module.start()]
            explicit = re.findall(r'#\[path\s*=\s*"([^"]+)"\]', attributes)
            if explicit:
                attribute_base = directory if inline else path.parent
                pending.append(attribute_base / explicit[-1])
            else:
                pending.extend((directory / f"{module.group(1)}.rs", directory / module.group(1) / "mod.rs"))
    return sorted(seen)


def inventory(root):
    bodies = defaultdict(set)
    laws = {}
    roots = set()
    generated = {}
    macros = {}
    sources = {}
    for path in source_paths(root):
        code = code_only(path.read_text())
        sources[path] = code
        item_end = 0
        for match in ITEM.finditer(code):
            if match.start() < item_end:
                continue
            start = code.find("{", match.end())
            semicolon = code.find(";", match.end())
            if start < 0 or 0 <= semicolon < start:
                continue
            end = closing_brace(code, start)
            item_end = end
            name = match.group(1)
            if "macro_rules!" in match.group():
                macros[name] = code[start:end]
            if "macro_rules!" in match.group() and path.is_relative_to(root / "crates/lash-conformance/src/conformance"):
                parameter = re.search(r"pub (?:async )?fn \$(\w+)", code[start:end])
                if parameter:
                    generated[name] = parameter.group(1)
            bodies[(path, name)].update(IDENT.findall(code[start:end]))
            line_start = code.rfind("\n", 0, match.start()) + 1
            declaration = code[line_start:match.end()].strip()
            if path.is_relative_to(root / "crates/lash-conformance/src/conformance"):
                if re.match(r"pub (?:async )?fn ", declaration):
                    laws[name] = ((path, name), f"{path.relative_to(root)}:{code.count(chr(10), 0, match.start()) + 1}")
            prefix = code[max(code.rfind("}", 0, match.start()),
                              code.rfind(";", 0, match.start())) + 1:match.start()]
            if name == "main" or re.search(r"#\[(?:tokio::)?test\b", prefix):
                roots.add((path, name))
    test_macros = {name for name, body in macros.items()
                   if re.search(r"#\[(?:tokio::)?test\b", body)}
    while True:
        parents = {name for name, body in macros.items()
                   if test_macros.intersection(re.findall(r"\b(\w+)\s*!", body))}
        if parents <= test_macros:
            break
        test_macros.update(parents)
    for path, code in sources.items():
        if path.is_relative_to(root / "crates/lash-conformance/src/conformance"):
            continue
        outside = list(code)
        item_end = 0
        for match in ITEM.finditer(code):
            if match.start() < item_end:
                continue
            start = code.find("{", match.end())
            semicolon = code.find(";", match.end())
            if start < 0 or 0 <= semicolon < start:
                continue
            item_end = closing_brace(code, start)
            outside[match.start():item_end] = " " * (item_end - match.start())
        # An import, unused registration macro, or non-test macro is not a mount.
        for invocation in re.finditer(r"\b(\w+)\s*!\s*([({\[])", "".join(outside)):
            name = invocation.group(1)
            if name not in test_macros:
                continue
            roots.add((path, name))
            opening = invocation.end() - 1
            delimiter = code[opening]
            closing = {"(": ")", "{": "}", "[": "]"}[delimiter]
            depth = 1
            for index in range(opening + 1, len(code)):
                depth += (code[index] == delimiter) - (code[index] == closing)
                if depth == 0:
                    roots.update((path, identifier) for identifier in IDENT.findall(code[opening:index]))
                    break
    for macro, parameter in generated.items():
        for path, code in sources.items():
            for invocation in re.finditer(r"\b" + macro + r"\s*!\s*([({\[])", code):
                opening = invocation.end() - 1
                delimiter = code[opening]
                closing = {"(": ")", "{": "}", "[": "]"}[delimiter]
                depth = 1
                for index in range(opening + 1, len(code)):
                    depth += (code[index] == delimiter) - (code[index] == closing)
                    if depth == 0:
                        arguments = code[opening:index]
                        break
                else:
                    raise ValueError(f"unclosed {macro} invocation")
                # Public law generators use a repeated tuple catalogue whose
                # first identifier is the function-name parameter.
                declaration = re.search(r"\(\s*\$" + parameter + r":ident", " ".join(sources.values()))
                if declaration is None:
                    raise ValueError(f"unsupported public law generator {macro}")
                for name in re.findall(r"\(\s*(\w+)\s*,", arguments):
                    laws[name] = ((path, name), str(path.relative_to(root)))
                    bodies[(path, name)].update(IDENT.findall(macros[macro]))
    return laws, bodies, roots


def unmounted(root):
    laws, bodies, pending = inventory(root)
    by_name = defaultdict(list)
    for key in bodies:
        by_name[key[1]].append(key)
    reached = set()
    while pending:
        path, name = pending.pop()
        # A local helper shadows equally named helpers in other source files.
        # Keep bodies separate: merging them would turn an unreachable caller
        # in a library into a mount through an unrelated executable's helper.
        local = (path, name)
        candidates = [local] if local in bodies else by_name[name]
        for key in candidates:
            if key in reached:
                continue
            reached.add(key)
            pending.update((key[0], identifier) for identifier in bodies[key])
    return {name: location for name, (key, location) in laws.items() if key not in reached}, len(laws)


def main():
    missing, count = unmounted(ROOT)
    for name, location in sorted(missing.items()):
        print(f"{location}: unmounted public conformance function {name}", file=sys.stderr)
    print(f"conformance mounts: {count - len(missing)}/{count} public functions reachable from tests or executable helpers")
    return bool(missing)


if __name__ == "__main__":
    sys.exit(main())
