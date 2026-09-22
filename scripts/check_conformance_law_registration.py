#!/usr/bin/env python3
"""Prove every runtime-persistence law is registered somewhere a backend runs it.

``runtime_persistence_tests!`` and ``runtime_persistence_reopenable_tests!``
register laws by *naming* them: each ``(law_ident, "fixture-label")`` pair in
``crates/lash-conformance/src/macros.rs`` expands into a ``#[tokio::test]`` that
calls ``runtime_persistence_macro_support::$law``.  That naming is checked in
one direction already — a catalogue entry that names no law fails to compile.
The other direction is not: the law modules are wildcard-reexported into
``runtime_persistence_macro_support``, so a ``pub`` law added to one of them
but never listed in a catalogue compiles cleanly and simply never runs on any
backend.

This check closes that direction.  A top-level ``pub fn`` in a swept module is
reachable iff at least one of these names it:

* a ``(name, "label")`` pair anywhere in ``macros.rs`` (any suite's catalogue —
  the runtime-persistence modules also feed the registration and drain-wait
  catalogues),
* a ``pub use`` re-export (the modules' own ``mod.rs`` lists and
  ``conformance/mod.rs`` export laws for direct cross-crate callers), or
* any other call/reference site — helpers a law calls inside its own file are
  covered transitively, so they count as reachable.

A ``pub`` item matching none of the three is a law no backend ever runs, or a
helper that should not be public; either way the failure names it.

The ordinary/reopenable partition is also asserted here: the shared catalogue
runs on every backend (in-memory, sqlite, postgres), while ``@reopen_laws``
runs only on the durable backends.  A law listed in both would generate two
tests with the same name — the check reports that collision before rustc does.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
MACROS = "crates/lash-conformance/src/macros.rs"
# Catalogue files split from macros.rs to stay inside the line budget.
MACRO_MODULES = "crates/lash-conformance/src/macros"
SUPPORT_INDEX = (
    "crates/lash-conformance/src/conformance/runtime_persistence/mod.rs"
)

# A catalogue row is `(law_ident, "fixture-label")` possibly across one line.
CATALOGUE_ROW = re.compile(r"\(\s*([a-z_][a-z0-9_]*)\s*,\s*\"")
# Top-level public functions only: methods and module-scoped items are indented.
PUB_FN = re.compile(r"^pub (?:async )?fn ([a-z_][a-z0-9_]*)", re.MULTILINE)
WORD = re.compile(r"[a-z_][a-z0-9_]*")


@dataclass(frozen=True)
class Sweep:
    """The modules wildcard-reexported into ``runtime_persistence_macro_support``."""

    files: tuple[str, ...]


def swept_files(root: Path, support_index_text: str) -> Sweep:
    block = re.search(
        r"pub mod runtime_persistence_macro_support \{(.*?)\n\}",
        support_index_text,
        re.DOTALL,
    )
    assert block, (
        "runtime_persistence_macro_support module not found in "
        f"{SUPPORT_INDEX}; the registration surface moved"
    )
    files: list[str] = []
    for path in re.findall(r"pub use (super|crate::conformance)::([a-z_]+)::\*;", block.group(1)):
        scope, module = path
        if scope == "super":
            files.append(
                f"crates/lash-conformance/src/conformance/runtime_persistence/{module}.rs"
            )
        else:
            files.append(f"crates/lash-conformance/src/conformance/{module}.rs")
    for file in files:
        assert (root / file).is_file(), f"swept module {file} does not exist"
    return Sweep(tuple(files))


def catalogue_pairs(text: str, start: int, end: int) -> list[str]:
    return CATALOGUE_ROW.findall(text[start:end])


def runtime_persistence_lists(macros: str) -> tuple[list[str], list[str], list[str]]:
    """(shared catalogue, plain-only, reopen-only) law idents, in order."""
    cat_arm = macros.index("(@catalogue $mode:ident $fixture:block) => {")
    cat_end = macros.index("macro_rules! runtime_persistence_reopenable_tests")
    shared = catalogue_pairs(macros, cat_arm, cat_end)

    plain_end = cat_arm
    plain = catalogue_pairs(macros, macros.index("macro_rules! runtime_persistence_tests"), plain_end)

    reopen_arm = macros.index("runtime_persistence_reopenable_tests!(@reopen_laws $fixture;")
    reopen_end = macros.index("(@reopen_laws $fixture:block", reopen_arm)
    reopen = catalogue_pairs(macros, reopen_arm, reopen_end)
    return shared, plain, reopen


def collect_candidates(root: Path, sweep: Sweep) -> dict[str, str]:
    """law-name -> defining file for every top-level pub fn in the sweep."""
    candidates: dict[str, str] = {}
    for file in sweep.files:
        for name in PUB_FN.findall((root / file).read_text(encoding="utf-8")):
            candidates[name] = file
    return candidates


def corpus(root: Path) -> dict[str, str]:
    files: dict[str, str] = {}
    for base in ("crates", "runbooks", "examples"):
        for path in sorted((root / base).rglob("*.rs")) if (root / base).is_dir() else []:
            try:
                files[str(path.relative_to(root))] = path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
    return files


def referenced_elsewhere(name: str, defining_file: str, files: dict[str, str]) -> bool:
    needle = re.compile(rf"\b{re.escape(name)}\b")
    definition = re.compile(rf"^pub (?:async )?fn {re.escape(name)}\b")
    for file, text in files.items():
        if file == defining_file:
            # Same file: a hit on any line that is not the definition itself.
            for line in text.splitlines():
                if needle.search(line) and not definition.match(line):
                    return True
        elif needle.search(text):
            return True
    return False


def check(root: Path) -> list[str]:
    macros = "\n".join(
        source.read_text(encoding="utf-8")
        for source in [root / MACROS, *sorted((root / MACRO_MODULES).glob("*.rs"))]
    )
    support_index = (root / SUPPORT_INDEX).read_text(encoding="utf-8")
    sweep = swept_files(root, support_index)
    candidates = collect_candidates(root, sweep)
    shared, plain, reopen = runtime_persistence_lists(macros)
    registered = set(catalogue_pairs(macros, 0, len(macros)))
    files = corpus(root)

    errors: list[str] = []

    for label, laws in (("shared catalogue", shared), ("plain-only", plain), ("reopen-only", reopen)):
        for name in laws:
            if name not in candidates:
                errors.append(
                    f"{label} entry `{name}` has no matching pub fn in the "
                    "runtime-persistence macro-support modules"
                )

    overlap = (set(shared) | set(plain)) & set(reopen)
    for name in sorted(overlap):
        errors.append(
            f"`{name}` is in both the shared catalogue and @reopen_laws; the two "
            "macros would generate duplicate tests on the reopenable backends"
        )

    # Reverse: every pub fn on the surface must be registered or reachable.
    for name, file in sorted(candidates.items()):
        if name in registered:
            continue
        if referenced_elsewhere(name, file, files):
            continue
        errors.append(
            f"`{name}` ({file}) is a pub fn in a runtime-persistence "
            "macro-support module but is named in no catalogue and referenced "
            "nowhere else, so no backend ever runs it. Register it in the "
            "runtime_persistence_tests! catalogue (in-memory + sqlite + "
            "postgres) or in runtime_persistence_reopenable_tests!'s "
            "@reopen_laws (sqlite + postgres only), or narrow its visibility "
            "if it is a helper."
        )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args()
    errors = check(ROOT)
    if errors:
        for error in errors:
            print(f"conformance law registration: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
