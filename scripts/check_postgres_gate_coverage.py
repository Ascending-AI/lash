#!/usr/bin/env python3
"""Every PostgreSQL law target has a runner that supplies its database.

The facade's PostgreSQL laws are ``#[ignore]``d tests: a bare ``cargo test``
runs zero of them, and for years that meant they ran only by hand (FIG-4743).
The ``postgres-store`` CI job selects them through ``store-tests.sh`` suites,
derived by name rather than listed by hand: a law whose only requirement is a
live PostgreSQL carries ``postgres`` in its libtest path, and the suites select
it with a ``postgres`` filter plus the ignored-test opt-in.

This check resolves every generated Rust test root and follows its modules,
including shared #[path] helpers. URL reads, PostgreSQL law names and the PG
tier macro require hermetic-postgres or a named main/certification gate.
An ignored law additionally needs an ignored-covering selection: merely
starting a hermetic server does not execute an ignored test.

It scans the workspace for
every ignored marker whose reason names PostgreSQL -- on a ``fn``, on a
law-table entry (``#[ignore] name: ...``), inside a ``macro_rules!`` arm that
generates an ignored test, or as a macro argument (``laws!(postgres, ...,
ignore = "...")``) -- and requires, for each:

* the test binary the file compiles into is run by some suite invoked under a
  ``with-service.sh pg*`` wrapper in the workflows, and that suite asks for
  ignored tests;
* a PostgreSQL-only law is actually selected there (its fragment matches the
  suite's name filter and is not swallowed by a ``--skip``);
* a law whose reason names a second service (``managed``, ``s3``)
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
import ast
from dataclasses import dataclass
import json
from pathlib import Path
import re
import sys

import yaml
from fixture_regenerators import TOKEN, without_comments


ROOT = Path(__file__).resolve().parents[1]
CRATE = Path("crates/lash")
STORE_TESTS = Path("scripts/ci/store-tests.sh")

POSTGRES = re.compile(r"postgres", re.IGNORECASE)
# A reason naming one of these means a `pg` container alone does not satisfy
# the law, so the Postgres suite must not select it.
SECOND_SERVICE = re.compile(r"managed|\bs3\b|minio|LASH_RELEASE_FIXTURES_DIR", re.IGNORECASE)

# `#[ignore]` and `#[ignore = "reason"]`.
ATTR_IGNORE = re.compile(r'#\[\s*ignore\b(?:\s*=\s*"([^"]*)")?\s*\]')
# `ignore = "reason"` outside an attribute: the law-table form, where the
# reason is one macro argument among several (`laws!(postgres, true, ignore =
# "...")`).
ARG_IGNORE = re.compile(r'\bignore\s*=\s*"([^"]*)"')
# A bare gate literal among a macro's arguments, where the macro's own
# `$(#[ignore = $ignore])?` arm writes the attribute: `laws!(s3_postgres,
# Postgres, true, "requires PostgreSQL and S3")`.
GATE_REASON = re.compile(r'requires[^"]*postgres|postgres[^"]*requires', re.IGNORECASE)
STRING_LITERAL = re.compile(r'"([^"]*)"')

INVOCATION = re.compile(r"\b(\w+)!\s*[({\[]")
MACRO_DEF = re.compile(r"\bmacro_rules!\s+(\w+)")

# A PostgreSQL-gated ignored law stays runnable only while its libtest path
# keeps `postgres`: that is what the suites' name filter selects.
FILTER_COVERED = re.compile(r"postgres")
PG_READ = re.compile(
    r'(?:var(?:_os)?\s*\(\s*"LASH_POSTGRES_DATABASE_URL"|required_database_url\s*\(|postgres_from_env\s*\()'
)
TEST_FUNCTION = re.compile(
    r'#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*(?:#\[[^\]]*\]\s*)*'
    r'(?:async\s+)?fn\s+(\w+)[^{]*\{'
)

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
    ignored: bool = True
    ignored_only: bool = False

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
    closing = [pairs[text[open_pos]]]
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
            closing.append(pairs[char])
        elif char == closing[-1]:
            closing.pop()
            if not closing:
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
        invocation = enclosing_invocation(text, site.start())
        fragment = postgres_argument(text, invocation) if invocation else None
        return (fragment,) if fragment else ()
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
        if not re.search(r"laws$|^tiered", invocation.group(1)):
            continue  # diagnostic literals in ordinary macros are not gate markers
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
            matrix = job.get("strategy", {}).get("matrix", {})
            services = matrix.get("service", [row.get("service") for row in matrix.get("include", [])])
            for step in steps:
                run = step.get("run") if isinstance(step, dict) else None
                if not isinstance(run, str):
                    continue
                if not re.search(r"with-service\.sh\s+\"?pg", run) and not (
                    'with-service.sh "${SERVICE}"' in run
                    and services and set(services) <= {"pg", "pg17", "pg18"}
                ):
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
                ignored_only="ignored-only" in selections,
            )
        )
    if "pg-release" in wrapped:
        # Read the literal release legs, not an independently maintained list.
        for name, arguments, labels in re.findall(
            r'echo "([^|]+)\|(?:shared|owned)\|([^|]*)\|([^|]*)\|', script
        ):
            if "$" in labels:
                continue  # the generated store row is hermetic already
            words = arguments.split()
            suites.append(Suite(
                name=f"pg-release/{name}", labels=frozenset(labels.split()),
                filters=tuple(word for word in words if not word.startswith("--")), skips=(),
                ignored="--include-ignored" in words or "--ignored" in words,
                ignored_only="--ignored" in words,
            ))
    return suites, violations


def rust_modules(source: Path):
    """Follow real module declarations, including inline modules and #[path]."""
    visited = set()

    def file(path, directory, prefix):
        if path in visited:
            return
        if not path.is_file():
            raise ValueError(f"missing Rust test source {path}")
        visited.add(path)
        raw = path.read_text(encoding="utf-8")
        # The shared lexer preserves offsets but blanks comment newlines.
        # Restore them so diagnostics retain the source's real line numbers.
        text = "".join("\n" if before == "\n" else after
                       for before, after in zip(raw, without_comments(raw)))
        yield path, prefix, text
        tokens = list(TOKEN.finditer(text))

        def close(at):
            opening = tokens[at][0]
            closing = {"{": "}", "(": ")", "[": "]"}[opening]
            depth = 1
            for stop in range(at + 1, len(tokens)):
                depth += (tokens[stop][0] == opening) - (tokens[stop][0] == closing)
                if depth == 0:
                    return stop
            raise ValueError(f"unclosed {opening} in {path}")

        def block(start, end, base, attribute_base, names):
            attrs = []
            at = start
            while at < end:
                value = tokens[at][0]
                if value == "#" and tokens[at + 1][0] == "[":
                    stop = close(at + 1)
                    attrs.append(text[tokens[at].start():tokens[stop].end()])
                    at = stop + 1
                    continue
                if value == "pub" and at + 1 < end and tokens[at + 1][0] == "(":
                    at = close(at + 1) + 1
                    continue
                if value == "mod" and at + 2 < end:
                    name, following = tokens[at + 1][0], tokens[at + 2][0]
                    if following == "{":
                        stop = close(at + 2)
                        yield from block(at + 3, stop, base / name, base / name, (*names, name))
                        at = stop + 1
                    elif following == ";":
                        override = re.search(r'#\[path\s*=\s*("[^"\n]+")\]', "\n".join(attrs))
                        child = attribute_base / json.loads(override[1]) if override else base / f"{name}.rs"
                        if not override and not child.is_file():
                            child = base / name / "mod.rs"
                        child_dir = child.parent if override or child.name == "mod.rs" else child.with_suffix("")
                        yield from file(child, child_dir, (*names, name))
                        at += 3
                    else:
                        at += 1
                    attrs = []
                    continue
                if value not in ("pub", "async", "unsafe"):
                    attrs = []
                if value in ("{", "(", "["):
                    at = close(at) + 1
                else:
                    at += 1

        yield from block(0, len(tokens), directory, path.parent, prefix)

    yield from file(source, source.parent, ())


def postgres_targets(root: Path):
    """Resolve every generated Rust test root; never infer owners from filenames."""
    inventory = json.loads((root / "tools/buck2/target-inventory.json").read_text())
    for package in inventory["packages"]:
        directory = Path(package["manifest"]).parent
        rules = {}
        for statement in ast.parse((root / directory / "BUCK").read_text()).body:
            if not isinstance(statement, ast.Expr) or not isinstance(statement.value, ast.Call):
                continue
            values = {kw.arg: ast.literal_eval(kw.value) for kw in statement.value.keywords
                      if kw.arg in ("name", "crate_root")}
            if "crate_root" in values:
                rules[values["name"]] = values["crate_root"]
        for target in package["targets"]:
            if target["kind"] not in ("test", "unit-test", "bin-unit-test"):
                continue
            label = target["label"]
            if label is None:
                continue  # Cargo-owned feature witnesses have no generated test root
            source = root / directory / rules[label.split(":", 1)[1]]
            sites = []
            markers = []
            fragments = set()
            for path, prefix, text in rust_modules(source):
                relative = path.relative_to(root)
                for site in scan_file(relative, text):
                    if not site.reason.startswith("regenerates ") and (POSTGRES.search(site.reason) or any(POSTGRES.search(f) for f in site.fragments)):
                        sites.append(Site(site.path, site.line, site.reason,
                                          tuple("::".join((*prefix, f)) for f in site.fragments)))
                # The tier macro instantiates postgres:: laws even though its
                # callsite contains neither a URL nor a PostgreSQL test name.
                marker_text = list(text)
                for definition in MACRO_DEF.finditer(text):
                    opening = text.find("{", definition.end())
                    if opening >= 0:
                        stop = matching_close(text, opening)
                        marker_text[definition.start():stop] = " " * (stop - definition.start())
                marker = re.search(r'\btiered_laws!|\b\w*_on_postgres\b', "".join(marker_text))
                if marker:
                    markers.append(f"{relative}:{line_of(text, marker.start())}")
                concrete = "".join(marker_text)
                for invocation in re.finditer(r'\btiered_laws!\s*\(', concrete):
                    opening = invocation.end() - 1
                    arguments = concrete[opening + 1:matching_close(concrete, opening) - 1]
                    arguments = arguments.strip().removeprefix("current_thread:")
                    fragments.update("::".join((*prefix, "postgres", law))
                                     for law in re.findall(r'\b[a-z_]\w*\b', arguments))
                fragments.update("::".join((*prefix, name)) for pair in re.findall(
                    r'\bfn\s+(\w*on_postgres\w*)\s*\(|\b(\w*on_postgres\w*)\s*=>', concrete
                ) for name in pair if name)
                for function in TEST_FUNCTION.finditer(concrete):
                    opening = function.end() - 1
                    if PG_READ.search(concrete[opening:matching_close(concrete, opening)]):
                        markers.append(f"{relative}:{line_of(text, function.start())}")
                        fragments.add("::".join((*prefix, function[1])))
                if target["kind"] == "test" and re.search(r'\bpostgres::', concrete):
                    markers.append(str(relative))
                if target["kind"] == "test" and re.search(r'#\[(?:tokio::)?test\b', text) and PG_READ.search(text):
                    markers.append(str(relative))
            if sites or markers:
                yield dict(target, postgres_laws=sorted(fragments)), sites, markers


def check_repository(root: Path) -> list[Violation]:
    suites, violations = covering_suites(root)
    inventory_path = root / "tools/buck2/target-inventory.json"
    if (root / "Cargo.toml").is_file() and not inventory_path.is_file():
        return violations + [Violation(str(inventory_path), "test roots", "missing generated target inventory")]
    if inventory_path.is_file():
        try:
            targets = list(postgres_targets(root))
        except (OSError, ValueError, KeyError, SyntaxError) as error:
            return violations + [Violation(str(inventory_path), "test roots", str(error))]
    else:
        # Source-only fixtures exercise the same selection rules without a
        # generated workspace. A real checkout must supply its inventory.
        grouped = {}
        for subtree in ("src", "tests"):
            for path in sorted(root.joinpath(CRATE, subtree).rglob("*.rs")):
                relative = path.relative_to(root)
                for site in scan_file(relative, without_comments(path.read_text())):
                    if POSTGRES.search(site.reason) or any(POSTGRES.search(f) for f in site.fragments):
                        grouped.setdefault(target_label(relative), []).append(site)
        targets = [({"label": label, "tags": []}, sites, []) for label, sites in grouped.items()]
    if not targets:
        violations.append(Violation(str(CRATE), "scan", "no PostgreSQL laws discovered"))
    for target, ignored_sites, markers in targets:
        label = target["label"]
        candidates = [suite for suite in suites if label in suite.labels]
        hermetic = "hermetic-postgres" in target.get("tags", [])
        if not hermetic and not candidates:
            violations.append(Violation(label, "PostgreSQL leg", f"{label} has PostgreSQL legs ({', '.join(markers)}) but neither hermetic-postgres nor a named PostgreSQL gate"))
        if not hermetic:
            for fragment in target.get("postgres_laws", []):
                ignored = any(fragment in site.fragments for site in ignored_sites)
                if not any(suite.selects(fragment) and not suite.skips_(fragment)
                           and (suite.ignored if ignored else not suite.ignored_only)
                           for suite in candidates):
                    violations.append(Violation(label, "PostgreSQL law", f"{label} law {fragment!r} is selected by no PostgreSQL gate"))
        for site in ignored_sites:
            if not site.fragments:
                violations.append(Violation(site.path, f"line {site.line}", "cannot resolve ignored PostgreSQL marker to a test name"))
            for fragment in site.fragments:
                selected = [suite for suite in candidates if suite.ignored and suite.selects(fragment)]
                if SECOND_SERVICE.search(site.reason):
                    for suite in selected:
                        if not suite.skips_(fragment):
                            violations.append(Violation(site.path, f"line {site.line}", f"{suite.name} selects {fragment!r} needing a second service without a skip="))
                elif not any(not suite.skips_(fragment) for suite in selected):
                    violations.append(Violation(site.path, f"line {site.line}", f"{label} ignored PostgreSQL law {fragment!r} is selected by no PostgreSQL gate"))
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
