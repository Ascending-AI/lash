#!/usr/bin/env python3
"""Every PostgreSQL-gated law in //crates/lash is selected by a Postgres suite.

The facade's PostgreSQL laws are ``#[ignore]``d tests: a bare ``cargo test``
runs zero of them, and for years that meant they ran only by hand (FIG-4743).
The ``postgres-store`` CI job selects them through ``store-tests.sh`` suites,
derived by name rather than listed by hand: a law whose only requirement is a
live PostgreSQL carries ``postgres`` in its libtest path, and the suites select
it with a ``postgres`` filter plus the ignored-test opt-in.

This check is the other half of that contract. It scans ``crates/lash`` for
every ignored marker whose reason names PostgreSQL -- on a ``fn``, on a
law-table entry (``#[ignore] name: ...``), inside a ``macro_rules!`` arm that
generates an ignored test, or as a macro argument (``laws!(postgres, ...,
ignore = "...")``) -- and requires, for each:

* the test binary the file compiles into is run by some suite invoked under a
  ``with-service.sh pg*`` wrapper in the workflows, and that suite asks for
  ignored tests;
* a PostgreSQL-only law is actually selected there (its fragment matches the
  suite's name filter and is not swallowed by a ``--skip``);
* a law whose reason names a second service (``restate``, ``managed``, ``s3``)
  is *not* silently run by a Postgres-only suite -- wherever a suite's filter
  would select it, a skip in the same suite must name it.

So a new ``#[ignore]``d PostgreSQL law fails this check if it lands in a
binary no Postgres suite runs, or is named such that no ``postgres`` filter
reaches it -- and a law that needs PostgreSQL plus a second service fails it
if the Postgres suite would run it without that service.

Only the standard library plus PyYAML is used, matching the sibling checks.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys

import yaml


ROOT = Path(__file__).resolve().parents[1]
CRATE = Path("crates/lash")
STORE_TESTS = Path("scripts/ci/store-tests.sh")

POSTGRES = re.compile(r"postgres", re.IGNORECASE)
# A reason naming one of these means a pg16 container alone does not satisfy
# the law, so the Postgres suite must not select it.
SECOND_SERVICE = re.compile(r"restate|managed|\bs3\b|minio", re.IGNORECASE)

# `#[ignore]` and `#[ignore = "reason"]`.
ATTR_IGNORE = re.compile(r'#\[\s*ignore\b(?:\s*=\s*"([^"]*)")?\s*\]')
# `ignore = "reason"` outside an attribute: the law-table form, where the
# reason is one macro argument among several (`laws!(postgres, true, ignore =
# "...")`).
ARG_IGNORE = re.compile(r'\bignore\s*=\s*"([^"]*)"')
# A bare gate literal among a macro's arguments, where the macro's own
# `$(#[ignore = $ignore])?` arm writes the attribute: `laws!(live_postgres,
# Postgres, true, "requires PostgreSQL and live Restate")`.
GATE_REASON = re.compile(r'requires[^"]*postgres|postgres[^"]*requires', re.IGNORECASE)
STRING_LITERAL = re.compile(r'"([^"]*)"')

INVOCATION = re.compile(r"\b(\w+)!\s*[({\[]")
MACRO_DEF = re.compile(r"\bmacro_rules!\s+(\w+)")

# A PostgreSQL-gated ignored law stays runnable only while its libtest path
# keeps `postgres`: that is what the suites' name filter selects.
FILTER_COVERED = re.compile(r"postgres")

# The count floor keeps a broken scanner honest: if the regexes stop finding
# the gate markers, the check must say so rather than report that everything
# is selected.
MIN_GATE_SITES = 20


@dataclass(frozen=True)
class Violation:
    path: str
    location: str
    detail: str


@dataclass(frozen=True)
class Suite:
    """One ignored-covering selection a Postgres-wrapped suite makes."""

    name: str
    labels: frozenset[str]
    filters: tuple[str, ...]  # empty means the whole binary
    skips: tuple[str, ...]

    def selects(self, fragment: str) -> bool:
        return not self.filters or any(f in fragment for f in self.filters)

    def skips_(self, fragment: str) -> bool:
        return any(skip in fragment for skip in self.skips)


@dataclass(frozen=True)
class Site:
    """One ignored marker in crates/lash with its resolved name fragments."""

    path: str
    line: int
    reason: str
    fragments: tuple[str, ...]


def line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def matching_close(text: str, open_pos: int) -> int:
    """The offset just past the bracket matching `text[open_pos]`."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    opening = text[open_pos]
    depth = 1
    pos = open_pos + 1
    while pos < len(text):
        char = text[pos]
        if char == '"':
            pos += 1
            while pos < len(text) and text[pos] != '"':
                pos += 2 if text[pos] == "\\" else 1
        elif char == "'":
            # A char literal may contain a bracket; a lifetime never does.
            if pos + 2 < len(text) and text[pos + 1] in "()[]{}\\":
                pos += 2
                while pos < len(text) and text[pos] != "'":
                    pos += 1
        elif char == "/" and text[pos : pos + 2] == "//":
            newline = text.find("\n", pos)
            pos = len(text) if newline == -1 else newline
        elif char in pairs:
            depth += 1
        elif char == pairs[opening]:
            depth -= 1
            if depth == 0:
                return pos + 1
        pos += 1
    return len(text)


def enclosing_invocation(text: str, pos: int) -> re.Match | None:
    """The innermost `name!(...)`/`name!{...}` invocation containing `pos`."""
    innermost = None
    for candidate in INVOCATION.finditer(text):
        open_pos = candidate.end() - 1
        if open_pos > pos:
            break
        if pos < matching_close(text, open_pos):
            if innermost is None or candidate.start() > innermost.start():
                innermost = candidate
    return innermost


def invocation_arguments(text: str, invocation: re.Match) -> str:
    open_pos = invocation.end() - 1
    return text[open_pos + 1 : matching_close(text, open_pos) - 1]


def postgres_argument(text: str, invocation: re.Match) -> str | None:
    """The name argument of a law-table invocation.

    Law-table macros take the generated module or case name first
    (`laws!(postgres, ...)`, `backend_laws!(double_postgres, ...)`), so the
    fragment is the first snake_case argument that carries `postgres` --
    failing that, the first snake_case argument at all, which lets the
    coverage verdict complain about the convention rather than miss it.
    """
    idents = re.findall(r"\b[a-z_]\w*\b", invocation_arguments(text, invocation))
    for ident in idents:
        if FILTER_COVERED.search(ident):
            return ident
    return idents[0] if idents else None


def macro_invocation_fragments(text: str, macro: str) -> tuple[str, ...]:
    """The postgres-carrying arguments every `macro!` invocation binds.

    `#[ignore]` inside a `macro_rules!` body precedes a metavariable `fn`
    (`async fn $postgres()`), so the marked test names come from the
    invocations, not the definition.
    """
    fragments: list[str] = []
    for invocation in INVOCATION.finditer(text):
        if invocation.group(1) != macro:
            continue
        for ident in re.findall(
            r"\b[a-z_]\w*\b", invocation_arguments(text, invocation)
        ):
            if FILTER_COVERED.search(ident):
                fragments.append(ident)
    return tuple(fragments)


def resolve_attribute(text: str, site: re.Match) -> tuple[str, ...]:
    """The name fragment(s) an `#[ignore]` attribute marks.

    Three shapes follow the attribute: a plain `fn name`, a law-table entry
    `name: ...` or `name => ...` inside an invocation, and a metavariable `fn
    $var` inside a `macro_rules!` body (resolved through the invocations).
    """
    rest = text[site.end() :]
    # Skip whitespace, line comments, and attributes stacked between the
    # ignore marker and the item (`#[ignore]` then `#[allow(...)]` then `fn`).
    while True:
        skipped = re.match(r"\s+|//[^\n]*|#\[[^\]]*\]", rest)
        if not skipped:
            break
        rest = rest[skipped.end() :]
    metavar = re.match(r"(?:async\s+)?fn\s+\$(\w+)", rest)
    if metavar:
        definitions = MACRO_DEF.findall(text[: site.start()])
        if not definitions:
            return ()
        return macro_invocation_fragments(text, definitions[-1])
    item = re.match(
        r"(?:async\s+)?fn\s+(\w+)|mod\s+(\w+)|(\w+)\s*:|(\w+)\s*=>", rest
    )
    if item is None:
        return ()
    return (next(group for group in item.groups() if group is not None),)


def scan_file(path: Path, text: str) -> list[Site]:
    sites: list[Site] = []
    attr_spans = []
    for match in ATTR_IGNORE.finditer(text):
        attr_spans.append(match.span())
        sites.append(
            Site(
                path=str(path),
                line=line_of(text, match.start()),
                reason=match.group(1) or "",
                fragments=resolve_attribute(text, match),
            )
        )
    for match in ARG_IGNORE.finditer(text):
        if any(start <= match.start() < end for start, end in attr_spans):
            continue  # the `ignore =` inside a `#[ignore = "..."]` attribute
        reason = match.group(1)
        if not POSTGRES.search(reason):
            continue
        invocation = enclosing_invocation(text, match.start())
        fragment = (
            postgres_argument(text, invocation) if invocation is not None else None
        )
        sites.append(
            Site(
                path=str(path),
                line=line_of(text, match.start()),
                reason=reason,
                fragments=() if fragment is None else (fragment,),
            )
        )
    for match in STRING_LITERAL.finditer(text):
        if any(start <= match.start() < end for start, end in attr_spans):
            continue
        reason = match.group(1)
        if not GATE_REASON.search(reason):
            continue
        # Not part of an `ignore = "..."` argument.
        if re.search(r"\bignore\s*=\s*$", text[: match.start()]):
            continue
        invocation = enclosing_invocation(text, match.start())
        if invocation is None:
            continue  # a literal about PostgreSQL, not a gate marker
        fragment = postgres_argument(text, invocation)
        sites.append(
            Site(
                path=str(path),
                line=line_of(text, match.start()),
                reason=reason,
                fragments=() if fragment is None else (fragment,),
            )
        )
    return sites


def target_label(path: Path) -> str:
    """The Buck2 label whose test binary compiles this file."""
    relative = path.relative_to(CRATE)
    if relative.parts[0] == "src":
        return "//crates/lash:lash__unit_test"
    # tests/<name>.rs and tests/<name>/** both compile into <name>__test.
    stem = relative.parts[1]
    if stem.endswith(".rs"):
        stem = stem[: -len(".rs")]
    return f"//crates/lash:{stem}__test"


def uniform_suites(script: str) -> dict[str, str]:
    table = script.split("declare -A uniform_store_suites=(", 1)
    if len(table) != 2:
        raise ValueError("store-tests.sh has no uniform_store_suites table")
    table = table[1].split("\n)", 1)[0]
    return {
        name: row
        for name, row in re.findall(r'^\s*\[\s*([a-z0-9-]+)\s*\]="([^"]*)"', table, re.M)
    }


def postgres_wrapped_suites(workflows: dict[Path, object]) -> set[str]:
    """Every `store-tests.sh` suite run inside a `with-service.sh pg*` leg."""
    suites = set()
    for document in workflows.values():
        if not isinstance(document, dict):
            continue
        jobs = document.get("jobs")
        if not isinstance(jobs, dict):
            continue
        for job in jobs.values():
            steps = job.get("steps") if isinstance(job, dict) else None
            if not isinstance(steps, list):
                continue
            for step in steps:
                run = step.get("run") if isinstance(step, dict) else None
                if not isinstance(run, str):
                    continue
                if not re.search(r"with-service\.sh\s+\"?pg", run):
                    continue
                suites.update(
                    re.findall(r"scripts/ci/store-tests\.sh\s+([a-z0-9-]+)", run)
                )
    return suites


def covering_suites(root: Path) -> tuple[list[Suite], list[Violation]]:
    """The ignored-covering selections the Postgres service legs make."""
    violations: list[Violation] = []
    script_path = root / STORE_TESTS
    try:
        script = script_path.read_text(encoding="utf-8")
    except OSError as error:
        return [], [Violation(str(STORE_TESTS), "file", f"cannot read the recipe: {error}")]
    try:
        rows = uniform_suites(script)
    except ValueError as error:
        return [], [Violation(str(STORE_TESTS), "table", str(error))]

    workflows: dict[Path, object] = {}
    directory = root / ".github" / "workflows"
    for path in sorted(directory.glob("*.yml")) + sorted(directory.glob("*.yaml")):
        try:
            workflows[path] = yaml.safe_load(path.read_text(encoding="utf-8"))
        except yaml.YAMLError as error:
            violations.append(
                Violation(str(path), "document", f"cannot parse workflow: {error}")
            )
    wrapped = postgres_wrapped_suites(workflows)

    suites: list[Suite] = []
    for name in sorted(wrapped & rows.keys()):
        fields = rows[name].split("|")
        if len(fields) != 6:
            violations.append(
                Violation(
                    str(STORE_TESTS),
                    f"suite `{name}`",
                    "does not match the label|filters|package|target|runner|flags "
                    "row shape the check reads",
                )
            )
            continue
        labels, filter_list, _package, _target, _runner, flags = fields
        selections = flags.split(",") if flags else []
        if not {"include-ignored", "ignored-only"} & set(selections):
            continue  # runs only the binaries' non-ignored tests
        suites.append(
            Suite(
                name=name,
                labels=frozenset(
                    label for label in labels.split(",") if label
                ),
                filters=tuple(f for f in filter_list.split(",") if f),
                skips=tuple(
                    flag[len("skip=") :]
                    for flag in selections
                    if flag.startswith("skip=")
                ),
            )
        )
    return suites, violations


def check_repository(root: Path) -> list[Violation]:
    violations: list[Violation] = []
    suites, suite_violations = covering_suites(root)
    violations.extend(suite_violations)

    sites: list[Site] = []
    for subtree in ("src", "tests"):
        for path in sorted(root.joinpath(CRATE, subtree).rglob("*.rs")):
            sites.extend(
                scan_file(path.relative_to(root), path.read_text(encoding="utf-8"))
            )

    gated = [
        site
        for site in sites
        if POSTGRES.search(site.reason)
        or any(FILTER_COVERED.search(fragment) for fragment in site.fragments)
    ]
    if len(gated) < MIN_GATE_SITES:
        violations.append(
            Violation(
                str(CRATE),
                "scan",
                f"found only {len(gated)} PostgreSQL-gated ignored sites; the "
                "crate's law tables alone mark more than that, so the scanner "
                "is not seeing the gates it exists to cover",
            )
        )

    for site in gated:
        where = f"{site.path}:{site.line}"
        if not site.fragments:
            violations.append(
                Violation(
                    site.path,
                    f"line {site.line}",
                    "an ignored marker the check cannot resolve to a test name: "
                    f"{site.reason!r}. Name the gate the way the law tables do "
                    "so the PostgreSQL suite's selection provably covers it.",
                )
            )
            continue
        label = target_label(Path(site.path))
        candidates = [suite for suite in suites if label in suite.labels]
        if not candidates:
            violations.append(
                Violation(
                    site.path,
                    f"line {site.line}",
                    f"needs PostgreSQL ({site.reason!r}) but no suite run under "
                    f"a `with-service.sh pg*` leg selects ignored tests in "
                    f"{label}; add the binary to the facade-laws suite.",
                )
            )
            continue
        needs_second_service = SECOND_SERVICE.search(site.reason)
        for fragment in site.fragments:
            selected = [suite for suite in candidates if suite.selects(fragment)]
            if needs_second_service:
                for suite in selected:
                    if not suite.skips_(fragment):
                        violations.append(
                            Violation(
                                site.path,
                                f"line {site.line}",
                                f"needs a second service ({site.reason!r}) but "
                                f"`{suite.name}` selects {fragment!r} without "
                                "skipping it; the law would run without that "
                                "service and fail. Name a `skip=` for it.",
                            )
                        )
            else:
                selected = [
                    suite for suite in selected if not suite.skips_(fragment)
                ]
                if not selected:
                    detail = (
                        f"is a PostgreSQL-only law ({site.reason!r}) in {label} "
                        "that no Postgres suite selects"
                    )
                    if any(
                        suite.selects(fragment) and suite.skips_(fragment)
                        for suite in candidates
                    ):
                        detail += "; a `skip=` meant for a second-service law "
                        "excludes it"
                    elif candidates and all(suite.filters for suite in candidates):
                        detail += (
                            ": every covering suite filters by name, and "
                            f"{fragment!r} matches none of them -- the "
                            "convention is `postgres` in the libtest path"
                        )
                    violations.append(Violation(site.path, f"line {site.line}", detail))

    return violations


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    violations = check_repository(args.repo)
    if violations:
        print("PostgreSQL gate coverage check failed:", file=sys.stderr)
        for violation in violations:
            print(
                f"- {violation.path}: {violation.location}: {violation.detail}",
                file=sys.stderr,
            )
        return 1
    print("PostgreSQL gate coverage check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
