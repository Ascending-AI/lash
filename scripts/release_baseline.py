#!/usr/bin/env python3
"""Inventory registered source constants, or check them against the release baseline.

The baseline is not a table: `baseline_of` states the one rule, and the
surfaces it applies to are the registry's (`scripts/versioned-surfaces.toml`).

The resolver accepts literal counters, string identities and local constant
aliases with integer addition/subtraction. It evaluates cfg(feature =
"synthetic-next") in both tiers. Unsupported expressions, missing definitions
and duplicate active definitions fail closed.
"""

from __future__ import annotations

import argparse
import ast
import json
from pathlib import Path
import re
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
    rows = tomllib.loads((repo / REGISTRY).read_text())["surface"]
    keys = [f'{row["constant_path"]}:{row["constant"]}' for row in rows]
    if not rows or len(set(keys)) != len(keys):
        raise BaselineError("surface registry is empty or contains duplicate keys")
    return rows


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
    """The release baseline of a surface whose default build holds `value`.

    Every counter starts at 1. A string identity keeps its prefix and ends in
    version 1.
    """
    if type(value) is int:
        return 1
    if type(value) is str and re.search(r"v\d+$", value):
        return re.sub(r"v\d+$", "v1", value)
    raise BaselineError(f"{value!r} is neither a counter nor a string identity ending in a version")


def mismatches(rows: list[dict]):
    errors = []
    for row in sorted(rows, key=lambda row: row["key"]):
        actual = row["default"]
        try:
            expected = baseline_of(actual)
        except BaselineError as error:
            errors.append(f'{row["key"]}: {error}')
            continue
        if actual != expected:
            errors.append(f'{row["key"]}: default {actual!r}, release baseline {expected!r}')
    return errors


SQLITE_STAMPS = Path("crates/lash-sqlite-store/src/schema.rs")
SQLITE_CATALOG = Path("crates/lash-sqlite-store/src/migration.rs")
COMPAT_DESCRIPTORS = Path("crates/lash-core-store/src/compat.rs")
CFG = r'(?P<attrs>(?:#\[cfg\([^\]]*\)\]\s*)*)'
STAMP_DATABASE = re.compile(
    r"version_guard\((?:(?!version_guard\().)*?rows\s*=\s*\"SqliteDatabase::(?P<database>\w+)\""
    r"(?:(?!version_guard\().)*?const\s+(?P<stamp>[A-Z][A-Z0-9_]*)\s*:", re.DOTALL,
)
DATABASE_COMPONENT = re.compile(r"Self::(\w+)\s*=>\s*ComponentId::(\w+)")
DESCRIPTOR = re.compile(
    CFG + r"CompatDescriptor\s*\{\s*component:\s*ComponentId::(?P<component>\w+),"
    r"\s*reads:[^,]*(?:\([^)]*\))?,\s*writes:\s*VersionRange::(?P<range>exactly|between)"
    r"\((?P<bounds>[^)]*)\)"
)
CATALOG_ROW = re.compile(
    CFG + r"SqliteMigration\s*\{\s*database:\s*SqliteDatabase::(?P<database>\w+),"
    r"\s*from:\s*(?P<from>\d+),\s*to:\s*(?P<to>\d+),"
)


def sqlite_stamp_mismatches(repo: Path):
    """Where a SQLite schema stamp and its migration catalog count differently.

    The catalog's steps, and the `lash_compat` row a store carries, are in the
    compatibility descriptor's numbers. A stamp's bump owes a catalog step
    from its old value to its new one (check_version_bumps.py), so that chain
    can only be read when the stamp is the descriptor's version. After the
    reset each stamp must therefore equal the version its database's
    descriptor writes, in the default and the synthetic-next build, and every
    catalog step must lie inside the stamp's own range.
    """
    schema = (repo / SQLITE_STAMPS).read_text()
    stamps = {m["stamp"]: m["database"] for m in STAMP_DATABASE.finditer(schema)}
    components = dict(DATABASE_COMPONENT.findall(without_comments(schema)))
    descriptors = without_comments((repo / COMPAT_DESCRIPTORS).read_text())
    catalog = without_comments((repo / SQLITE_CATALOG).read_text())
    if not stamps or set(stamps.values()) - components.keys():
        raise BaselineError("cannot read the SQLite stamps and their databases")
    errors = []
    for stamp, database in sorted(stamps.items()):
        values = {}
        for synthetic in (False, True):
            tier = "synthetic-next" if synthetic else "default"
            written = [
                int(m["bounds"].split(",")[-1])
                for m in DESCRIPTOR.finditer(descriptors)
                if m["component"] == components[database] and enabled(m["attrs"], synthetic)
            ]
            if len(written) != 1:
                raise BaselineError(f"{database}: expected one {tier} compat descriptor, found {len(written)}")
            values[synthetic] = value = resolve(schema, stamp, synthetic)
            if value != written[0]:
                errors.append(
                    f"{SQLITE_STAMPS}:{stamp}: {tier} stamp {value}, but the "
                    f"{components[database]} descriptor and its catalog write {written[0]}"
                )
            for row in CATALOG_ROW.finditer(catalog):
                if row["database"] != database or not enabled(row["attrs"], synthetic):
                    continue
                start, end = int(row["from"]), int(row["to"])
                if not 1 <= start < end <= value:
                    errors.append(
                        f"{SQLITE_CATALOG}: {database} step {start} to {end} is outside "
                        f"the {tier} stamp {stamp} = {value}"
                    )
        steps = {
            (int(row["from"]), int(row["to"]))
            for row in CATALOG_ROW.finditer(catalog)
            if row["database"] == database and enabled(row["attrs"], True)
        }
        at = values[False]
        while at < values[True] and any(start == at for start, _ in steps):
            at = max(end for start, end in steps if start == at)
        if at != values[True]:
            errors.append(
                f"{SQLITE_CATALOG}: {database} has no step chain from the default stamp "
                f"{values[False]} to the synthetic-next stamp {values[True]} ({stamp})"
            )
    return errors


POSTGRES_STAMPS = Path("crates/lash-postgres-store/src/lib.rs")
POSTGRES_CATALOG = Path("crates/lash-postgres-store/src/postgres/migrate.rs")
STAMP_POSTGRES = re.compile(
    r"version_guard\((?:(?!version_guard\().)*?"
    r"catalog\(\s*path\s*=\s*\"(?P<path>[^\"]+)\"\s*,\s*(?P<table>[A-Z][A-Z0-9_]*)\s*\)"
    r"(?:(?!version_guard\().)*?const\s+(?P<stamp>[A-Z][A-Z0-9_]*)\s*:", re.DOTALL,
)
POSTGRES_COMPONENT = re.compile(
    r"descriptor\(\s*lash_core_execution::compat::ComponentId::(?P<component>\w+)"
)
EXPAND_TABLE = re.compile(
    r"static\s+EXPAND_MIGRATIONS\s*:[^=]*?=\s*&\[(?P<body>.*?)\];", re.DOTALL,
)
EXPAND_STEP = re.compile(
    CFG + r"ExpandMigration\s*\{\s*id:\s*\"[^\"]*\",\s*from_version:\s*(?P<from>\d+),"
    r"\s*to_version:\s*(?P<to>\d+),"
)


def postgres_stamp_mismatches(repo: Path):
    """Where the PostgreSQL schema stamp and its expand catalog count differently.

    The catalog's steps, and the `lash_schema_versions` row a store carries,
    are in the compatibility descriptor's numbers. A stamp's bump owes a
    catalog step from its old value to its new one (check_version_bumps.py),
    so that chain can only be read when the stamp is the descriptor's
    version. After the reset the stamp a store carries must therefore equal
    the version the POSTGRES descriptor writes, in the default and the
    synthetic-next build, and every catalog step must lie inside the stamp's
    own range. SCHEMA_VERSION is one constant in both tiers: provisioning
    stamps the catalog's own number, and the synthetic build's built-in
    expand (apply_synthetic_expand) then carries a store one step to the
    version that tier writes.
    """
    schema = (repo / POSTGRES_STAMPS).read_text()
    stamps = {m["stamp"]: (m["path"], m["table"]) for m in STAMP_POSTGRES.finditer(schema)}
    components = set(POSTGRES_COMPONENT.findall(schema))
    descriptors = without_comments((repo / COMPAT_DESCRIPTORS).read_text())
    catalog = without_comments((repo / POSTGRES_CATALOG).read_text())
    table = EXPAND_TABLE.search(catalog)
    rows = list(EXPAND_STEP.finditer(table["body"])) if table else []
    declared = len(re.findall(r"ExpandMigration\s*\{", table["body"])) if table else -1
    if (list(stamps.values()) != [(str(POSTGRES_CATALOG), "EXPAND_MIGRATIONS")]
            or len(components) != 1 or len(rows) != declared):
        raise BaselineError("cannot read the PostgreSQL stamp, its component and its catalog")
    stamp = next(iter(stamps))
    component = next(iter(components))
    errors = []
    values = {}
    for synthetic in (False, True):
        tier = "synthetic-next" if synthetic else "default"
        written = [
            int(m["bounds"].split(",")[-1])
            for m in DESCRIPTOR.finditer(descriptors)
            if m["component"] == component and enabled(m["attrs"], synthetic)
        ]
        if len(written) != 1:
            raise BaselineError(f"{component}: expected one {tier} compat descriptor, found {len(written)}")
        values[synthetic] = value = resolve(schema, stamp, synthetic)
        # A store carries SCHEMA_VERSION under a default build; the synthetic
        # build's expand moves it exactly one version (`version + 1 == next`).
        carried = value + (1 if synthetic else 0)
        if carried != written[0]:
            errors.append(
                f"{POSTGRES_STAMPS}:{stamp}: {tier} stamp {carried}, but the "
                f"{component} descriptor and its catalog write {written[0]}"
            )
        for row in rows:
            if not enabled(row["attrs"], synthetic):
                continue
            start, end = int(row["from"]), int(row["to"])
            if not 1 <= start < end <= value:
                errors.append(
                    f"{POSTGRES_CATALOG}: expand step {start} to {end} is outside "
                    f"the {tier} stamp {stamp} = {value}"
                )
    steps = {
        (int(row["from"]), int(row["to"]))
        for row in rows
        if enabled(row["attrs"], True)
    }
    at = values[False]
    while at < values[True] and any(start == at for start, _ in steps):
        at = max(end for start, end in steps if start == at)
    if at != values[True]:
        errors.append(
            f"{POSTGRES_CATALOG}: expand has no step chain from the default stamp "
            f"{values[False]} to the synthetic-next stamp {values[True]} ({stamp})"
        )
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT)
    commands = parser.add_subparsers(dest="command", required=True)
    inventory_command = commands.add_parser("inventory")
    inventory_command.add_argument("--build-report", type=Path)
    inventory_command.add_argument("--synthetic", action="store_true")
    commands.add_parser("check")
    args = parser.parse_args()
    try:
        rows = inventory(args.repo)
        if args.command == "inventory":
            if args.build_report is not None:
                verify_build(rows, args.build_report, args.synthetic)
            print(json.dumps(rows, indent=2))
            return 0
        errors = (mismatches(rows) + sqlite_stamp_mismatches(args.repo)
                  + postgres_stamp_mismatches(args.repo))
        if errors:
            print("\n".join(errors), file=sys.stderr)
            return 1
        print(
            f"release baseline: {len(rows)} surfaces, zero mismatches; "
            "SQLite and PostgreSQL stamps equal their catalog numbers"
        )
        return 0
    except (BaselineError, OSError, KeyError, tomllib.TOMLDecodeError, json.JSONDecodeError) as error:
        print(f"release baseline error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
