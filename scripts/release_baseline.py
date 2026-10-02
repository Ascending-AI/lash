#!/usr/bin/env python3
"""Inventory source-declared constants, check the 1.0 baseline, hold a
compiled operator's versions to the inventory, or write the value tables
generated from the inventory.

The resolver accepts literal counters, string identities and local constant
aliases with integer addition/subtraction. It evaluates cfg(feature =
"synthetic-next") in both tiers. Unsupported expressions, missing definitions,
duplicate active definitions and unregistered versions fail closed.
"""

from __future__ import annotations

import argparse
import ast
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
REGISTRY = Path("scripts/versioned-surfaces.toml")
CONST = re.compile(
    r"^[ \t]*(?P<attrs>(?:#\[[^\]]*\]\s*)*)"
    r"(?:pub(?:\([^)]*\))?\s+)?const\s+(?P<name>[A-Z][A-Z0-9_]*)"
    r"\s*:\s*[^=;]+?=\s*(?P<value>[^;]+);", re.MULTILINE,
)
LEXEME = re.compile(r'r(?P<hashes>\#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"|//[^\n]*|/\*', re.DOTALL)


class BaselineError(Exception):
    """The source or declaration cannot establish the baseline."""


def without_comments(text: str) -> str:
    # Preserve offsets for the reset planner, including comments between attrs.
    result = list(text)
    at = 0
    while match := LEXEME.search(text, at):
        at = match.end()
        if match.group().startswith("/*"):
            depth = 1
            while depth:
                token = re.search(r"/\*|\*/", text[at:])
                if token is None:
                    raise BaselineError("unterminated Rust block comment")
                at += token.end()
                depth += 1 if token.group() == "/*" else -1
        elif not match.group().startswith("//"):
            continue
        for index in range(match.start(), at):
            if result[index] != "\n":
                result[index] = " "
    return "".join(result)


def enabled(attrs: str, synthetic: bool) -> bool:
    for condition in re.findall(r"#\[cfg\((.*?)\)\]", attrs, re.DOTALL):
        condition = re.sub(r"\s+", "", condition)
        if condition == 'feature="synthetic-next"':
            active = synthetic
        elif condition == 'not(feature="synthetic-next")':
            active = not synthetic
        elif condition == "test":
            active = False
        elif condition == "not(test)":
            active = True
        else:
            raise BaselineError(f"unsupported cfg({condition})")
        if not active:
            return False
    if "cfg_attr" in attrs:
        raise BaselineError("cfg_attr on a version constant is unsupported")
    return True


def definitions(text: str):
    return list(CONST.finditer(without_comments(text)))


def resolve(text: str, name: str, synthetic: bool):
    constants = {}
    for match in definitions(text):
        constants.setdefault(match["name"], []).append(match)

    def value(symbol: str, trail: tuple[str, ...]):
        if symbol in trail:
            raise BaselineError(f"cyclic constant alias: {' -> '.join((*trail, symbol))}")
        candidates = [m for m in constants.get(symbol, []) if enabled(m["attrs"], synthetic)]
        if len(candidates) != 1:
            raise BaselineError(f"{symbol}: expected one active definition, found {len(candidates)}")
        expression = candidates[0]["value"].strip()
        expression = re.sub(r"\b(\d[\d_]*)(?:u(?:8|16|32|64|128|size)|i(?:8|16|32|64|128|size))\b", r"\1", expression)
        try:
            node = ast.parse(expression, mode="eval").body
        except SyntaxError as error:
            raise BaselineError(f"{symbol}: unsupported expression {expression!r}") from error

        def evaluate(node):
            if isinstance(node, ast.Constant) and type(node.value) in (int, str):
                return node.value
            if isinstance(node, ast.Name):
                return value(node.id, (*trail, symbol))
            if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Sub)):
                left, right = evaluate(node.left), evaluate(node.right)
                if type(left) is int and type(right) is int:
                    return left + right if isinstance(node.op, ast.Add) else left - right
            raise BaselineError(f"{symbol}: unsupported expression {expression!r}")

        return evaluate(node)

    return value(name, ())


def surfaces(repo: Path):
    import check_version_bumps as gate
    from discover_version_surfaces import discover
    try:
        discovered, _ = discover(gate.WorktreeView(repo))
    except gate.CheckError as error:
        raise BaselineError(str(error)) from error
    return discovered


def inventory(repo: Path):
    rows = []
    for surface in surfaces(repo):
        key = f'{surface["constant_path"]}:{surface["constant"]}'
        try:
            text = (repo / surface["constant_path"]).read_text()
            default = resolve(text, surface["constant"], False)
            synthetic = resolve(text, surface["constant"], True)
        except BaselineError as error:
            raise BaselineError(f"{key}: {error}") from error
        rows.append({"key": key, "default": default, "synthetic": synthetic,
                     "upgrade": surface["upgrade"], "manifest": surface.get("manifest")})
    return rows


def baseline_of(value):
    """Every counter starts at 1; a string retains its prefix and suffix."""
    if type(value) is int:
        return 1
    if type(value) is str and re.search(r'[/:-]v[0-9]+(?=[/:-]|$)', value):
        return re.sub(r'([/:-])v[0-9]+(?=[/:-]|$)', r'\1v1', value)
    raise BaselineError(f"{value!r} is neither a counter nor a versioned string identity")


def mismatches(rows: list[dict]):
    errors = []
    for row in sorted(rows, key=lambda row: row["key"]):
        actual = row["default"]
        expected = baseline_of(actual)
        if actual != expected:
            errors.append(f'{row["key"]}: default {actual!r}, release baseline {expected!r}')
    return errors


STORE_VERSIONS = Path("crates/lash-core-store/src/compat.rs")
SQLITE_SCHEMA = Path("crates/lash-sqlite-store/src/schema.rs")
SQLITE_CATALOG = Path("crates/lash-sqlite-store/src/migration.rs")
POSTGRES_ALIAS = Path("crates/lash-postgres-store/src/lib.rs")
POSTGRES_CATALOG = Path("crates/lash-postgres-store/src/postgres/migrate.rs")
POSTGRES_SCHEMA = Path("crates/lash-postgres-store/schema.sql")
CFG = r'(?P<attrs>(?:#\[cfg\([^\]]*\)\]\s*)*)'
STORE_DESCRIPTOR = re.compile(
    r"\bstore\(\s*ComponentId::(?P<component>\w+)\s*,\s*(?P<constant>[A-Z][A-Z0-9_]*)\s*,?\s*\)"
)
DATABASE_COMPONENT = re.compile(r"Self::(\w+)\s*=>\s*ComponentId::(\w+)")
SQLITE_STEP = re.compile(
    CFG + r"SqliteMigration\s*\{\s*database:\s*SqliteDatabase::(?P<database>\w+),"
    r"\s*from:\s*(?P<from>[^,{}]+),\s*to:\s*(?P<to>[^,{}]+),"
)
EXPAND_TABLE = re.compile(
    r"static\s+EXPAND_MIGRATIONS\s*:[^=]*?=\s*&\[(?P<body>.*?)\];", re.DOTALL,
)
EXPAND_STEP = re.compile(
    CFG + r"ExpandMigration\s*\{\s*id:\s*\"[^\"]*\",\s*from_version:\s*(?P<from>[^,{}]+),"
    r"\s*to_version:\s*(?P<to>[^,{}]+),"
)
POSTGRES_ALIAS_DEFINITION = re.compile(
    r"const\s+SCHEMA_VERSION\s*:\s*i32\s*=\s*lash_core_execution::compat::"
    r"(?P<constant>[A-Z][A-Z0-9_]*)\s+as\s+i32\s*;"
)
POSTGRES_HEADER = re.compile(r"\A-- lash-postgres-store schema, component version (\d+)\.\n")
POSTGRES_SEED = re.compile(r"VALUES \('lash-postgres-store', (\d+), (\d+)\)")
STEP_BOUND = re.compile(r"(?:(?:\w+::)*(?P<name>[A-Z][A-Z0-9_]*)(?:\s*\+\s*(?P<add>\d+))?|(?P<literal>\d+))\Z")


def store_versions(repo: Path):
    """Each store component's schema-version constant and its value.

    The constants live in compat.rs, where `DESCRIPTORS` hands each one to
    `store(..)`: the component's descriptor, the stamp a store carries and the
    numbers its catalog steps are written in are that one constant. The
    default build writes it and the synthetic-next build writes the version
    after it, so a constant has one definition for both tiers.
    """
    text = (repo / STORE_VERSIONS).read_text()
    rows = {m["component"]: m["constant"] for m in STORE_DESCRIPTOR.finditer(without_comments(text))}
    if not rows:
        raise BaselineError("cannot read the store descriptors and their schema-version constants")
    versions = {}
    for component, constant in rows.items():
        default, synthetic = resolve(text, constant, False), resolve(text, constant, True)
        if type(default) is not int or default != synthetic:
            raise BaselineError(f"{constant}: a store schema version is one integer in both tiers")
        versions[component] = (constant, default)
    return versions


def step_bound(expression: str, names: dict[str, int], where: str):
    """A catalog step's `from` or `to`: an integer, or the store's own
    schema-version constant with an optional `+ N`."""
    match = STEP_BOUND.match(expression.strip())
    if match is None or (match["name"] and match["name"] not in names):
        raise BaselineError(f"{where}: step bound {expression.strip()!r} is not the store's own version")
    if match["literal"]:
        return int(match["literal"])
    return names[match["name"]] + int(match["add"] or 0)


def catalog_mismatches(catalog: Path, label: str, constant: str, value: int, rows):
    """Where a catalog's steps leave the range of the version they are numbered in.

    `rows` are `(attrs, from, to)`. A default-build step lies inside
    `[1, value]`; the synthetic-next build writes `value + 1`, and its steps
    must chain there from `value`.
    """
    errors = []
    for synthetic in (False, True):
        tier = "synthetic-next" if synthetic else "default"
        written = value + (1 if synthetic else 0)
        for attrs, start, end in rows:
            if enabled(attrs, synthetic) and not 1 <= start < end <= written:
                errors.append(
                    f"{catalog}: {label} step {start} to {end} is outside "
                    f"the {tier} stamp {constant} = {written}"
                )
    steps = {(start, end) for attrs, start, end in rows if enabled(attrs, True)}
    at = value
    while at < value + 1 and any(start == at for start, _ in steps):
        at = max(end for start, end in steps if start == at)
    if at != value + 1:
        errors.append(
            f"{catalog}: {label} has no step chain from the default stamp "
            f"{value} to the synthetic-next stamp {value + 1} ({constant})"
        )
    return errors


def sqlite_stamp_mismatches(repo: Path):
    """Where a SQLite database's catalog and its schema version count differently.

    Each database's `lash_compat` row, its descriptor and its catalog steps
    are in the one constant compat.rs declares for it. The backend must not
    restate that number, every catalog step must lie inside the version's own
    range, and the synthetic-next build's steps must carry the database from
    the constant to the version after it.
    """
    schema = (repo / SQLITE_SCHEMA).read_text()
    components = dict(DATABASE_COMPONENT.findall(without_comments(schema)))
    versions = store_versions(repo)
    if not components or set(components.values()) - versions.keys():
        raise BaselineError("cannot read the SQLite databases and their components")
    errors = [
        f"{SQLITE_SCHEMA}:{match['name']}: a SQLite schema version is restated outside {STORE_VERSIONS}"
        for match in definitions(schema) if match["name"].endswith("SCHEMA_VERSION")
    ]
    catalog = without_comments((repo / SQLITE_CATALOG).read_text())
    for database, component in sorted(components.items()):
        constant, value = versions[component]
        where = f"{SQLITE_CATALOG}: {database}"
        rows = [
            (row["attrs"], step_bound(row["from"], {constant: value}, where),
             step_bound(row["to"], {constant: value}, where))
            for row in SQLITE_STEP.finditer(catalog) if row["database"] == database
        ]
        errors += catalog_mismatches(SQLITE_CATALOG, database, constant, value, rows)
    return errors


def postgres_stamp_mismatches(repo: Path):
    """Where the PostgreSQL schema, its catalog and its schema version count differently.

    The stamp `schema.sql` seeds, the header it carries, the descriptor and
    the expand catalog's steps are in the one constant compat.rs declares for
    the POSTGRES component. The backend names it only through its `i32`
    alias; the artifact's header and seed row must state it; every catalog
    step must lie inside the version's own range; and the synthetic-next
    build's steps must carry a store from the constant to the version after
    it.
    """
    constant, value = store_versions(repo)["POSTGRES"]
    aliases = POSTGRES_ALIAS_DEFINITION.findall(without_comments((repo / POSTGRES_ALIAS).read_text()))
    catalog = without_comments((repo / POSTGRES_CATALOG).read_text())
    table = EXPAND_TABLE.search(catalog)
    steps = list(EXPAND_STEP.finditer(table["body"])) if table else []
    declared = len(re.findall(r"ExpandMigration\s*\{", table["body"])) if table else -1
    if len(steps) != declared:
        raise BaselineError("cannot read the PostgreSQL expand catalog")
    errors = []
    if aliases != [constant]:
        errors.append(
            f"{POSTGRES_ALIAS}:SCHEMA_VERSION: must be the i32 alias of {STORE_VERSIONS}:{constant}"
        )
    artifact = (repo / POSTGRES_SCHEMA).read_text()
    header, seeds = POSTGRES_HEADER.match(artifact), POSTGRES_SEED.findall(artifact)
    if header is None or len(seeds) != 1:
        raise BaselineError(f"cannot read the component version {POSTGRES_SCHEMA} states")
    stated = (int(header[1]), int(seeds[0][0]), int(seeds[0][1]))
    if stated != (value, value, value):
        errors.append(
            f"{POSTGRES_SCHEMA}: header {stated[0]} and seed stamp {stated[1]}/{stated[2]}, "
            f"but POSTGRES is at {constant} = {value}"
        )
    names = {constant: value, "SCHEMA_VERSION": value}
    rows = [
        (row["attrs"], step_bound(row["from"], names, str(POSTGRES_CATALOG)),
         step_bound(row["to"], names, str(POSTGRES_CATALOG)))
        for row in steps
    ]
    return errors + catalog_mismatches(POSTGRES_CATALOG, "expand", constant, value, rows)


CATALOG_ROW = re.compile(
    r"(?m)^[ \t]*" + CFG + r"(?P<kind>ExpandMigration|BackfillMigration|ContractMigration|SqliteMigration)\s*\{"
)


def production_catalog_mismatches(repo: Path):
    """Each step a production catalog carries.

    1.0 provisions every store fresh at its baseline, so the default build's
    PostgreSQL expand, backfill and contract catalogs and its SQLite catalog
    carry no step: any step there moves a store from a pre-1.0 version. Only
    the synthetic-next build's adjacent steps remain, under their cfg.
    """
    errors = []
    for path in (POSTGRES_CATALOG, SQLITE_CATALOG):
        text = without_comments((repo / path).read_text())
        rows = list(CATALOG_ROW.finditer(text))
        if not rows:
            raise BaselineError(f"cannot read the catalog rows of {path}")
        for row in rows:
            if enabled(row["attrs"], False):
                line = text.count("\n", 0, row.start("kind")) + 1
                errors.append(f"{path}:{line}: the production catalog carries a {row['kind']} step; "
                              "1.0 carries no pre-1.0 predecessor step")
    return errors


def postgres_schema_at(text: str, version: int):
    """`schema.sql` restated at `version`: its header and its seed row's stamp."""
    header, seed = POSTGRES_HEADER.match(text), POSTGRES_SEED.search(text)
    if header is None or len(POSTGRES_SEED.findall(text)) != 1:
        raise BaselineError(f"cannot read the component version {POSTGRES_SCHEMA} states")
    for start, end in sorted([seed.span(2), seed.span(1), header.span(1)], reverse=True):
        text = text[:start] + str(version) + text[end:]
    return text


BLAKE3_DOMAIN_TABLE = Path("crates/lash-sansio/src/blake3_domains.rs")
PREDECESSOR_TABLE = Path("crates/lash-core-store/src/store/synthetic_next_versions.rs")
GENERATED = "// @generated by `python3 scripts/release_baseline.py tables --write`; do not edit.\n"
BLAKE3_DOMAIN = re.compile(r"lash[^/]+/v[0-9]+")


def retired_hash_domains(repo: Path):
    """The registry's `[[retired_hash_domain]]` names: reserved, never hashed again."""
    rows = tomllib.loads((repo / REGISTRY).read_text()).get("retired_hash_domain", [])
    names = [row.get("name") for row in rows]
    if (len(set(names)) != len(names)
            or not all(isinstance(name, str) and BLAKE3_DOMAIN.fullmatch(name) for name in names)
            or not all(isinstance(row.get("reason"), str) and row["reason"].strip() for row in rows)):
        raise BaselineError("each [[retired_hash_domain]] needs a unique `lash*/vN` name and a reason")
    return names


def floors(repo: Path):
    """`(path, floor, surface)` for each `[[unregistered]]` admission floor
    the registry ties to a surface in the same file with `floor_of`."""
    rows = tomllib.loads((repo / REGISTRY).read_text()).get("unregistered", [])
    return [(row["constant_path"], row["constant"], row["floor_of"]) for row in rows if "floor_of" in row]


def hash_domains(rows: list[dict]):
    """The BLAKE3 domains in use: the inventory's `lash*/vN` identities."""
    return sorted({row["default"] for row in rows
                   if type(row["default"]) is str and BLAKE3_DOMAIN.fullmatch(row["default"])})


def generated_tables(rows: list[dict], retired: list[str]):
    """The value tables written from the inventory, as `{path: text}`.

    A crate below a surface's owner cannot name the owner's constant, and a
    reservation table lists what the constants say. Each such table is
    generated from `rows` and the registry's `retired` hash domains, so
    resetting the constants resets the tables.
    """
    active, retired = hash_domains(rows), sorted(retired)
    reused = sorted(set(active) & set(retired))
    if reused:
        raise BaselineError(f"retired hash domains are in use again: {', '.join(reused)}")

    def strings(values):
        return "".join(f"    {json.dumps(value)},\n" for value in values)

    domains = (
        "//! The BLAKE3 domains of the workspace's hash owners: the `lash*/vN`\n"
        "//! identity constants the release inventory resolves, and the\n"
        "//! `[[retired_hash_domain]]` rows of `scripts/versioned-surfaces.toml`.\n"
        + GENERATED + "\n"
        "/// Reserved BLAKE3 domains, current and retired: a retired domain stays\n"
        "/// reserved so it cannot be silently reused.\n"
        '/// version_reservations = "hash-domain reservations generated from the release inventory, including retired names"\n'
        "pub(crate) const BLAKE3_DOMAINS: &[&str] = &[\n" + strings(sorted({*active, *retired})) + "];\n\n"
        "/// Permanently reserved, but no longer used.\n"
        '/// version_reservations = "retired hash-domain names generated from the registry"\n'
        "#[cfg(test)]\n"
        "pub(crate) const RETIRED_BLAKE3_DOMAINS: &[&str] = &[\n" + strings(retired) + "];\n"
    )

    moved = {}
    for row in rows:
        name = row["key"].rsplit(":", 1)[1]
        if type(row["default"]) is not int or row["synthetic"] != row["default"] + 1:
            continue
        if moved.setdefault(name, row["default"]) != row["default"]:
            raise BaselineError(f"{name}: two surfaces of one name are at different versions")
    predecessors = (
        "//! The version N writes of every surface the synthetic N+1 moves, by the\n"
        "//! name the surface registers under: the release inventory's default-build\n"
        "//! value of each constant whose synthetic-next value is one more.\n"
        + GENERATED + "\n"
        "pub(super) const PREDECESSOR_WRITES: &[(&str, u32)] = &[\n"
        + "".join(f"    ({json.dumps(name)}, {value}),\n" for name, value in sorted(moved.items()))
        + "];\n"
    )
    return {BLAKE3_DOMAIN_TABLE: domains, PREDECESSOR_TABLE: predecessors}


def table_mismatches(repo: Path, rows: list[dict]):
    """The generated tables that differ from what the inventory generates."""
    errors = []
    for path, text in generated_tables(rows, retired_hash_domains(repo)).items():
        target = repo / path
        if not target.is_file() or target.read_text() != text:
            errors.append(f"{path}: stale generated table; run `python3 scripts/release_baseline.py tables --write`")
    return errors


def verify_build(rows: list[dict], report: Path, synthetic: bool):
    text = report.read_text()
    marker = "release-inventory-build="
    line = next((line.split(marker, 1)[1] for line in text.splitlines() if marker in line), None)
    if line is None:
        raise BaselineError("build probe did not execute or emitted no inventory")
    built = json.loads(line)
    tier = "synthetic" if synthetic else "default"
    expected = {row["key"].rsplit(":", 1)[1]: row[tier] for row in rows if row["manifest"]}
    actual = {row["constant"]: row["value"] for row in built["formats"]}
    if actual != expected or len(actual) != len(built["formats"]):
        raise BaselineError(f"{tier} durable_formats differs from source: expected={expected}, actual={actual}")
    for name, wire in (("REMOTE_PROTOCOL_VERSION", "remote_protocol"), ("RESTATE_WIRE_VERSION", "restate")):
        value = next(row[tier] for row in rows if row["key"].endswith(":" + name))
        bound = "min" if wire == "remote_protocol" else "max"
        if built["version"]["wires"][wire][bound] != value:
            raise BaselineError(f"lashctl version's {wire} differs from source")
    print(f"build probe agrees on {len(actual)} durable formats and 2 lashctl wires", file=sys.stderr)


PROBE_TARGET = "//crates/lashctl:lashctl__unit_test"
PROBE_LAW = "tests::release_inventory_build_probe"


def probe(repo: Path, rows: list[dict]):
    """Build the operator in the default and synthetic-next tiers, run its
    build probe, and hold the versions each build reports to the inventory."""
    from release_cut_laws import synthetic_variant
    for synthetic, label in ((False, PROBE_TARGET), (True, synthetic_variant(repo, PROBE_TARGET))):
        if label is None:
            raise BaselineError(f"{PROBE_TARGET} has no synthetic-next variant")
        output = repo / ".buck2/release-probe" / label.rsplit(":", 1)[1]
        shutil.rmtree(output, ignore_errors=True)
        subprocess.run(["kiln", "test", label, "--no-test-cache", "--test_arg=--exact",
                        f"--test_arg={PROBE_LAW}", "--test_arg=--nocapture",
                        "--test-output-dir", str(output)], cwd=repo, check=True)
        logs = list(output.rglob("test.log"))
        if len(logs) != 1:
            raise BaselineError(f"{label}: expected one probe log under {output}, found {len(logs)}")
        verify_build(rows, logs[0], synthetic)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT)
    commands = parser.add_subparsers(dest="command", required=True)
    inventory_command = commands.add_parser("inventory")
    inventory_command.add_argument("--build-report", type=Path)
    inventory_command.add_argument("--synthetic", action="store_true")
    commands.add_parser("check")
    commands.add_parser("probe")
    tables_command = commands.add_parser("tables")
    tables_command.add_argument("--write", action="store_true")
    args = parser.parse_args()
    try:
        rows = inventory(args.repo)
        if args.command == "tables":
            if args.write:
                for path, text in generated_tables(rows, retired_hash_domains(args.repo)).items():
                    (args.repo / path).write_text(text)
            errors = table_mismatches(args.repo, rows)
            if errors:
                print("\n".join(errors), file=sys.stderr)
            return 1 if errors else 0
        if args.command == "probe":
            probe(args.repo, rows)
            return 0
        if args.command == "inventory":
            if args.build_report is not None:
                verify_build(rows, args.build_report, args.synthetic)
            print(json.dumps(rows, indent=2))
            return 0
        errors = (mismatches(rows) + sqlite_stamp_mismatches(args.repo)
                  + postgres_stamp_mismatches(args.repo) + production_catalog_mismatches(args.repo)
                  + table_mismatches(args.repo, rows))
        if errors:
            print("\n".join(errors), file=sys.stderr)
            return 1
        print(
            f"release baseline: {len(rows)} surfaces, zero mismatches; "
            "SQLite and PostgreSQL catalogs and artifacts are in their compat.rs versions; "
            "production catalogs carry no step; "
            "generated tables are current"
        )
        return 0
    except (BaselineError, OSError, KeyError, tomllib.TOMLDecodeError, json.JSONDecodeError,
            subprocess.CalledProcessError) as error:
        print(f"release baseline error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
