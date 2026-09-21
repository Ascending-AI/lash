#!/usr/bin/env python3
"""Fail when a registered conformance law left no execution receipt.

Registration is static: every ``(law, "label")`` row in a ``*_tests!``
catalogue in ``crates/lash-conformance/src/macros.rs`` expands into a
``#[tokio::test]`` in whichever backend test binary invokes the macro.  Until
FIG-3429 item 8, nothing observed the other side: a generated test can finish
green without the law ever running -- the fixture self-skips on a missing
service (the FIG-3414 Postgres shape, where ``let Some(..) = .. else { return
}`` reports ``ok`` for a law that never touched a database), or the claiming
job simply never runs the binary.

The receipts are the durable half of the fix: each generated test appends
``law<TAB>label`` to ``$LASH_LAW_RECEIPTS`` (Cargo/nextest legs) or to
``$TEST_UNDECLARED_OUTPUTS_DIR/law-receipts.txt`` (Bazel legs, collected into
``bazel-testlogs/<pkg>/<target>/test.outputs/``).  This census is the other
half: for each claimed unit it recomputes the registered law set from
``macros.rs`` and the crate's own ``*_tests!`` invocations, and fails on any
law that produced no receipt.  A receipt naming no registered law is also a
failure -- a stale or fabricated record is not evidence either.

Claims are passed explicitly so each CI job asserts exactly the coverage it
executes:

* ``--crate <dir>``: every ``*_tests!`` invocation anywhere under the crate.
  The ``cargo test -p`` legs run every target in the package, so the claim is
  the whole crate.
* ``--test-file <file>``: invocations in one test-root file plus its
  ``#[path]``/``mod`` includes -- the per-binary claim.
* ``--labels <file> --crate-root <dir>``: a Bazel label file such as
  ``tools/bazel/postgres_test_labels.txt``, resolved to test-root files the
  same way the generator names them.
* ``--suite <name>``: one ``*_tests!`` macro's full catalogue.
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
from dataclasses import dataclass, field
from pathlib import Path
import re
import sys
import zipfile


ROOT = Path(__file__).resolve().parents[1]
MACROS = ROOT / "crates/lash-conformance/src/macros.rs"
WORKSPACE_TARGETS = ROOT / "tools/bazel/workspace_targets.bzl"
RECEIPT_NAME = "law-receipts.txt"

CATALOGUE_ROW = re.compile(r"\(\s*([a-z_][a-z0-9_]*)\s*,\s*\"([^\"]*)\"")
SUITE_CALL = re.compile(r"\b([a-z_][a-z0-9_]*_tests)\s*!")
SUITE_DEFINE = re.compile(r"macro_rules!\s+([a-z_][a-z0-9_]*_tests)\b")
DELEGATE_CALL = re.compile(r"\b([a-z_][a-z0-9_]*_tests)\s*!\s*\(\s*@([a-z_]+)")
PATH_INCLUDE = re.compile(r"#\[\s*path\s*=\s*\"([^\"]+)\"\s*\]\s*(?:\n\s*)*mod\b")
MOD_INCLUDE = re.compile(r"^\s*(?:pub\s+)?mod\s+([a-z_][a-z0-9_]*)\s*;", re.MULTILINE)
LINE_COMMENT = re.compile(r"//[^\n]*")
TEST_TARGET = re.compile(r'name\s*=\s*"([^"]+)"')


@dataclass
class Macro:
    name: str
    # (arm pattern head, arm body text) -- a list, since two arms may share a head
    arms: list[tuple[str, str]] = field(default_factory=list)


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
    """Remove ``//`` line comments so `// No foo_tests!:` notes don't claim."""
    return LINE_COMMENT.sub("", text)


def source_files(root_file: Path) -> list[Path]:
    """A test-root file plus its ``#[path]`` and ``mod`` includes, recursively."""
    seen: list[Path] = []
    stack = [root_file]
    visited: set[Path] = set()
    while stack:
        path = stack.pop()
        path = path.resolve()
        if path in visited or not path.is_file():
            continue
        visited.add(path)
        seen.append(path)
        text = strip_comments(path.read_text(encoding="utf-8"))
        for inc in PATH_INCLUDE.findall(text):
            stack.append(path.parent / inc)
        for mod in MOD_INCLUDE.findall(text):
            stack.append(path.parent / f"{mod}.rs")
            stack.append(path.parent / mod / "mod.rs")
    return seen


def invoked_suites(text: str) -> set[str]:
    return set(SUITE_CALL.findall(strip_comments(text))) - set(
        SUITE_DEFINE.findall(text)
    )


IGNORE_HEAD = re.compile(r"\s*\(\s*#\s*\[\s*ignore\b")


def live_invoked_suites(text: str) -> set[str]:
    """Invoked suites minus those whose every invocation is ``#[ignore]``d.

    An ``#[ignore]`` attribute passed at the call site lands on every test the
    suite generates: the laws stay registered but are deferred to the lane the
    ignore reason names, so this file's target owes no receipt for them. A
    suite invoked once ignored and once live still owes its receipts.
    """
    stripped = strip_comments(text)
    defined = set(SUITE_DEFINE.findall(stripped))
    live: dict[str, bool] = {}
    for m in SUITE_CALL.finditer(stripped):
        name = m.group(1)
        if name in defined:
            continue
        ignored = bool(IGNORE_HEAD.match(stripped, m.end()))
        live[name] = live.get(name, False) or not ignored
    return {name for name, is_live in live.items() if is_live}


def expected_for_files(files: list[Path], macros: dict[str, Macro]) -> set[tuple[str, str]]:
    expected: set[tuple[str, str]] = set()
    for path in files:
        for suite in live_invoked_suites(path.read_text(encoding="utf-8")):
            expected |= suite_expected(macros, suite)
    return expected


def expected_for_crate(crate: Path, macros: dict[str, Macro]) -> set[tuple[str, str]]:
    expected: set[tuple[str, str]] = set()
    for path in sorted(crate.rglob("*.rs")):
        expected |= expected_for_files([path], macros)
    return expected


def cargo_test_paths(crate_root: Path) -> dict[str, Path]:
    """Bazel ``<name>__test`` target -> its Cargo test-root file."""
    mapping: dict[str, Path] = {}
    cargo_toml = crate_root / "Cargo.toml"
    if cargo_toml.is_file():
        text = cargo_toml.read_text(encoding="utf-8")
        for section in re.findall(r"\[\[test\]\](.*?)(?=\n\[|\Z)", text, re.DOTALL):
            name = TEST_TARGET.search(section)
            path = re.search(r'path\s*=\s*"([^"]+)"', section)
            if name and path:
                mapping[name.group(1)] = crate_root / path.group(1)
    for path in sorted((crate_root / "tests").glob("*.rs")) if (crate_root / "tests").is_dir() else []:
        mapping.setdefault(path.stem, path)
    return mapping


def resolve_bazel_label(crate_root: Path, target: str) -> list[Path]:
    """A ``<name>__test`` or ``<pkg>__unit_test`` label to its source files."""
    if target.endswith("__unit_test"):
        src = crate_root / "src"
        files = [src / "lib.rs", src / "main.rs"]
        return [f for f in files if f.is_file()] + [
            p for p in sorted(src.rglob("*.rs")) if p.name not in ("lib.rs", "main.rs")
        ]
    name = target[: -len("__test")] if target.endswith("__test") else target
    root = cargo_test_paths(crate_root).get(name)
    return [root] if root else []


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


def resolve_label(label: str) -> list[Path]:
    """A ``//package:target`` label to its test-root source files.

    Batch members are not confined to ``crates/`` -- the generated mapping
    carries ``runbooks/`` and ``examples/`` packages too -- so resolution is
    by label, not by directory convention.
    """
    package, _, target = label.partition(":")
    if not package.startswith("//") or not target:
        return []
    return resolve_bazel_label(ROOT / package.removeprefix("//"), target)


def census_testlogs(
    testlogs_dir: Path,
    batches: dict[str, list[str]],
    macros: dict[str, Macro],
    registered: dict[tuple[str, str], set[str]],
) -> list[str]:
    """The per-target census over one Bazel testlogs tree.

    A ran target is any directory carrying ``test.log`` or ``test.outputs``;
    its path relative to the root is its label's package and name, so
    discovery reaches nested packages (``runbooks/…``, ``examples/…``) the
    same way it reaches ``crates/…``. A target the generated
    ``WORKSPACE_TEST_BATCHES`` names as a batch is censused over the union of
    its members' sources: every member label must resolve to a test root,
    and a ran target that left receipts it cannot account for is a failure,
    never a skip.
    """
    errors: list[str] = []
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
        files: list[Path] = []
        unresolved: list[str] = []
        for member in members:
            member_files = resolve_label(member)
            if member_files:
                files.extend(member_files)
            else:
                unresolved.append(member)
        t_observed = bazel_receipts(target_dir)
        if unresolved and t_observed:
            errors.append(
                f"bazel target {target_label} ran and left "
                f"{len(t_observed)} receipts, but "
                + (
                    "no batch member"
                    if len(unresolved) == len(members)
                    else f"member(s) {', '.join(sorted(unresolved))}"
                )
                + " could be resolved to a test root -- the census cannot "
                "name the laws those receipts owe"
            )
        if not files:
            continue
        t_expected = expected_for_files(
            [f for root_f in files for f in source_files(root_f)], macros
        )
        if not t_expected:
            continue
        t_missing = sorted(t_expected - t_observed)
        for law, label in t_missing:
            errors.append(
                f"bazel target {target_label} ran but registered law "
                f"`{law}` (label `{label}`) left no execution receipt"
            )
        t_unknown = sorted(p for p in t_observed if p not in registered)
        for law, label in t_unknown:
            errors.append(
                f"bazel target {target_label} receipt for `{law}` (label "
                f"`{label}`) names no registered law"
            )
    return errors


def read_receipts(paths: list[Path]) -> set[tuple[str, str]]:
    observed: set[tuple[str, str]] = set()
    for path in paths:
        for line in path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            parts = line.split("\t")
            if len(parts) != 2:
                continue
            observed.add((parts[0], parts[1]))
    return observed


def bazel_receipts(target_dir: Path) -> set[tuple[str, str]]:
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipts", action="append", default=[], metavar="FILE")
    parser.add_argument("--receipts-root", action="append", default=[], metavar="DIR")
    parser.add_argument("--crate", dest="crates", action="append", default=[], metavar="DIR")
    parser.add_argument("--test-file", action="append", default=[], metavar="FILE")
    parser.add_argument("--suite", action="append", default=[], metavar="NAME")
    parser.add_argument("--labels", action="append", default=[], metavar="FILE")
    parser.add_argument("--crate-root", metavar="DIR")
    parser.add_argument("--bazel-testlogs", action="append", default=[], metavar="DIR")
    args = parser.parse_args()

    macros = macro_blocks(MACROS.read_text(encoding="utf-8"))
    registered = registered_pairs(macros)

    errors: list[str] = []

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
    observed = read_receipts([p for p in receipts_paths if p.is_file()])

    expected: dict[tuple[str, str], str] = {}

    def claim(pairs: set[tuple[str, str]], source: str) -> None:
        for pair in pairs:
            expected.setdefault(pair, source)

    for crate in args.crates:
        claim(expected_for_crate(ROOT / crate, macros), f"crate {crate}")
    for test_file in args.test_file:
        claim(
            expected_for_files(source_files(ROOT / test_file), macros),
            f"test file {test_file}",
        )
    for suite in args.suite:
        name = suite if suite.endswith("_tests") else f"{suite}_tests"
        pairs = suite_expected(macros, name)
        if not pairs:
            errors.append(f"suite {name} registers no laws in macros.rs")
        claim(pairs, f"suite {name}")
    if args.labels:
        crate_root = Path(args.crate_root or ".")
        for labels_file in args.labels:
            for line in (ROOT / labels_file).read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line or ":" not in line:
                    continue
                target = line.rsplit(":", 1)[1]
                files = resolve_bazel_label(crate_root, target)
                for f in files:
                    claim(
                        expected_for_files(source_files(f), macros),
                        f"label {line}",
                    )

    missing = sorted(set(expected) - observed)
    for law, label in missing:
        errors.append(
            f"registered law `{law}` (label `{label}`, claimed by "
            f"{expected[(law, label)]}) produced no execution receipt"
        )
    unknown = sorted(p for p in observed if p not in registered)
    for law, label in unknown:
        errors.append(
            f"receipt for `{law}` (label `{label}`) names no registered law in "
            "macros.rs -- stale or fabricated record"
        )

    batches = workspace_test_batches() if args.bazel_testlogs else {}
    for testlogs in args.bazel_testlogs:
        testlogs_dir = Path(testlogs)
        if not testlogs_dir.is_dir():
            errors.append(f"{testlogs} is not a testlogs directory")
            continue
        errors.extend(census_testlogs(testlogs_dir, batches, macros, registered))

    if errors:
        for error in errors:
            print(f"law execution census: {error}", file=sys.stderr)
        return 1
    print(
        f"law execution census: {len(observed)} receipts cover "
        f"{len(expected)} claimed laws"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
