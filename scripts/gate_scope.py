#!/usr/bin/env python3
"""Classify a diff into the local gate families it can possibly affect.

A diff that only touches prose still pays for the full compile battery. This
script maps the merge-base..head path set (plus anything dirty in the working
tree) onto gate *families*, so a battery can skip the families no touched path
can reach.

The rule the whole design answers to: a family is only ever marked ``skip``
when every touched path provably cannot affect it. Anything ambiguous -- a
shared input, an unrecognised path, an empty path set, a git failure -- runs
everything. Being wrong in the skip direction hides a real failure until CI;
being wrong in the run direction costs wall-clock and nothing else.

Families
--------
``rust-compile``
    fmt, check, clippy, nextest, doctests, rustdoc, the API-coverage build,
    and every source guard that reads ``crates/``.
``docs-text``
    lint_docs, the production file-size budget, documented format versions.
``scripts``
    the repository script self-test suite.
``registry``
    ``docs/api-example-coverage.toml`` consistency. Emitted for CI's benefit
    and for the audit table; ``push-gate.sh`` has no registry leg to dispatch,
    because ``check_api_example_coverage.py`` is deliberately CI-only (an
    omission must block the merge, not the push).
``workflows``
    the ``.github/`` guards. This family can never differ from the global
    answer today: its only inputs are ``.github/**``, ``scripts/**`` and the
    ``justfile``, which are shared inputs or unrecognised, so anything that
    moves it already runs everything. It stays a separate axis because the
    guard it owns is *about* those paths, and folding it into shared-inputs
    would hide that fact the day a workflow gate grows a narrower input.

Prose the Rust suite reads
--------------------------
``docs-only`` is only a safe skip while no compiled test depends on prose. The
workspace has tests that do -- they read ADRs and ``CONTEXT.md`` at run time
and assert on their wording -- so those exact files are named in
``RUST_RUNTIME_DOC_INPUTS`` and classified as affecting ``rust-compile``. The
alternative shapes were rejected: a ``docs/**`` glob would shrink ``docs-only``
to nothing, and a bare hardcoded list would rot the first time someone adds
another such test. The list is therefore *enforced*, not asserted: the
self-test sweeps every tracked ``*.rs`` for references to ``docs/**`` and
``CONTEXT.md`` and fails on any path this file does not already treat as
rust-affecting.

Two local gates are deliberately in *no* family and must always run:
``scripts/release_notes.py check-pr`` and ``scripts/check-transcript-diff.py``
read the commit range's messages and the diff's semantics, not its paths, so
every non-empty change affects them. A path-scoped classifier has nothing to
say about a commit-scoped gate.

Note that ``docs-text`` runs for source changes too: its gates read ``crates/``
as well as ``docs/``. It stays a named family so the audit line printed into a
gate log enumerates every family rather than an interesting subset.

Only the Python standard library is used, so this runs before any toolchain.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import shlex
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent

FAMILIES = ("rust-compile", "docs-text", "scripts", "registry", "workflows")
ALL_FAMILIES = frozenset(FAMILIES)

# Shared inputs: paths that can move the result of *any* gate, either because
# they feed the build graph (manifests, lockfile, toolchain, cargo config,
# build scripts) or because they are the gate machinery itself (scripts/,
# .github/). A change here is never provably narrow, so it runs everything --
# including, self-referentially, changes to this file.
SHARED_INPUT_PATTERNS = (
    "Cargo.toml",
    "Cargo.lock",
    "**/Cargo.toml",
    "**/Cargo.lock",
    "rust-toolchain*",
    "**/rust-toolchain*",
    ".config/**",
    ".cargo/**",
    "scripts/**",
    ".github/**",
    "build.rs",
    "**/build.rs",
)


# Prose files that a Rust test reads at run time, so a change to one of them
# can fail `cargo nextest` without touching a single line of Rust. See the
# `rust-runtime-doc-inputs` rule below for why this exists and
# `RustRuntimeDocInputTests` in the self-test for what keeps it honest.
RUST_RUNTIME_DOC_INPUTS = (
    "docs/adr/0008-confidence-gate.md",
    "docs/adr/0009-deterministic-simulation-harness.md",
    "CONTEXT.md",
)


@dataclass(frozen=True)
class Rule:
    """A named path class and the gate families a match in it can affect."""

    name: str
    patterns: tuple[str, ...]
    families: frozenset[str]


# Ordered, first match wins: the narrowest classes come first so a specific
# path is not swallowed by the shared-input net behind it.
RULES: tuple[Rule, ...] = (
    # The example-coverage registry names public symbols, so a change to it is
    # only meaningful against the crates it cites: it runs the registry gate
    # and the rust-compile family that owns the API-coverage leg.
    Rule(
        name="registry-manifest",
        patterns=("docs/api-example-coverage.toml",),
        families=frozenset({"registry", "rust-compile", "docs-text"}),
    ),
    Rule(
        name="shared-inputs",
        patterns=SHARED_INPUT_PATTERNS,
        families=ALL_FAMILIES,
    ),
    # Rust sources also drive the registry (coverage is a claim about which
    # public symbols examples use) and the text gates that scan `crates/`.
    Rule(
        name="rust-sources",
        patterns=("crates/**", "examples/**/*.rs"),
        families=frozenset({"rust-compile", "docs-text", "registry"}),
    ),
    # Prose the Rust suite reads. `rust-sources` already routes crate-local
    # markdown to `rust-compile` because `include_str!` can pull it in; the
    # same reasoning applies to repository-root prose that a test opens at
    # runtime, and the workspace has such tests. These paths are prose, so the
    # text gates still own them, but a change here can turn the Rust suite red
    # and must not be classified `docs-only`.
    #
    # The list is exact rather than a glob so `docs-only` keeps its value: only
    # the files a Rust source actually names are pulled in. It is pinned by
    # `RustRuntimeDocInputTests` in `test_gate_scope.py`, which sweeps every
    # tracked `*.rs` for `docs/**` and `CONTEXT.md` references and fails on any
    # path this rule does not cover -- so a newly added runtime doc read
    # reopens no hole silently, it turns the gate red until the rule catches up.
    Rule(
        name="rust-runtime-doc-inputs",
        patterns=RUST_RUNTIME_DOC_INPUTS,
        families=frozenset({"rust-compile", "docs-text"}),
    ),
    # Prose: `docs/**` and repository-root markdown (README, CONTRIBUTING,
    # CONTEXT, PRODUCT, ...). Markdown nested under a crate is matched by
    # `rust-sources` above instead, because it can be pulled into a doc test
    # by `include_str!`.
    Rule(
        name="docs-prose",
        patterns=("docs/**", "*.md"),
        families=frozenset({"docs-text"}),
    ),
)

UNKNOWN = "unknown"


def glob_to_regex(pattern: str) -> re.Pattern[str]:
    """Translate a slash-aware glob into a full-match regex.

    ``**`` crosses directory separators, ``*`` and ``?`` do not. ``**/`` also
    matches zero directories, so ``**/Cargo.toml`` covers the root manifest.
    """

    out: list[str] = []
    index = 0
    while index < len(pattern):
        char = pattern[index]
        if pattern.startswith("**/", index):
            out.append("(?:.*/)?")
            index += 3
        elif pattern.startswith("**", index):
            out.append(".*")
            index += 2
        elif char == "*":
            out.append("[^/]*")
            index += 1
        elif char == "?":
            out.append("[^/]")
            index += 1
        else:
            out.append(re.escape(char))
            index += 1
    return re.compile("".join(out) + r"\Z")


_COMPILED: dict[str, re.Pattern[str]] = {}


def matches(pattern: str, path: str) -> bool:
    compiled = _COMPILED.get(pattern)
    if compiled is None:
        compiled = glob_to_regex(pattern)
        _COMPILED[pattern] = compiled
    return compiled.match(path) is not None


def classify_path(path: str) -> str:
    """Return the name of the first rule matching ``path``, or ``UNKNOWN``."""

    for rule in RULES:
        if any(matches(pattern, path) for pattern in rule.patterns):
            return rule.name
    return UNKNOWN


@dataclass(frozen=True)
class Scope:
    """The classification verdict for one path set."""

    classification: str
    reason: str
    families: frozenset[str]

    def runs(self, family: str) -> bool:
        return family in self.families


def _rule_by_name(name: str) -> Rule:
    for rule in RULES:
        if rule.name == name:
            return rule
    raise KeyError(name)


def _sample(paths: list[str], limit: int = 3) -> str:
    shown = paths[:limit]
    remainder = len(paths) - len(shown)
    text = ", ".join(shown)
    if remainder > 0:
        text += f", +{remainder} more"
    return text


def classify(paths: list[str]) -> Scope:
    """Map a touched-path list onto the families it can affect."""

    ordered = sorted(dict.fromkeys(p for p in paths if p.strip()))
    if not ordered:
        return Scope(
            classification="empty-diff",
            reason=(
                "no touched paths were resolved; an empty path set is a "
                "resolution failure, not a proof of narrowness"
            ),
            families=ALL_FAMILIES,
        )

    buckets: dict[str, list[str]] = {}
    for path in ordered:
        buckets.setdefault(classify_path(path), []).append(path)

    if UNKNOWN in buckets:
        return Scope(
            classification="unknown-paths",
            reason=(
                "paths outside every known class: "
                f"{_sample(buckets[UNKNOWN])}"
            ),
            families=ALL_FAMILIES,
        )

    families: set[str] = set()
    for name in buckets:
        families |= _rule_by_name(name).families

    if "shared-inputs" in buckets:
        classification = "shared-inputs"
    elif set(buckets) == {"docs-prose"}:
        classification = "docs-only"
    elif set(buckets) == {"rust-sources"}:
        classification = "rust-only"
    elif set(buckets) == {"registry-manifest"}:
        classification = "registry-only"
    elif set(buckets) == {"rust-runtime-doc-inputs"}:
        classification = "rust-input-docs"
    else:
        classification = "mixed"

    reason = "; ".join(
        f"{name} ({_sample(buckets[name])})" for name in sorted(buckets)
    )
    return Scope(
        classification=classification,
        reason=reason,
        families=frozenset(families),
    )


def _git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} failed ({result.returncode}): "
            f"{result.stderr.strip()}"
        )
    return result.stdout


def _worktree_paths(repo: Path) -> list[str]:
    """Uncommitted paths, so a dirty tree cannot be classified as narrow."""

    tokens = [t for t in _git(repo, "status", "--porcelain", "-z").split("\0") if t]
    paths: list[str] = []
    index = 0
    while index < len(tokens):
        entry = tokens[index]
        index += 1
        if len(entry) < 4:
            continue
        status, path = entry[:2], entry[3:]
        paths.append(path)
        # A rename or copy spends a second NUL-token on its source path, and
        # that path is touched too.
        if "R" in status or "C" in status:
            if index < len(tokens):
                paths.append(tokens[index])
                index += 1
    return paths


def collect_paths(repo: Path, base: str, head: str, worktree: bool) -> list[str]:
    merge_base = _git(repo, "merge-base", base, head).strip()
    if not merge_base:
        raise RuntimeError(f"no merge base between {base} and {head}")
    # `-z` so a path needing quoting arrives as itself rather than as an
    # escaped string that would classify as unknown.
    diff = _git(repo, "diff", "--name-only", "-z", f"{merge_base}..{head}")
    paths = [entry for entry in diff.split("\0") if entry]
    if worktree:
        paths.extend(_worktree_paths(repo))
    return paths


def render_text(scope: Scope) -> str:
    lines = [
        f"{family}: {'run' if scope.runs(family) else 'skip'}"
        for family in FAMILIES
    ]
    lines.append(f"classification: {scope.classification} -- {scope.reason}")
    return "\n".join(lines)


def render_env(scope: Scope) -> str:
    lines = [
        f"GATE_RUN_{family.upper().replace('-', '_')}="
        f"{1 if scope.runs(family) else 0}"
        for family in FAMILIES
    ]
    lines.append(f"GATE_SCOPE_CLASSIFICATION={shlex.quote(scope.classification)}")
    lines.append(f"GATE_SCOPE_REASON={shlex.quote(scope.reason)}")
    return "\n".join(lines)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Classify a diff into the gate families it can affect."
    )
    parser.add_argument(
        "--base",
        default="origin/main",
        help="base ref to take the merge base against (default: origin/main)",
    )
    parser.add_argument("--head", default="HEAD", help="head ref (default: HEAD)")
    parser.add_argument(
        "--paths-from",
        help=(
            "read the touched-path list from this file ('-' for stdin) "
            "instead of consulting git"
        ),
    )
    parser.add_argument(
        "--format",
        choices=("text", "env"),
        default="text",
        help="'text' for the audit table, 'env' for shell eval",
    )
    parser.add_argument(
        "--no-worktree",
        action="store_true",
        help="ignore uncommitted changes (they are included by default)",
    )
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(list(sys.argv[1:] if argv is None else argv))

    try:
        if args.paths_from:
            if args.paths_from == "-":
                raw = sys.stdin.read()
            else:
                raw = Path(args.paths_from).read_text(encoding="utf-8")
            paths = [line.strip() for line in raw.splitlines()]
        else:
            paths = collect_paths(
                args.repo, args.base, args.head, not args.no_worktree
            )
    except (RuntimeError, OSError) as error:
        print(f"gate_scope: {error}", file=sys.stderr)
        print("gate_scope: callers must treat this as 'run everything'", file=sys.stderr)
        return 2

    scope = classify(paths)
    render = render_env if args.format == "env" else render_text
    print(render(scope))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
