#!/usr/bin/env python3
"""Derive the version-bump recreation E2E fixtures from ``SCHEMA_MIGRATIONS``.

``check_version_bumps.py`` proves *guarded shape changed => component constant
bumped*.  Nothing proved the other half: the recreation E2E's fixtures are pinned
to the newest PostgreSQL component generation, so a bump that moves the constant
without moving them leaves a fixture that describes a generation the build no
longer has.  Such a fixture does not merely fail late in a container gate — it
can keep passing on the *wrong* refusal, which is how FIG-1259's older-store
phase came to assert divergence instead of the reject-and-recreate boundary it
exists to prove.

Every fixture constant here is therefore a projection of ``SCHEMA_MIGRATIONS``,
and this check recomputes each projection and demands equality.  It reads sources
only, needs no database, and uses the standard library alone so it can run before
the Rust toolchain is installed.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
VERSION_SOURCE = "crates/lash-postgres-store/src/lib.rs"
MIGRATIONS_SOURCE = "crates/lash-postgres-store/src/postgres/schema.rs"
FIXTURE_SOURCE = "runbooks/restate-postgres-workers/src/bin/version_bump.rs"
GATE_SOURCE = "scripts/version-bump-recreation-e2e.sh"

# Each refusal marker must identify exactly one of the gate's error renderers, so
# a marker constant is bound to the function whose prose it is meant to select.
REFUSAL_MARKERS = (
    ("DIVERGENT_ARTIFACTS_MARKER", "schema_migration_divergence_error"),
    ("SOURCE_MISMATCH_MARKER", "schema_migration_source_mismatch_error"),
    ("NO_APPLICABLE_MIGRATION_MARKER", "version_mismatch_error"),
)

VERSION_CONSTANT = re.compile(r"^const SCHEMA_VERSION: i32 = (\d+);$", re.MULTILINE)
MIGRATIONS_BLOCK = re.compile(
    r"^const SCHEMA_MIGRATIONS: &\[SchemaMigration\] = &\[$(.*?)^\];$",
    re.MULTILINE | re.DOTALL,
)
MIGRATION_ENTRY = re.compile(r"^\s{4}SchemaMigration \{$", re.MULTILINE)
STRING_LITERAL = re.compile(r'"([^"\\]*)"')
LINE_CONTINUATION = re.compile(r"\\\n\s*")
AS_STR_ARM = re.compile(r'^\s*Self::(\w+) => "([a-z_]+)",$', re.MULTILINE)
# Which table each migration DDL puts an index on. `DROP TABLE` takes an index
# with it, so this is what decides whether a post-floor index still needs an
# explicit drop. The shape follows PostgreSQL's grammar:
# `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] name ON [ONLY] table`.
INDEX_TARGET = re.compile(
    r"CREATE (?:UNIQUE )?INDEX (?:CONCURRENTLY )?(?:IF NOT EXISTS )?(\w+)"
    r"\s+ON (?:ONLY )?(\w+)"
)
# Rust line comments, including doc comments. Prose quoting DDL must not be read
# as DDL.
COMMENT_LINE = re.compile(r"^[ \t]*//.*$", re.MULTILINE)
EXPECTED_KINDS_TABLE = re.compile(
    r"^EXPECTED_REFUSAL_KINDS = \{$(.*?)^\}$", re.MULTILINE | re.DOTALL
)
EXPECTED_KINDS_ENTRY = re.compile(r'^\s*"(\w+)": "([a-z_]+)",$', re.MULTILINE)
PRE_CUTOVER_KIND = re.compile(
    r"^const PRE_CUTOVER_REFUSAL_KIND: RefusalKind = RefusalKind::(\w+);$",
    re.MULTILINE,
)


class CheckError(RuntimeError):
    """Source-shape or configuration error: the check could not be decided."""


@dataclass(frozen=True)
class Migration:
    from_version: int
    to_version: int
    source_missing_tables: tuple[str, ...]
    source_missing_columns: tuple[tuple[str, str], ...]
    introduced_relations: tuple[str, ...]


def read_source(repo: Path, relative: str) -> str:
    path = repo / relative
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        raise CheckError(f"cannot read {relative}: {error}") from error


def int_constant(text: str, source: str, pattern: re.Pattern[str], name: str) -> int:
    matches = pattern.findall(text)
    if len(matches) != 1:
        raise CheckError(
            f"{source}: expected exactly one {name} declaration, found {len(matches)}"
        )
    return int(matches[0])


def string_array_constant(text: str, source: str, name: str) -> tuple[str, ...]:
    pattern = re.compile(
        rf"^const {re.escape(name)}: \[&str; (\d+)\] = \[(.*?)\];$",
        re.MULTILINE | re.DOTALL,
    )
    matches = pattern.findall(text)
    if len(matches) != 1:
        raise CheckError(
            f"{source}: expected exactly one {name} declaration, found {len(matches)}"
        )
    declared_length, body = matches[0]
    values = tuple(STRING_LITERAL.findall(body))
    if len(values) != int(declared_length):
        raise CheckError(
            f"{source}: {name} declares {declared_length} entries but lists {len(values)}"
        )
    if len(set(values)) != len(values):
        raise CheckError(f"{source}: {name} lists a duplicate entry")
    return values


def pair_array_constant(text: str, source: str, name: str) -> tuple[tuple[str, str], ...]:
    pattern = re.compile(
        rf"^const {re.escape(name)}: \[\(&str, &str\); (\d+)\] =\s*\[(.*?)\];$",
        re.MULTILINE | re.DOTALL,
    )
    matches = pattern.findall(text)
    if len(matches) != 1:
        raise CheckError(
            f"{source}: expected exactly one {name} declaration, found {len(matches)}"
        )
    declared_length, body = matches[0]
    values = STRING_LITERAL.findall(body)
    if len(values) % 2 != 0:
        raise CheckError(f"{source}: {name} does not list (table, column) pairs")
    pairs = tuple(zip(values[0::2], values[1::2]))
    if len(pairs) != int(declared_length):
        raise CheckError(
            f"{source}: {name} declares {declared_length} entries but lists {len(pairs)}"
        )
    if len(set(pairs)) != len(pairs):
        raise CheckError(f"{source}: {name} lists a duplicate entry")
    return pairs


def string_constant(text: str, source: str, name: str) -> str:
    pattern = re.compile(rf'^const {re.escape(name)}: &str = "(.*)";$', re.MULTILINE)
    matches = pattern.findall(text)
    if len(matches) != 1:
        raise CheckError(
            f"{source}: expected exactly one {name} declaration, found {len(matches)}"
        )
    return matches[0]


def rust_function_body(text: str, source: str, name: str) -> str:
    """The one top-level Rust function of that name, with continuations rendered."""
    pattern = re.compile(
        rf"^(?:pub(?:\([^)]*\))? )?fn {re.escape(name)}\b.*?^\}}$",
        re.MULTILINE | re.DOTALL,
    )
    matches = pattern.findall(text)
    if len(matches) != 1:
        raise CheckError(
            f"{source}: expected exactly one top-level fn {name}, found {len(matches)}"
        )
    return LINE_CONTINUATION.sub("", matches[0])


def refusal_kind_literals(text: str, source: str) -> dict[str, str]:
    """The wire string each `RefusalKind` variant emits, from its `as_str` arms."""
    arms = AS_STR_ARM.findall(text)
    if not arms:
        raise CheckError(f"{source}: RefusalKind::as_str has no recognizable match arms")
    literals = dict(arms)
    if len(literals) != len(arms):
        raise CheckError(f"{source}: RefusalKind::as_str maps a variant twice")
    if len(set(literals.values())) != len(literals):
        raise CheckError(f"{source}: two RefusalKind variants share one wire string")
    return literals


def index_targets(text: str, source: str) -> dict[str, str]:
    """The table each `CREATE INDEX` in the migration DDL indexes.

    Comment lines go first: this file's doc comments quote DDL in prose, and only
    executed statement text may decide which table drop covers an index. A name
    that resolves to more than one table is a hard error rather than a
    last-match-wins lookup — a duplicate whose last occurrence happens to sit on
    a `POST_FLOOR_TABLES` table would silently excuse the index from
    `POST_FLOOR_INDEXES` and red the container gate instead.
    """
    targets: dict[str, set[str]] = {}
    for index, table in INDEX_TARGET.findall(COMMENT_LINE.sub("", text)):
        targets.setdefault(index, set()).add(table)
    ambiguous = sorted(index for index, tables in targets.items() if len(tables) > 1)
    if ambiguous:
        raise CheckError(
            f"{source}: CREATE INDEX names "
            + ", ".join(ambiguous)
            + " over more than one table, so which table drop covers the index is undecidable"
        )
    return {index: tables.pop() for index, tables in targets.items()}


def expected_gate_kinds(text: str, source: str) -> dict[str, str]:
    """The refusal kind the shell gate demands of each checkpoint."""
    table = EXPECTED_KINDS_TABLE.search(text)
    if table is None:
        raise CheckError(f"{source}: EXPECTED_REFUSAL_KINDS is not in the expected shape")
    entries = EXPECTED_KINDS_ENTRY.findall(table.group(1))
    if not entries:
        raise CheckError(f"{source}: EXPECTED_REFUSAL_KINDS lists no checkpoints")
    kinds = dict(entries)
    if len(kinds) != len(entries):
        raise CheckError(f"{source}: EXPECTED_REFUSAL_KINDS names a checkpoint twice")
    return kinds


def string_field(entry: str, field: str) -> tuple[str, ...]:
    match = re.search(rf"{re.escape(field)}: &\[(.*?)\]", entry, re.DOTALL)
    if match is None:
        raise CheckError(f"{MIGRATIONS_SOURCE}: a migration entry has no {field}")
    return tuple(STRING_LITERAL.findall(match.group(1)))


def pair_field(entry: str, field: str) -> tuple[tuple[str, str], ...]:
    """The `(table, column)` literals of a migration's pair-valued field.

    The array carries no nested brackets, so the flat run of string literals
    between the outer ones is exactly the pairs in order. An odd count means the
    entry was written as something other than `(table, column)` tuples, which
    would silently halve the set this derives.
    """
    values = string_field(entry, field)
    if len(values) % 2 != 0:
        raise CheckError(
            f"{MIGRATIONS_SOURCE}: {field} does not list (table, column) pairs"
        )
    return tuple(zip(values[0::2], values[1::2]))


def int_field(entry: str, field: str) -> int:
    match = re.search(rf"^\s*{re.escape(field)}: (\d+),$", entry, re.MULTILINE)
    if match is None:
        raise CheckError(f"{MIGRATIONS_SOURCE}: a migration entry has no {field}")
    return int(match.group(1))


def parse_migrations(text: str) -> tuple[Migration, ...]:
    block = MIGRATIONS_BLOCK.search(text)
    if block is None:
        raise CheckError(f"{MIGRATIONS_SOURCE}: SCHEMA_MIGRATIONS is not in the expected shape")
    body = block.group(1)
    starts = [match.start() for match in MIGRATION_ENTRY.finditer(body)]
    if not starts:
        raise CheckError(f"{MIGRATIONS_SOURCE}: SCHEMA_MIGRATIONS lists no migrations")
    bounds = [*starts, len(body)]
    migrations = tuple(
        Migration(
            from_version=int_field(entry, "from"),
            to_version=int_field(entry, "to"),
            source_missing_tables=string_field(entry, "source_missing_tables"),
            source_missing_columns=pair_field(entry, "source_missing_columns"),
            introduced_relations=string_field(entry, "introduced_relations"),
        )
        for entry in (body[start:end] for start, end in zip(bounds, bounds[1:]))
    )
    froms = [migration.from_version for migration in migrations]
    if len(set(froms)) != len(froms):
        raise CheckError(f"{MIGRATIONS_SOURCE}: two migrations share a source version")
    return migrations


def named_set_failure(constant: str, derivation: str, found: tuple[str, ...], expected: tuple[str, ...]) -> str:
    missing = sorted(set(expected) - set(found))
    extra = sorted(set(found) - set(expected))
    detail = []
    if missing:
        detail.append("missing " + ", ".join(missing))
    if extra:
        detail.append("stale " + ", ".join(extra))
    return (
        f"{FIXTURE_SOURCE}: {constant} is not {derivation}: " + "; ".join(detail)
    )


def check(repo: Path) -> tuple[bool, str]:
    version_text = read_source(repo, VERSION_SOURCE)
    migrations_text = read_source(repo, MIGRATIONS_SOURCE)
    fixture_text = read_source(repo, FIXTURE_SOURCE)
    gate_text = read_source(repo, GATE_SOURCE)

    component_version = int_constant(
        version_text, VERSION_SOURCE, VERSION_CONSTANT, "SCHEMA_VERSION"
    )
    migrations = parse_migrations(migrations_text)
    migration_targets = {migration.to_version for migration in migrations}
    if len(migration_targets) != 1:
        versions = ", ".join(str(version) for version in sorted(migration_targets))
        return False, (
            f"{MIGRATIONS_SOURCE}: SCHEMA_MIGRATIONS targets multiple generations: {versions}"
        )
    migration_target = migration_targets.pop()
    if migration_target not in {component_version, component_version - 1}:
        return False, (
            f"{MIGRATIONS_SOURCE}: SCHEMA_MIGRATIONS targets component {migration_target}, "
            f"which is neither the current component {component_version} nor its retained "
            "pre-cutover generation"
        )
    destructive_cutover = migration_target == component_version - 1
    off_target = [
        migration for migration in migrations if migration.to_version != migration_target
    ]
    if off_target:
        versions = ", ".join(str(migration.to_version) for migration in off_target)
        return False, (
            f"{MIGRATIONS_SOURCE}: SCHEMA_MIGRATIONS targets {versions}, not the selected "
            f"migration generation {migration_target}"
        )

    floor = min(migrations, key=lambda migration: migration.from_version)
    predecessors = [
        migration
        for migration in migrations
        if migration.from_version == migration_target - 1
    ]
    if len(predecessors) != 1:
        return False, (
            f"{MIGRATIONS_SOURCE}: no migration from component {migration_target - 1}. The "
            "historical divergence fixture needs the immediate predecessor of the retained "
            "migration generation"
        )
    predecessor = predecessors[0]

    failures: list[str] = []

    declared_floor = int_constant(
        fixture_text,
        FIXTURE_SOURCE,
        re.compile(r"^const MIGRATION_FLOOR_VERSION: i32 = (\d+);$", re.MULTILINE),
        "MIGRATION_FLOOR_VERSION",
    )
    if declared_floor != floor.from_version:
        failures.append(
            f"{FIXTURE_SOURCE}: MIGRATION_FLOOR_VERSION is {declared_floor}, but the oldest "
            f"migration source in {MIGRATIONS_SOURCE} is {floor.from_version}. The older-store "
            "fixture stamps below the floor; a stale floor stamps a version this build migrates"
        )
    if any(
        migration.from_version == declared_floor - 1 for migration in migrations
    ):
        failures.append(
            f"{FIXTURE_SOURCE}: MIGRATION_FLOOR_VERSION - 1 ({declared_floor - 1}) has an "
            "explicit migration, so the older-store fixture would be migrated instead of refused"
        )

    for constant, expected, derivation in (
        (
            "POST_FLOOR_TABLES",
            floor.source_missing_tables,
            f"the component-{floor.from_version} migration's source_missing_tables",
        ),
        (
            "POST_FLOOR_ARTIFACTS",
            floor.introduced_relations,
            f"the component-{floor.from_version} migration's introduced_relations",
        ),
        (
            "DIVERGENT_ARTIFACTS",
            predecessor.introduced_relations,
            f"the component-{predecessor.from_version} migration's introduced_relations",
        ),
    ):
        found = string_array_constant(fixture_text, FIXTURE_SOURCE, constant)
        if set(found) != set(expected):
            failures.append(named_set_failure(constant, derivation, found, expected))

    # The rewind has a column axis as well as a relation one: a nullable column
    # added to a table the floor catalog already has survives every `DROP TABLE`
    # and every `DROP INDEX`, so without this the "published floor catalog" the
    # fixture reconstructs still carries current-generation columns and the
    # refusal it proves is not the one an older store gets.
    declared_columns = pair_array_constant(
        fixture_text, FIXTURE_SOURCE, "POST_FLOOR_COLUMNS"
    )
    if set(declared_columns) != set(floor.source_missing_columns):
        failures.append(
            named_set_failure(
                "POST_FLOOR_COLUMNS",
                f"the component-{floor.from_version} migration's source_missing_columns",
                tuple(f"{table}.{column}" for table, column in declared_columns),
                tuple(
                    f"{table}.{column}"
                    for table, column in floor.source_missing_columns
                ),
            )
        )

    # `POST_FLOOR_INDEXES` is the one fixture list nothing else can see. The
    # older-store fixture drops the post-floor *tables*, which takes their indexes
    # with them; every other post-floor relation has to be dropped by name or the
    # fixture keeps a current-only artifact and the run reds on
    # `current_artifact_count` — inside a container, after the whole seed phase.
    # Derive that remainder from the migration DDL rather than trusting a hand-kept
    # list, and reject names the table drops already cover: those would be a
    # `DROP INDEX` of a relation that no longer exists.
    indexed_table = index_targets(migrations_text, MIGRATIONS_SOURCE)
    post_floor_tables = set(floor.source_missing_tables)
    left_behind = tuple(
        relation
        for relation in floor.introduced_relations
        if relation not in post_floor_tables
        and indexed_table.get(relation) not in post_floor_tables
    )
    declared_indexes = string_array_constant(
        fixture_text, FIXTURE_SOURCE, "POST_FLOOR_INDEXES"
    )
    if set(declared_indexes) != set(left_behind):
        failures.append(
            named_set_failure(
                "POST_FLOOR_INDEXES",
                f"the component-{floor.from_version} migration's introduced_relations that "
                "dropping POST_FLOOR_TABLES leaves behind (a relation survives unless it is "
                "one of those tables or its CREATE INDEX in "
                f"{MIGRATIONS_SOURCE} names one)",
                declared_indexes,
                left_behind,
            )
        )

    # The refusal classifier decides which claim a refusal supports, and it does so
    # by selecting one of the gate's three error renderers. Presence alone is not
    # enough: a marker that also appears in a sibling's prose makes two kinds match
    # at once, which the classifier rejects at run time — after a container. Each
    # marker must therefore select exactly its own renderer and no other. The
    # renderers wrap with Rust line continuations, which the rendered string does
    # not contain.
    renderers = {
        function: rust_function_body(migrations_text, MIGRATIONS_SOURCE, function)
        for _, function in REFUSAL_MARKERS
    }
    for marker_constant, owner in REFUSAL_MARKERS:
        marker = string_constant(fixture_text, FIXTURE_SOURCE, marker_constant)
        carriers = sorted(
            function for function, body in renderers.items() if marker in body
        )
        if carriers == [owner]:
            continue
        if not carriers:
            failures.append(
                f"{FIXTURE_SOURCE}: {marker_constant} ({marker!r}) no longer appears in "
                f"{MIGRATIONS_SOURCE}'s {owner}, so the refusal kind it identifies cannot "
                "be classified"
            )
        else:
            failures.append(
                f"{FIXTURE_SOURCE}: {marker_constant} ({marker!r}) must select only "
                f"{owner}, but {MIGRATIONS_SOURCE} carries it in "
                + ", ".join(carriers)
                + ". Overlapping markers make the harness match more than one refusal "
                "kind and fail mid-run"
            )

    # The kind strings the shell gate demands are the harness's own `as_str`
    # literals. Renaming either side without the other is otherwise only visible
    # once a live run emits a kind no assertion recognizes.
    emitted = refusal_kind_literals(fixture_text, FIXTURE_SOURCE)
    demanded = expected_gate_kinds(gate_text, GATE_SOURCE)
    unknown = sorted(set(demanded.values()) - set(emitted.values()))
    if unknown:
        failures.append(
            f"{GATE_SOURCE}: EXPECTED_REFUSAL_KINDS demands "
            + ", ".join(repr(kind) for kind in unknown)
            + f", which no RefusalKind variant in {FIXTURE_SOURCE} emits (it emits "
            + ", ".join(repr(kind) for kind in sorted(emitted.values()))
            + ")"
        )
    pre_cutover_matches = PRE_CUTOVER_KIND.findall(fixture_text)
    if len(pre_cutover_matches) != 1:
        raise CheckError(
            f"{FIXTURE_SOURCE}: expected exactly one PRE_CUTOVER_REFUSAL_KIND declaration, "
            f"found {len(pre_cutover_matches)}"
        )
    pre_cutover_variant = pre_cutover_matches[0]
    if pre_cutover_variant not in emitted:
        failures.append(
            f"{FIXTURE_SOURCE}: PRE_CUTOVER_REFUSAL_KIND names unknown variant "
            f"{pre_cutover_variant}"
        )
    else:
        configured_kind = emitted[pre_cutover_variant]
        expected_kind = (
            "no_applicable_migration" if destructive_cutover else "divergent_artifacts"
        )
        if configured_kind != expected_kind:
            failures.append(
                f"{FIXTURE_SOURCE}: PRE_CUTOVER_REFUSAL_KIND emits {configured_kind!r}, but "
                f"component {component_version} requires {expected_kind!r}"
            )
        gate_kind = demanded.get("refused_divergent_store")
        if gate_kind != configured_kind:
            failures.append(
                f"{GATE_SOURCE}: refused_divergent_store demands {gate_kind!r}, but the "
                f"harness pre-cutover refusal emits {configured_kind!r}"
            )

    if failures:
        return False, "\n".join(
            [
                "version-bump recreation fixtures are stale against the current component "
                f"generation ({component_version}):",
                *(f"- {failure}" for failure in failures),
            ]
        )
    return True, (
        f"version-bump fixture check passed: component {component_version}, floor "
        f"{floor.from_version}, {len(migrations)} explicit migrations, "
        f"{len(renderers)} disjoint refusal kinds, {len(declared_indexes)} explicitly "
        f"dropped post-floor indexes, {len(declared_columns)} explicitly dropped "
        f"post-floor columns, {len(demanded)} asserted checkpoints"
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    try:
        valid, message = check(parse_args(argv).repo)
    except CheckError as error:
        print(f"version-bump fixture check could not run: {error}", file=sys.stderr)
        return 2
    print(message, file=sys.stdout if valid else sys.stderr)
    return 0 if valid else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
