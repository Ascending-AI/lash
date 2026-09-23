#!/usr/bin/env python3
"""Fail when a registered conformance law left no execution receipt.

Registration is static: every ``(law, "label")`` row in a ``*_tests!``
catalogue under ``crates/lash-conformance/src/`` (``macros.rs`` holds most of
them; a suite may declare its catalogue beside its laws) expands into a
``#[tokio::test]`` in whichever backend test binary invokes the macro.  Until
FIG-3429 item 8, nothing observed the other side: a generated test can finish
green without the law ever running -- the fixture self-skips on a missing
service (the FIG-3414 Postgres shape, where ``let Some(..) = .. else { return
}`` reports ``ok`` for a law that never touched a database), or the claiming
job simply never runs the binary.

The receipts are the durable half of the fix: each generated test appends
``claimant<TAB>law<TAB>label`` to ``$LASH_LAW_RECEIPTS`` (Cargo/nextest legs)
or to ``$TEST_UNDECLARED_OUTPUTS_DIR/law-receipts.txt`` (Bazel legs, collected
into ``bazel-testlogs/<pkg>/<target>/test.outputs/``).  The claimant is the
module path at the invocation site (``module_path!()``), whose first segment
is the test binary's crate name -- so ``mod native`` and ``mod sqlite``
invocations of one suite in one binary are separate obligations, and a receipt
from one can never satisfy the other (FIG-3472).

This census is the other half: it walks each claimed crate's module tree,
records every ``*_tests!(`` invocation with the claimant it will expand under,
and compares the multiset of registered laws per claimant against the
receipts, exactly:

* a registered law with fewer receipts than invocations is missing;
* a receipt naming a (law, label) the claimant does not owe is a bug in the
  receipt -- stale, fabricated, or emitted under the wrong claimant;
* more receipts than invocations is a duplicate;
* a claimant with receipts but no expectation fails too.

``#[ignore]``d invocations are deferred laws, not exemptions: every ignored
invocation must be named by ``scripts/deferred-law-invocations.toml`` with the
recipe, CI job, and receipt artifact that owns its execution, and every
manifest entry must name a real ignored invocation (a stale entry fails the
same way a missing one does).  ``--deferred <recipe>`` censuses a deferred
lane's receipts against the manifest's entries for that recipe.

Claims are passed explicitly so each CI job asserts exactly the coverage it
executes:

* ``--crate <dir>``: every ``*_tests!`` invocation anywhere under the crate.
  The ``cargo test -p`` legs run every target in the package, so the claim is
  the whole crate.  Claims never union: expectations stay per claimant, and a
  repeated ``--crate`` does not double-count an invocation site.
* ``--test-file <file>``: invocations in one test-root file plus its
  ``#[path]``/``mod`` includes -- the per-binary claim.
* ``--labels <file> --crate-root <dir>``: a Bazel label file such as
  ``tools/bazel/postgres_test_labels.txt``, resolved to test-root files the
  same way the generator names them.
* ``--suite <name>``: every live invocation of that ``*_tests!`` macro across
  all crates under ``crates/``, ``examples/``, and ``runbooks/``, per
  claimant.
* ``--deferred <recipe>``: the deferred manifest's entries for one recipe.
* ``--bazel-testlogs <dir>``: self-describing per-target mode for Bazel legs.
  Every test target that ran is discovered under
  ``<dir>/crates/<pkg>/<target>/``; each law-bearing target must have left a
  complete receipt set in its own ``test.outputs``.  This catches
  fixture-skips inside a binary that ran; it cannot see a binary the job never
  ran, which is what the explicit claims above are for.
"""

from __future__ import annotations

import argparse
import ast
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
import re
import sys
import tomllib
import zipfile


ROOT = Path(__file__).resolve().parents[1]
MACROS = ROOT / "crates/lash-conformance/src/macros.rs"
CONFORMANCE_SRC = ROOT / "crates/lash-conformance/src"
WORKSPACE_TARGETS = ROOT / "tools/bazel/workspace_targets.bzl"
DEFERRED_MANIFEST = ROOT / "scripts/deferred-law-invocations.toml"
RECEIPT_NAME = "law-receipts.txt"

CATALOGUE_ROW = re.compile(r"\(\s*([a-z_][a-z0-9_]*)\s*,\s*\"([^\"]*)\"")
SUITE_CALL = re.compile(r"\b([a-z_][a-z0-9_]*_tests)\s*!")
SUITE_DEFINE = re.compile(r"macro_rules!\s+([a-z_][a-z0-9_]*_tests)\b")
DELEGATE_CALL = re.compile(r"\b([a-z_][a-z0-9_]*_tests)\s*!\s*\(\s*@([a-z_]+)")
LINE_COMMENT = re.compile(r"//[^\n]*")
BLOCK_COMMENT = re.compile(r"/\*.*?\*/", re.DOTALL)
TEST_TARGET = re.compile(r'name\s*=\s*"([^"]+)"')

IGNORE_HEAD = re.compile(r"\s*\(\s*#\s*\[\s*ignore\b")

# One module-tree token: a `#[path = "..."]` attribute (remembered for the
# next `mod` declaration), a `mod x;` / `mod x {` declaration, an
# `include!("...")` textual include, or a `*_tests!(` invocation.
MODULE_TOKEN = re.compile(
    r"#\[\s*path\s*=\s*\"(?P<path>[^\"]+)\"\s*\]"
    r"|(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(?P<mod>[a-zA-Z_][a-zA-Z0-9_]*)\s*(?P<term>[;{])"
    r"|include!\s*\(\s*\"(?P<include>[^\"]+)\"\s*\)"
    r"|(?P<suite>\b[a-z_][a-z0-9_]*_tests\s*!)"
)


@dataclass
class Macro:
    name: str
    # (arm pattern head, arm body text) -- a list, since two arms may share a head
    arms: list[tuple[str, str]] = field(default_factory=list)


@dataclass(frozen=True)
class Invocation:
    """One ``*_tests!(`` call site, under the claimant it expands for."""

    claimant: str
    suite: str
    ignored: bool
    file: Path
    line: int


def split_top_level(text: str) -> list[str]:
    """Split a macro_rules! body into its top-level arms: ``(pat) => {body};``."""
    arms: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        if text[i] != "(":
            i += 1
            continue
        # match the pattern group
        depth = 0
        j = i
        while j < n:
            c = text[j]
            if c in "([":
                depth += 1
            elif c in ")]":
                depth -= 1
                if depth == 0:
                    break
            elif c == "{":
                # brace inside a pattern (e.g. $fixture:block uses parens;
                # braces only appear in => bodies) -- treat as opaque depth
                depth += 1
            elif c == "}":
                depth -= 1
            j += 1
        if j >= n:
            break
        m = re.match(r"\s*=>\s*\{", text[j + 1 :])
        if not m:
            i = j + 1
            continue
        body_start = j + 1 + m.end() - 1  # at the '{'
        depth = 0
        k = body_start
        while k < n:
            if text[k] == "{":
                depth += 1
            elif text[k] == "}":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        if k >= n:
            break
        arms.append(text[i : k + 1])
        i = k + 1
    return arms


def macro_blocks(text: str) -> dict[str, Macro]:
    """Parse every ``macro_rules!`` definition into name -> {arm head: rows}."""
    macros: dict[str, Macro] = {}
    for m in re.finditer(r"macro_rules!\s+([a-z_][a-z0-9_]*)\s*\{", text):
        name = m.group(1)
        depth = 1
        i = m.end()
        while i < len(text) and depth:
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
            i += 1
        body = text[m.end() : i - 1]
        macro = Macro(name)
        for arm in split_top_level(body):
            head_end = arm.index("=>")
            macro.arms.append((arm[:head_end].strip(), arm[head_end + 2 :]))
        macros[name] = macro
    return macros


def load_macros() -> dict[str, Macro]:
    """Every ``*_tests!`` catalogue definition the census owes against.

    ``macros.rs`` first, then each other source under the conformance crate
    that defines a suite macro, in path order. A catalogue declared beside its
    laws, or split out to keep ``macros.rs`` inside its size budget, is as much
    a registration as one in ``macros.rs``; a hand-kept file list silently
    rejects every receipt of a suite it omits as misclaimed.
    """
    texts = [MACROS.read_text(encoding="utf-8")]
    for path in sorted(CONFORMANCE_SRC.rglob("*.rs")):
        if path == MACROS:
            continue
        text = path.read_text(encoding="utf-8")
        if SUITE_DEFINE.search(text):
            texts.append(text)
    return macro_blocks("\n".join(texts))


def arm_rows(arm_body: str) -> set[tuple[str, str]]:
    """The (law, label) catalogue rows in one arm body."""
    return set(CATALOGUE_ROW.findall(arm_body))


def registered_pairs(macros: dict[str, Macro]) -> dict[tuple[str, str], set[str]]:
    """(law, label) -> suite macro names that register it."""
    pairs: dict[tuple[str, str], set[str]] = {}
    for name, macro in macros.items():
        if not name.endswith("_tests"):
            continue
        for _, arm_body in macro.arms:
            for pair in arm_rows(arm_body):
                pairs.setdefault(pair, set()).add(name)
    return pairs


def suite_expected(macros: dict[str, Macro], name: str) -> set[tuple[str, str]]:
    """Every law a ``X_tests!`` invocation registers, following @delegations.

    ``runtime_persistence_reopenable_tests!`` does not list the shared
    catalogue itself; it calls ``runtime_persistence_tests!(@catalogue ..)``.
    Rows inline in a delegate call (helper-register macros) are already in the
    caller's own arms, so only named ``@arm`` delegation needs chasing.
    """
    expected: set[tuple[str, str]] = set()
    macro = macros.get(name)
    if macro is None:
        return expected
    for _, arm_body in macro.arms:
        expected |= arm_rows(arm_body)
    for head, arm_body in macro.arms:
        for target, arm_name in DELEGATE_CALL.findall(arm_body):
            if target == name:
                continue
            target_macro = macros.get(target)
            if target_macro is None:
                continue
            for t_head, t_body in target_macro.arms:
                if f"@{arm_name}" in t_head:
                    expected |= arm_rows(t_body)
    return expected


def strip_comments(text: str) -> str:
    """Remove ``//`` and ``/* */`` comments so negations and docs don't claim."""
    return LINE_COMMENT.sub("", BLOCK_COMMENT.sub("", text))


def brace_match(text: str, open_index: int) -> int:
    """The index of the ``}`` closing the ``{`` at ``open_index``."""
    depth = 0
    for i in range(open_index, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return i
    return len(text)


def resolve_mod_file(declaring: Path, name: str, path_attr: str | None) -> Path | None:
    """The file a ``mod name;`` in ``declaring`` refers to, or None.

    From ``lib.rs``/``main.rs``/``mod.rs`` the module lives at
    ``<dir>/<name>.rs`` or ``<dir>/<name>/mod.rs``; from any other file it
    lives under ``<dir>/<stem>/`` instead.  ``#[path = "..."]`` overrides the
    file name under the same base directory.
    """
    if path_attr is not None:
        # `#[path]` is relative to the declaring file's own directory even
        # when that file is not lib.rs/main.rs/mod.rs (unlike plain `mod`).
        candidate = declaring.parent / path_attr
        return candidate if candidate.is_file() else None
    if declaring.name in ("lib.rs", "main.rs", "mod.rs"):
        base = declaring.parent
    else:
        base = declaring.parent / declaring.stem
    for candidate in (base / f"{name}.rs", base / name / "mod.rs"):
        if candidate.is_file():
            return candidate
    return None


def _scan_module_text(
    text: str,
    prefix: list[str],
    file: Path,
    out: list[Invocation],
    stack: list[tuple[Path, list[str]]],
    line_offset: int = 0,
) -> None:
    """One file (or inline-module body) of the module walk.

    ``prefix`` is the module path the text expands under -- its first segment
    is the crate name, which is what ``module_path!()`` (and therefore the
    receipt's claimant column) reports.  ``mod x;`` declarations push their
    resolved file onto ``stack``; inline ``mod x { ... }`` bodies are scanned
    recursively with the path extended.
    """
    defined = set(SUITE_DEFINE.findall(text))
    pending_path: str | None = None
    pos = 0
    while True:
        m = MODULE_TOKEN.search(text, pos)
        if m is None:
            return
        if m.group("path") is not None:
            pending_path = m.group("path")
            pos = m.end()
            continue
        if m.group("include") is not None:
            # Textual include: the file's items land in *this* module, so its
            # invocations carry this prefix, not a child path.
            stack.append((file.parent / m.group("include"), prefix))
            pos = m.end()
            continue
        if m.group("mod") is not None:
            name = m.group("mod")
            if m.group("term") == ";":
                target = resolve_mod_file(file, name, pending_path)
                if target is not None:
                    stack.append((target, prefix + [name]))
                pos = m.end()
            else:
                open_index = m.end() - 1
                close = brace_match(text, open_index)
                _scan_module_text(
                    text[open_index + 1 : close],
                    prefix + [name],
                    file,
                    out,
                    stack,
                    line_offset + text.count("\n", 0, open_index + 1),
                )
                pos = close + 1
            pending_path = None
            continue
        suite = m.group("suite")
        suite_name = suite[: suite.index("!")].strip()
        if suite_name not in defined:
            out.append(
                Invocation(
                    claimant="::".join(prefix),
                    suite=suite_name,
                    ignored=bool(IGNORE_HEAD.match(text, m.end())),
                    file=file,
                    line=line_offset + text.count("\n", 0, m.start()) + 1,
                )
            )
        pos = m.end()


def invocations_in_root(root_file: Path, prefix: str) -> list[Invocation]:
    """Every ``*_tests!`` invocation a crate root's module tree reaches."""
    out: list[Invocation] = []
    stack: list[tuple[Path, list[str]]] = [(root_file, [prefix])]
    visited: set[tuple[Path, tuple[str, ...]]] = set()
    while stack:
        path, prefix_parts = stack.pop()
        resolved = path.resolve()
        key = (resolved, tuple(prefix_parts))
        if key in visited or not resolved.is_file():
            continue
        visited.add(key)
        _scan_module_text(
            strip_comments(resolved.read_text(encoding="utf-8")),
            prefix_parts,
            resolved,
            out,
            stack,
        )
    return out


def crate_manifest(crate: Path) -> dict:
    manifest_path = crate / "Cargo.toml"
    if not manifest_path.is_file():
        return {}
    return tomllib.loads(manifest_path.read_text(encoding="utf-8"))


def crate_roots(crate: Path) -> list[tuple[str, Path]]:
    """(claimant prefix, root file) for every compilation root of a package.

    The first segment of ``module_path!()`` is the crate name: the ``[lib]``
    name (or the package name with ``-`` -> ``_``) for ``src/lib.rs``, the bin
    name for ``src/main.rs``/``[[bin]]``, the file stem for ``tests/*.rs``,
    and the declared ``name`` for ``[[test]]``.
    """
    manifest = crate_manifest(crate)
    package = manifest.get("package", {}).get("name", crate.name)
    roots: list[tuple[str, Path]] = []

    lib = manifest.get("lib", {})
    lib_path = crate / lib.get("path", "src/lib.rs")
    if lib_path.is_file():
        roots.append((lib.get("name", package.replace("-", "_")), lib_path))

    bin_paths: set[Path] = set()
    for section in manifest.get("bin", []):
        name = section.get("name")
        path = crate / section.get("path", f"src/bin/{name}.rs")
        if name and path.is_file():
            roots.append((name, path))
            bin_paths.add(path.resolve())
    main = crate / "src/main.rs"
    if main.is_file() and main.resolve() not in bin_paths:
        roots.append((package.replace("-", "_"), main))

    test_paths: set[Path] = set()
    for section in manifest.get("test", []):
        name = section.get("name")
        path = crate / section.get("path", f"tests/{name}.rs")
        if name and path.is_file():
            roots.append((name, path))
            test_paths.add(path.resolve())
    tests_dir = crate / "tests"
    if tests_dir.is_dir():
        for path in sorted(tests_dir.glob("*.rs")):
            if path.resolve() not in test_paths:
                roots.append((path.stem, path))
    return roots


def invocations_in_crate(crate: Path) -> list[Invocation]:
    out: list[Invocation] = []
    for prefix, root in crate_roots(crate):
        out.extend(invocations_in_root(root, prefix))
    return out


def root_prefix(crate: Path, root_file: Path) -> str:
    """The claimant prefix a claim file expands under inside ``crate``."""
    manifest = crate_manifest(crate)
    package = manifest.get("package", {}).get("name", crate.name)
    resolved = root_file.resolve()
    lib = manifest.get("lib", {})
    if resolved == (crate / lib.get("path", "src/lib.rs")).resolve():
        return lib.get("name", package.replace("-", "_"))
    for section in manifest.get("bin", []):
        if resolved == (crate / section.get("path", f"src/bin/{section.get('name')}.rs")).resolve():
            return section.get("name", root_file.stem)
    if resolved == (crate / "src/main.rs").resolve():
        return package.replace("-", "_")
    for section in manifest.get("test", []):
        if resolved == (crate / section.get("path", f"tests/{section.get('name')}.rs")).resolve():
            return section.get("name", root_file.stem)
    return root_file.stem


def cargo_test_paths(crate_root: Path) -> dict[str, Path]:
    """Bazel ``<name>__test`` target -> its Cargo test-root file."""
    mapping: dict[str, Path] = {}
    cargo_toml = crate_root / "Cargo.toml"
    if cargo_toml.is_file():
        manifest = crate_manifest(crate_root)
        for section in manifest.get("test", []):
            name = section.get("name")
            path = section.get("path")
            if name and path:
                mapping[name] = crate_root / path
    tests_dir = crate_root / "tests"
    if tests_dir.is_dir():
        for path in sorted(tests_dir.glob("*.rs")):
            mapping.setdefault(path.stem, path)
    return mapping


def resolve_bazel_label(crate_root: Path, target: str) -> list[tuple[str, Path]]:
    """A ``<name>__test`` or ``<pkg>__unit_test`` label to (claimant, root)s."""
    if target.endswith("__unit_test"):
        manifest = crate_manifest(crate_root)
        unit_roots = {
            (
                crate_root
                / manifest.get("lib", {}).get("path", "src/lib.rs")
            ).resolve(),
            (crate_root / "src/main.rs").resolve(),
        }
        for section in manifest.get("bin", []):
            unit_roots.add(
                (
                    crate_root
                    / section.get("path", f"src/bin/{section.get('name')}.rs")
                ).resolve()
            )
        return [
            (prefix, root)
            for prefix, root in crate_roots(crate_root)
            if root.resolve() in unit_roots
        ]
    name = target[: -len("__test")] if target.endswith("__test") else target
    root = cargo_test_paths(crate_root).get(name)
    return [(name, root)] if root else []


def workspace_test_batches() -> dict[str, list[str]]:
    """The generated ``WORKSPACE_TEST_BATCHES``: batch label -> member labels.

    A ``lash_batch_test`` target runs a package's plain test binaries inside
    one test action, so its testlogs entry carries the union of every member's
    receipts under the batch's own name -- and resolves to no test root of its
    own. The generated mapping is the only complete list of members; a batch
    the mapping does not name cannot be censused at all.
    """
    match = re.search(
        r"^WORKSPACE_TEST_BATCHES = (\{.*?^\})$",
        WORKSPACE_TARGETS.read_text(encoding="utf-8"),
        flags=re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError("missing generated dict WORKSPACE_TEST_BATCHES")
    return ast.literal_eval(match.group(1))


def resolve_label(label: str) -> list[tuple[str, Path]]:
    """A ``//package:target`` label to its (claimant prefix, test root)s.

    Batch members are not confined to ``crates/`` -- the generated mapping
    carries ``runbooks/`` and ``examples/`` packages too -- so resolution is
    by label, not by directory convention.
    """
    package, _, target = label.partition(":")
    if not package.startswith("//") or not target:
        return []
    return resolve_bazel_label(ROOT / package.removeprefix("//"), target)


def deferred_manifest() -> list[dict[str, str]]:
    """The checked deferred-law manifest: one ``[[deferred]]`` row per
    ``#[ignore]``d invocation, naming the recipe and CI lane that owns it."""
    if not DEFERRED_MANIFEST.is_file():
        return []
    data = tomllib.loads(DEFERRED_MANIFEST.read_text(encoding="utf-8"))
    return list(data.get("deferred", []))


PARKED_RECIPE = "parked"
TICKET = re.compile(r"FIG-[0-9]+")


def parked_entry_errors(key: tuple[str, str, str], entry: dict[str, str]) -> list[str]:
    """A parked entry runs nowhere, so it must say why and which ticket
    brings it back: ``ticket`` names a ``FIG-n`` and ``reason`` is non-empty.
    Only a parked entry may carry a ticket, so a live recipe cannot hide one."""
    parked = entry.get("recipe") == PARKED_RECIPE
    if not parked:
        if "ticket" in entry or "reason" in entry:
            return [
                f"deferred-law manifest entry {key} carries a ticket or reason "
                f"but is not parked (recipe `{entry.get('recipe')}`)"
            ]
        return []
    errors = []
    if not TICKET.fullmatch(entry.get("ticket", "")):
        errors.append(
            f"parked deferred-law manifest entry {key} must name its ticket as `FIG-n`"
        )
    if not entry.get("reason", "").strip():
        errors.append(f"parked deferred-law manifest entry {key} must give a reason")
    return errors


def parked_skips(crate_name: str, macros: dict[str, Macro]) -> list[str]:
    """libtest arguments that skip every law of a parked invocation in
    ``crate_name``, one argument per line of output: a recipe that runs a
    crate's ignored tests passes them so a parked law runs nowhere."""
    arguments: list[str] = []
    for entry in deferred_manifest():
        if entry.get("recipe") != PARKED_RECIPE:
            continue
        crate, _, module = entry.get("claimant", "").partition("::")
        if crate != crate_name:
            continue
        for law, _ in sorted(suite_expected(macros, entry.get("suite", ""))):
            arguments.extend(["--skip", f"{module}::{law}" if module else law])
    return arguments


def manifest_check(errors: list[str]) -> dict[tuple[str, str, str], Invocation]:
    """Both directions of the deferred-law contract, always on.

    Returns ``(file, claimant, suite)`` -> the real ignored invocation each
    manifest entry names, for callers to match ignored invocations against
    and for ``--deferred`` to census the invocation at its true file and
    line.  Every entry must name a real ignored invocation: the entry's
    crate is walked and an ignored ``*_tests!(`` with the entry's claimant
    and suite must exist at the named file.
    """
    entries = deferred_manifest()
    matched: dict[tuple[str, str, str], Invocation] = {}
    crate_invocations: dict[Path, list[Invocation]] = {}
    for entry in entries:
        key = (entry.get("file", ""), entry.get("claimant", ""), entry.get("suite", ""))
        errors.extend(parked_entry_errors(key, entry))
        rel = Path(entry.get("file", ""))
        file = ROOT / rel
        if not file.is_file():
            errors.append(
                f"deferred-law manifest entry {key} names a file that does "
                "not exist -- stale entry"
            )
            continue
        crate = file.resolve().parent
        while crate != ROOT and not (crate / "Cargo.toml").is_file():
            crate = crate.parent
        if crate not in crate_invocations:
            crate_invocations[crate] = invocations_in_crate(crate)
        match = next(
            (
                inv
                for inv in crate_invocations[crate]
                if inv.ignored
                and inv.claimant == entry.get("claimant")
                and inv.suite == entry.get("suite")
                and inv.file == file.resolve()
            ),
            None,
        )
        if match is None:
            errors.append(
                f"deferred-law manifest entry {key} names no real "
                "#[ignore]d invocation -- stale entry"
            )
        else:
            matched[key] = match
    return matched


def check_ignored(
    invocations: list[Invocation],
    manifest_set: dict[tuple[str, str, str], Invocation],
    errors: list[str],
) -> None:
    """Every ignored invocation in claimed sources must be manifest-named."""
    for inv in invocations:
        if not inv.ignored:
            continue
        rel = inv.file.relative_to(ROOT).as_posix()
        if (rel, inv.claimant, inv.suite) not in manifest_set:
            errors.append(
                f"#[ignore]d invocation {inv.suite} at {rel}:{inv.line} "
                f"(claimant `{inv.claimant}`) is not in "
                "scripts/deferred-law-invocations.toml -- a deferred law "
                "needs a manifest entry naming the recipe that runs it"
            )


def deferred_invocations(
    recipe: str,
    manifest_index: dict[tuple[str, str, str], Invocation],
) -> tuple[list[Invocation], str | None]:
    """The live expectation a ``--deferred <recipe>`` claim asserts.

    Each manifest entry resolves to the real ignored invocation
    ``manifest_check`` already matched -- its true file and line, not a
    synthetic ``line=0``, so two entries sharing a file and claimant stay
    two distinct obligations instead of deduping to the first.
    """
    entries = [e for e in deferred_manifest() if e.get("recipe") == recipe]
    if not entries:
        return [], f"deferred recipe `{recipe}` has no manifest entries"
    invocations: list[Invocation] = []
    for entry in entries:
        key = (
            entry.get("file", ""),
            entry.get("claimant", ""),
            entry.get("suite", ""),
        )
        inv = manifest_index.get(key)
        if inv is None:
            # manifest_check already reported the stale entry.
            continue
        invocations.append(
            Invocation(
                claimant=inv.claimant,
                suite=inv.suite,
                ignored=False,
                file=inv.file,
                line=inv.line,
            )
        )
    return invocations, None


def expected_from_invocations(
    invocations: list[Invocation],
    macros: dict[str, Macro],
) -> dict[str, Counter]:
    """claimant -> Counter[(law, label)] from the live invocations."""
    expected: dict[str, Counter] = {}
    seen: set[tuple[str, Path, int]] = set()
    for inv in invocations:
        if inv.ignored:
            continue
        key = (inv.claimant, inv.file, inv.line)
        if key in seen:
            continue
        seen.add(key)
        expected.setdefault(inv.claimant, Counter()).update(
            suite_expected(macros, inv.suite)
        )
    return expected


def read_receipts(paths: list[Path]) -> tuple[dict[str, Counter], list[str]]:
    """claimant -> Counter[(law, label)] from 3-column receipt lines."""
    observed: dict[str, Counter] = {}
    errors: list[str] = []
    for path in paths:
        for lineno, line in enumerate(
            path.read_text(encoding="utf-8").splitlines(), start=1
        ):
            if not line.strip():
                continue
            parts = line.split("\t")
            if len(parts) == 2:
                errors.append(
                    f"{path}:{lineno}: pre-FIG-3472 receipt format "
                    f"`{line}` -- receipts are now claimant<TAB>law<TAB>label; "
                    "re-run the tests with a current build"
                )
                continue
            if len(parts) != 3:
                errors.append(f"{path}:{lineno}: malformed receipt line `{line}`")
                continue
            claimant, law, label = parts
            observed.setdefault(claimant, Counter())[(law, label)] += 1
    return observed, errors


def bazel_receipts(target_dir: Path) -> tuple[dict[str, Counter], list[str]]:
    """Receipts a single Bazel test target left in its undeclared outputs."""
    outputs_dir = target_dir / "test.outputs"
    found: list[Path] = []
    for candidate in (
        outputs_dir / RECEIPT_NAME,
        outputs_dir / "outputs" / RECEIPT_NAME,
    ):
        if candidate.is_file():
            found.append(candidate)
    for zip_path in target_dir.glob("test.outputs/*.zip"):
        with zipfile.ZipFile(zip_path) as zf:
            for member in zf.namelist():
                if member.endswith(RECEIPT_NAME):
                    tmp = target_dir / "test.outputs" / "__census_receipts.txt"
                    tmp.write_bytes(zf.read(member))
                    found.append(tmp)
    return read_receipts(found)


def census_compare(
    expected: dict[str, Counter],
    observed: dict[str, Counter],
    where: str,
) -> list[str]:
    """The exact per-claimant multiset comparison."""
    errors: list[str] = []
    for claimant in sorted(expected):
        exp = expected[claimant]
        obs = observed.get(claimant, Counter())
        for (law, label), missing in sorted((exp - obs).items()):
            errors.append(
                f"{where}claimant `{claimant}`: registered law `{law}` "
                f"(label `{label}`) produced {exp[(law, label)] - missing} of "
                f"{exp[(law, label)]} execution receipts"
            )
        for (law, label), count in sorted(obs.items()):
            if (law, label) not in exp:
                errors.append(
                    f"{where}claimant `{claimant}`: receipt for `{law}` "
                    f"(label `{label}`) names a law this claimant does not "
                    "owe -- stale, fabricated, or misclaimed record"
                )
            elif count > exp[(law, label)]:
                errors.append(
                    f"{where}claimant `{claimant}`: receipt for `{law}` "
                    f"(label `{label}`) appeared {count} times but the law "
                    f"is owed {exp[(law, label)]} times -- duplicate record"
                )
    for claimant in sorted(set(observed) - set(expected)):
        errors.append(
            f"{where}receipts name claimant `{claimant}`, which no claim "
            "covers -- stale, fabricated, or misclaimed record"
        )
    return errors


def census_testlogs(
    testlogs_dir: Path,
    batches: dict[str, list[str]],
    macros: dict[str, Macro],
    manifest_set: dict[tuple[str, str, str], Invocation],
) -> tuple[list[str], int]:
    """The per-target census over one Bazel testlogs tree.

    A ran target is any directory carrying ``test.log`` or ``test.outputs``;
    its path relative to the root is its label's package and name, so
    discovery reaches nested packages (``runbooks/…``, ``examples/…``) the
    same way it reaches ``crates/…``. A target the generated
    ``WORKSPACE_TEST_BATCHES`` names as a batch is censused over the union of
    its members' roots -- the claimants stay per member binary. Every member
    label must resolve to a test root, and a ran target that left receipts it
    cannot account for is a failure, never a skip.
    """
    errors: list[str] = []
    verified = 0
    target_dirs = {
        marker.parent
        for marker in testlogs_dir.rglob("*")
        if marker.name in ("test.log", "test.outputs") and marker.parent.is_dir()
    }
    for target_dir in sorted(target_dirs):
        rel = target_dir.relative_to(testlogs_dir)
        if len(rel.parts) < 2:
            continue
        target_label = f"//{'/'.join(rel.parts[:-1])}:{rel.parts[-1]}"
        members = batches.get(target_label, [target_label])
        invocations: list[Invocation] = []
        unresolved: list[str] = []
        for member in members:
            roots = resolve_label(member)
            if roots:
                for prefix, root_file in roots:
                    invocations.extend(invocations_in_root(root_file, prefix))
            else:
                unresolved.append(member)
        t_observed, receipt_errors = bazel_receipts(target_dir)
        errors.extend(receipt_errors)
        check_ignored(invocations, manifest_set, errors)
        if unresolved and t_observed:
            errors.append(
                f"bazel target {target_label} ran and left receipts, but "
                + (
                    "no batch member"
                    if len(unresolved) == len(members)
                    else f"member(s) {', '.join(sorted(unresolved))}"
                )
                + " could be resolved to a test root -- the census cannot "
                "name the laws those receipts owe"
            )
        t_expected = expected_from_invocations(invocations, macros)
        verified += sum(sum(c.values()) for c in t_expected.values())
        errors.extend(
            census_compare(t_expected, t_observed, f"bazel target {target_label} ran but ")
        )
    return errors, verified


def workspace_package_dirs() -> list[Path]:
    """Every package directory under the claimable roots."""
    dirs: list[Path] = []
    for base_name in ("crates", "examples", "runbooks"):
        base = ROOT / base_name
        if not base.is_dir():
            continue
        for manifest in sorted(base.rglob("Cargo.toml")):
            dirs.append(manifest.parent)
    return dirs


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipts", action="append", default=[], metavar="FILE")
    parser.add_argument("--receipts-root", action="append", default=[], metavar="DIR")
    parser.add_argument("--crate", dest="crates", action="append", default=[], metavar="DIR")
    parser.add_argument("--test-file", action="append", default=[], metavar="FILE")
    parser.add_argument("--suite", action="append", default=[], metavar="NAME")
    parser.add_argument("--labels", action="append", default=[], metavar="FILE")
    parser.add_argument("--crate-root", metavar="DIR")
    parser.add_argument("--deferred", action="append", default=[], metavar="RECIPE")
    parser.add_argument("--bazel-testlogs", action="append", default=[], metavar="DIR")
    parser.add_argument("--parked-skips", metavar="CRATE")
    args = parser.parse_args()

    if args.parked_skips:
        print("\n".join(parked_skips(args.parked_skips, load_macros())))
        return 0

    macros = load_macros()

    errors: list[str] = []
    manifest_set = manifest_check(errors)

    receipts_paths: list[Path] = [Path(p) for p in args.receipts]
    for root in args.receipts_root:
        receipts_paths.extend(sorted(Path(root).rglob(RECEIPT_NAME)))
    for path in receipts_paths:
        if not path.is_file():
            # Not an error on its own: a claim with expected laws fails below
            # on the missing receipts either way, and a claim expecting none
            # (a crate whose suites arrived later) is exactly this case.
            print(
                f"law execution census: no receipts file at {path} "
                "(empty observation)",
                file=sys.stderr,
            )
    observed, receipt_errors = read_receipts(
        [p for p in receipts_paths if p.is_file()]
    )
    errors.extend(receipt_errors)

    invocations: list[Invocation] = []
    for crate in args.crates:
        invocations.extend(invocations_in_crate(ROOT / crate))
    for test_file in args.test_file:
        root_file = ROOT / test_file
        crate = root_file.resolve().parent
        while crate != ROOT and not (crate / "Cargo.toml").is_file():
            crate = crate.parent
        invocations.extend(
            invocations_in_root(root_file, root_prefix(crate, root_file))
        )
    for suite in args.suite:
        name = suite if suite.endswith("_tests") else f"{suite}_tests"
        found = False
        for crate_dir in workspace_package_dirs():
            crate_invs = invocations_in_crate(crate_dir)
            suite_invs = [inv for inv in crate_invs if inv.suite == name]
            if suite_invs:
                found = True
                invocations.extend(suite_invs)
        if not found:
            errors.append(f"suite {name} has no invocation under crates/, examples/, runbooks/")
    if args.labels:
        crate_root = Path(args.crate_root or ".")
        for labels_file in args.labels:
            for line in (ROOT / labels_file).read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line or ":" not in line:
                    continue
                target = line.rsplit(":", 1)[1]
                for prefix, root_file in resolve_bazel_label(crate_root, target):
                    invocations.extend(invocations_in_root(root_file, prefix))
    for recipe in args.deferred:
        deferred, deferred_error = deferred_invocations(recipe, manifest_set)
        if deferred_error is not None:
            errors.append(deferred_error)
        invocations.extend(deferred)

    check_ignored(invocations, manifest_set, errors)
    expected = expected_from_invocations(invocations, macros)
    errors.extend(census_compare(expected, observed, ""))

    batches = workspace_test_batches() if args.bazel_testlogs else {}
    bazel_verified = 0
    for testlogs in args.bazel_testlogs:
        testlogs_dir = Path(testlogs)
        if not testlogs_dir.is_dir():
            errors.append(f"{testlogs} is not a testlogs directory")
            continue
        target_errors, verified = census_testlogs(
            testlogs_dir, batches, macros, manifest_set
        )
        errors.extend(target_errors)
        bazel_verified += verified

    if errors:
        for error in errors:
            print(f"law execution census: {error}", file=sys.stderr)
        return 1
    total = sum(sum(c.values()) for c in expected.values()) + bazel_verified
    print(
        f"law execution census: receipts cover {total} claimed laws across "
        f"{len(expected)} claimants"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
