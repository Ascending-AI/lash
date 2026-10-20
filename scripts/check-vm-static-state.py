#!/usr/bin/env python3
"""Keep guest-derived state out of statics in the VM crates (FIG-4158, ADR 0123).

Model code runs in a worker that is reset and reused across sessions. A reset
drops the one machine the worker hosts, so every piece of guest-derived state
has to live in that instance: anything a `static`,
`thread_local!`, `OnceLock`, `LazyLock` or `lazy_static!` holds survives the
reset and is shared by the next session.

This check refuses every static item in the kernel crates, the worker and
lash's side of the machine's host boundary unless `scripts/vm-static-state-allowlist.txt` lists it with a one-line reason.
The allowlist is for guest-free constants and build tables only: data no guest
input ever reaches, or opt-in diagnostics that hold no guest values. Test-only code is
outside this inventory. An entry that no longer matches a static fails the check too, so the
list stays exactly the reviewed set.

Allowlist lines are `<path>::<scope>::<NAME>  # <reason>`, where `<scope>` is
the enclosing `fn` (or `<module>` for an item at module level). Every static
item form declares a `static NAME:` item, so that is what the check keys on:
`thread_local!` and `lazy_static!` bodies, `OnceLock`/`LazyLock` cells and
`static mut` all do.

Test-only crates and items disabled by `cfg(test)` or the `testing` feature
are excluded. External modules are followed from production crate roots, so
test modules are excluded by reachability, not their directory names.

Only `src/` is checked: a crate's `tests/`, `examples/` and `benches/` trees
build separate binaries that never link into a worker.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import os
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
ALLOWLIST = Path("scripts/vm-static-state-allowlist.txt")

# The crates whose code runs inside a VM worker: the kernel set (machine,
# library, dialects, extensions and the engines they fork), lash's side of
# the machine's host boundary and the shipped library both sides hold. A new kernel crate joins the list by its name,
# so its first static is reviewed like any other.
VM_CRATE_PATTERN = re.compile(r"^lash-(kernel|dialect|ext)-|^lash-regress$")
VM_CRATES = ("crates/lash-vm-runtime", "crates/lash-vm-releases")
WORKER_CRATE = "crates/lash-vm-worker"

CHAR_LITERAL = re.compile(r"'(?:[^'\\\n]|\\[^\n]{1,9}?)'")
# Search in C for the next comment or literal instead of visiting every byte
# of generated Rust in Python. The alternatives retain the lexer's precedence.
COMMENT_OR_LITERAL = re.compile(r'//|/\*|(?:b?r)(#*)"|' + CHAR_LITERAL.pattern + r'|"')
STATIC_ITEM = re.compile(r"(?<![\w'])static\s+(?:mut\s+|ref\s+)?([A-Za-z_]\w*)\s*:")


@dataclass(frozen=True)
class StaticItem:
    path: str
    scope: str
    name: str
    line: int

    @property
    def key(self) -> str:
        return f"{self.path}::{self.scope}::{self.name}"


def checked_crates(root: Path) -> list[str]:
    crates = list(VM_CRATES)
    if (root / "crates").is_dir():
        crates.extend(
            f"crates/{path.name}"
            for path in sorted((root / "crates").iterdir())
            if path.is_dir() and VM_CRATE_PATTERN.match(path.name)
            # This unpublished corpus/harness crate is a development dependency
            # only; it is never linked into a worker.
            and path.name != "lash-kernel-conformance"
        )
    if (root / WORKER_CRATE).is_dir():
        crates.append(WORKER_CRATE)
    return crates


def strip_comments(text: str) -> list[str]:
    """Blank comments and literals, preserving every source offset and line."""
    out = list(text)
    index = 0
    while match := COMMENT_OR_LITERAL.search(text, index):
        start = match.start()
        token = match.group()
        index = match.end()
        if token == "//":
            end = text.find("\n", index)
            index = len(text) if end < 0 else end
        elif token == "/*":
            depth = 1
            while index < len(text) and depth:
                if text.startswith("/*", index):
                    depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
        elif match.group(1) is not None:
            closing = '"' + match.group(1)
            end = text.find(closing, match.end())
            index = len(text) if end < 0 else end + len(closing)
        elif token == '"':
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                elif text[index] == '"':
                    index += 1
                    break
                else:
                    index += 1
        for at in range(start, min(index, len(text))):
            if out[at] != "\n":
                out[at] = " "
    return "".join(out).splitlines()


def cfg_in_production(expression: str) -> bool | None:
    """Three-valued cfg evaluation: unknown shipping features remain possible."""
    expression = expression.strip()
    if expression == "test" or re.fullmatch(r'feature\s*=\s*"testing"', expression):
        return False
    if match := re.fullmatch(r"(any|all|not)\s*\((.*)\)", expression, re.S):
        operator, contents = match.groups()
        depth = 0
        start = 0
        terms = []
        for at, char in enumerate(contents + ","):
            if char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
            elif char == "," and depth == 0:
                if contents[start:at].strip():
                    terms.append(cfg_in_production(contents[start:at]))
                start = at + 1
        if operator == "not":
            return None if len(terms) != 1 or terms[0] is None else not terms[0]
        if operator == "all":
            return False if False in terms else (None if None in terms else True)
        return True if True in terms else (None if None in terms else False)
    return None


def production(text: str) -> str:
    """Blank test-only items, retaining line numbers and shipping cfg branches."""
    clean = "\n".join(strip_comments(text))
    # splitlines omits a terminal newline; add it for offset fidelity.
    if text.endswith("\n"):
        clean += "\n"
    out = list(text)
    for match in re.finditer(r"#\s*\[\s*cfg\s*\(", clean):
        close = clean.find("]", match.end())
        expression = text[match.end():close].rstrip().removesuffix(")")
        if close < 0 or cfg_in_production(expression) is not False:
            continue
        depth = 0
        opened = False
        end = close + 1
        for end in range(close + 1, len(clean)):
            char = clean[end]
            if char == "{":
                depth += 1
                opened = True
            elif char == "}":
                depth -= 1
            elif char == ";" and not opened:
                break
            if opened and depth == 0:
                break
        for at in range(match.start(), end + 1):
            if out[at] != "\n":
                out[at] = " "
    return "".join(out)


def module_declarations(text: str) -> list[tuple[str | None, str]]:
    clean = "\n".join(strip_comments(text))
    declarations = []
    depth = 0
    inline = []
    pending = None
    modules = list(re.finditer(r"\bmod\s+(\w+)\s*([;{])", clean))
    events = [(match.start(), "mod", match) for match in modules]
    events += [(at, char, None) for at, char in enumerate(clean) if char in "{}"]
    for at, kind, match in sorted(events):
        if kind == "mod":
            name, delimiter = match.groups()
            if delimiter == "{":
                pending = name
                continue
            prefix = text[:at]
            attribute = re.search(r'#\s*\[\s*path\s*=\s*"([^"\n]+)"\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?$', prefix)
            directory = "/".join(name for _, name in inline)
            explicit = attribute.group(1) if attribute else None
            if explicit and directory:
                explicit = f"{directory}/{explicit}"
            name = f"{directory}/{name}" if directory else name
            declarations.append((explicit, name))
        elif kind == "{":
            depth += 1
            if pending is not None:
                inline.append((depth, pending))
                pending = None
        else:
            if inline and inline[-1][0] == depth:
                inline.pop()
            depth -= 1
    return declarations


def production_sources(crate: Path) -> list[Path]:
    """Follow production module declarations, including explicit path modules."""
    pending = [crate / "src" / name for name in ("lib.rs", "main.rs")]
    manifest = crate / "Cargo.toml"
    if manifest.is_file():
        for binary in tomllib.loads(manifest.read_text()).get("bin", []):
            if "testing" not in binary.get("required-features", []):
                pending.append(crate / binary.get("path", f"src/bin/{binary['name']}.rs"))
    seen = set()
    while pending:
        path = Path(os.path.normpath(pending.pop().absolute()))
        if path in seen or not path.is_file():
            continue
        seen.add(path)
        base = path.parent if path.name in {"lib.rs", "main.rs", "mod.rs"} else path.with_suffix("")
        for explicit, name in module_declarations(production(path.read_text())):
            if explicit:
                pending.append(path.parent / explicit)
            else:
                pending.extend([base / f"{name}.rs", base / name / "mod.rs"])
    return sorted(seen)


FN_NAME = re.compile(r"(?<![\w])fn\s+(\w+)")


def statics_in(root: Path, path: Path) -> list[StaticItem]:
    relative = path.relative_to(root).as_posix()
    items = []
    # The enclosing `fn` of each item: a `fn` opens its scope at the first `{`
    # after its name and closes it at the matching `}`; a bodiless `fn ...;`
    # (a trait method) opens nothing.
    depth = 0
    pending: str | None = None
    scopes: list[tuple[int, str]] = []
    for number, line in enumerate(strip_comments(production(path.read_text())), start=1):
        line = CHAR_LITERAL.sub("   ", line)
        for match in STATIC_ITEM.finditer(line):
            scope = scopes[-1][1] if scopes else "<module>"
            items.append(StaticItem(relative, scope, match.group(1), number))
        events = [(m.start(), "fn", m.group(1)) for m in FN_NAME.finditer(line)]
        events += [(i, c, "") for i, c in enumerate(line) if c in "{};"]
        for _, kind, name in sorted(events):
            if kind == "fn":
                pending = name
            elif kind == "{":
                depth += 1
                if pending is not None:
                    scopes.append((depth, pending))
                    pending = None
            elif kind == "}":
                if scopes and scopes[-1][0] == depth:
                    scopes.pop()
                depth -= 1
            elif kind == ";":
                pending = None
    return items


def collect(root: Path) -> list[StaticItem]:
    items = []
    for crate in checked_crates(root):
        source = root / crate / "src"
        if not source.is_dir():
            continue
        for path in production_sources(root / crate):
            items.extend(statics_in(root, path))
    return items


def load_allowlist(root: Path) -> tuple[dict[str, str], list[str]]:
    """`{key: reason}` and the malformed lines; a line without a reason is one."""
    entries: dict[str, str] = {}
    problems = []
    path = root / ALLOWLIST
    for number, raw in enumerate(path.read_text().splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        body, _, reason = line.partition("#")
        fields = body.split()
        if len(fields) != 1 or fields[0].count("::") != 2 or not reason.strip():
            problems.append(f"{ALLOWLIST}:{number}: want `<path>::<scope>::<NAME>  # <reason>`")
            continue
        if fields[0] in entries:
            problems.append(f"{ALLOWLIST}:{number}: `{fields[0]}` is listed twice")
            continue
        entries[fields[0]] = reason.strip()
    return entries, problems


def check(root: Path) -> list[str]:
    allowlist, problems = load_allowlist(root)
    items = collect(root)
    seen = set()
    for item in items:
        seen.add(item.key)
        if item.key not in allowlist:
            problems.append(
                f"{item.path}:{item.line}: static `{item.name}` (in `{item.scope}`) is not on "
                f"{ALLOWLIST}; guest-derived state belongs in the hosted machine, and a guest-free "
                "constant needs an allowlist entry with its reason"
            )
    for key in sorted(set(allowlist) - seen):
        problems.append(f"{ALLOWLIST}: `{key}` matches no static item; remove the stale entry")
    return problems


def main(argv: list[str]) -> int:
    root = Path(argv[1]).resolve() if len(argv) > 1 else ROOT
    problems = check(root)
    if problems:
        print("VM static-state check failed:", file=sys.stderr)
        print("\n".join(f"  {problem}" for problem in problems), file=sys.stderr)
        return 1
    print("VM static-state check passed: guest-derived state has an instance owner")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
