#!/usr/bin/env python3
"""Hold the single-owner layout of the shared SQL stores (FIG-3380).

`crates/lash-store-sql` owns, per table, one column list and every statement
whose text is identical across backends; each backend crate owns its driver's
row decoder and the statements that genuinely fork. Nothing in Rust enforces
that on its own: a call site can always paste a statement back, a second
column subset costs nothing to add, and a fork that exists on one backend only
is invisible until the conformance suite happens to cover it.

This gate is that enforcement, and it is *scoped*: `converted` in
`crates/lash-store-sql/dialect-only.toml` lists the families that have moved,
and the gate is total for them and silent about every other table in the
repository. FIG-3387 closes it over everything.

What it refuses, for each converted family:

1. **Stray SQL.** A production SQL string literal naming one of the family's
   tables, in a file that is not one of the family's declared owners, its
   schema artifacts, or an explicitly exempted source. `#[cfg(test)]` modules
   and test files are white-box and may spell SQL freely.
2. **A duplicate statement.** Two names whose text is the same, inside one
   backend's statement set (shared plus that backend's dialect-only).
3. **A shadowed statement.** A name declared both in the shared crate and in a
   backend.
4. **An unmanifested fork.** A statement declared in a backend and absent from
   `[[dialect_only]]`, an entry whose backends do not match the declarations
   (which is how "exists in one backend only" is named), or an entry with no
   reason.
5. **A stray column list.** A projection of two or more columns over a
   converted table that is not one of the `pub const … &str` column lists its
   table module declares.
6. **An undeclared cross-family statement.** A statement that is SQL over a
   converted table belonging to another family, with no `[[cross_family]]`
   entry naming the tables it reaches and why. A sweep that spans families is
   still owned by exactly one module; the entry is what makes the other
   families' owners able to find it.
7. **A spelled lifecycle literal.** A statement over a table whose columns
   carry domain vocabulary (`[families.<name>.vocabulary_columns]`) may not
   spell that vocabulary itself — `status IN ('running', …)`. It names the
   predicate as a `{{term(column)}}` token and the renderer expands it from
   the one generated source. This is FIG-2844's rule, held over the statement
   text this gate parses, so the two gates agree rather than merely not
   colliding.

Rules 1 and 6 read a literal as SQL over a table only when the table stands in
a *relation position* — after `FROM`, `INTO`, `UPDATE`, `JOIN`, `TABLE` or
`TRUNCATE`. A statement keyword alone matches prose: `"api.sessions.select"`
and a test name about merging wakes "across processes" are not queries.

A vocabulary token is part of a statement's text like any other characters, so
two statements that differ only in a token are two different statements and a
duplicate of one is still a duplicate.

Only the Python standard library is used, so this runs before any toolchain.
Run it from anywhere; paths are resolved against the repository root.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = Path("crates/lash-store-sql/dialect-only.toml")

# A string literal is SQL over a table when it carries a statement keyword AND
# names the table where SQL puts a relation. `lash_` is PostgreSQL's table
# prefix, so a literal naming the prefixed spelling is the same table.
SQL_KEYWORDS = ("SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "MERGE", "WITH")

# The keywords a relation can follow. Requiring one is what separates a query
# from prose that happens to contain a SQL word: `"api.sessions.select"`,
# `"triggers.update"` and "must batch compatible wakes across processes" all
# carry a keyword and a table name and none of them is a statement.
TABLE_POSITION_KEYWORDS = ("FROM", "INTO", "UPDATE", "JOIN", "TABLE", "TRUNCATE")

SET_HEADER = re.compile(
    r"statements!\s*\{(?P<body>)", re.MULTILINE
)
STRUCT_HEADER = re.compile(
    r"struct\s+(?P<name>\w+)\s*@\s*\"(?P<family>[\w.]+)\"\s*\{"
)
COLUMN_CONST = re.compile(
    r"pub\s+const\s+(?P<name>[A-Z][A-Z0-9_]*)\s*:\s*&\s*str\s*=\s*\"(?P<value>[^\"]*)\"\s*;",
    re.DOTALL,
)
TABLE_CONST = re.compile(r"pub\s+const\s+TABLE\s*:\s*&\s*str\s*=\s*\"(?P<value>\w+)\"\s*;")


def canonical(sql: str) -> str:
    """Collapse whitespace so two spellings of one statement compare equal."""
    return " ".join(sql.split())


@dataclass
class Declaration:
    """One named statement, as declared in a `statements!` block."""

    name: str
    sql: str
    path: str
    line: int


@dataclass
class Findings:
    """Everything the gate refuses, gathered so one run reports all of it."""

    failures: list[str] = field(default_factory=list)

    def refuse(self, message: str) -> None:
        self.failures.append(message)


def read_text(root: Path, relative: str) -> str:
    path = root / relative
    if not path.is_file():
        raise SystemExit(f"{MANIFEST}: `{relative}` does not exist")
    return path.read_text(encoding="utf-8")


def strip_cfg_test(source: str) -> str:
    """Blank out `#[cfg(test)] mod … { … }` blocks, keeping byte offsets.

    Test modules are white-box: a test may spell any statement it likes. Only
    production code answers to the ownership rules, and replacing the block
    with spaces keeps every later line number honest.
    """
    out = list(source)
    for match in re.finditer(r"#\[cfg\(test\)\]", source):
        rest = source[match.end() :]
        brace = rest.find("{")
        semicolon = rest.find(";")
        if brace == -1 or (semicolon != -1 and semicolon < brace):
            # `#[cfg(test)] mod tests;` — an out-of-line module, skipped by path.
            continue
        index = match.end() + brace
        depth = 0
        while index < len(source):
            character = source[index]
            if character == "{":
                depth += 1
            elif character == "}":
                depth -= 1
                if depth == 0:
                    break
            index += 1
        for position in range(match.start(), min(index + 1, len(source))):
            if out[position] != "\n":
                out[position] = " "
    return "".join(out)


def string_literals(source: str) -> list[tuple[int, str]]:
    """Every Rust string literal in `source`, as (byte offset, contents).

    Handles ordinary and raw literals, and skips line and block comments so a
    `//` mentioning a table is prose rather than a statement.
    """
    literals: list[tuple[int, str]] = []
    index = 0
    length = len(source)
    while index < length:
        character = source[index]
        if character == "/" and index + 1 < length and source[index + 1] == "/":
            end = source.find("\n", index)
            index = length if end == -1 else end + 1
            continue
        if character == "/" and index + 1 < length and source[index + 1] == "*":
            end = source.find("*/", index + 2)
            index = length if end == -1 else end + 2
            continue
        if character == "r" and index + 1 < length and source[index + 1] in "#\"":
            hashes = 0
            cursor = index + 1
            while cursor < length and source[cursor] == "#":
                hashes += 1
                cursor += 1
            if cursor < length and source[cursor] == '"':
                terminator = '"' + "#" * hashes
                end = source.find(terminator, cursor + 1)
                if end == -1:
                    break
                literals.append((index, source[cursor + 1 : end]))
                index = end + len(terminator)
                continue
        if character == '"':
            cursor = index + 1
            body: list[str] = []
            while cursor < length:
                if source[cursor] == "\\":
                    body.append(source[cursor : cursor + 2])
                    cursor += 2
                    continue
                if source[cursor] == '"':
                    break
                body.append(source[cursor])
                cursor += 1
            literals.append((index, "".join(body)))
            index = cursor + 1
            continue
        index += 1
    return literals


def line_of(source: str, offset: int) -> int:
    return source.count("\n", 0, offset) + 1


def names_table(text: str, table: str) -> bool:
    return re.search(rf"\b(?:lash_)?{re.escape(table)}\b", text) is not None


def is_sql_over(text: str, table: str) -> bool:
    """Whether `text` is a SQL statement (or fragment) over `table`.

    Two conditions, both required: the text carries a statement keyword, and
    the table stands in a relation position. The second is the one that stops
    the gate matching prose — a keyword on its own appears in tool names, route
    paths and English sentences.
    """
    if not names_table(text, table):
        return False
    upper = text.upper()
    if not any(re.search(rf"\b{keyword}\b", upper) for keyword in SQL_KEYWORDS):
        return False
    position = "|".join(TABLE_POSITION_KEYWORDS)
    return (
        re.search(rf"\b(?:{position})\s+(?:LASH_)?{re.escape(table.upper())}\b", upper) is not None
    )


def squeeze(text: str) -> str:
    """Lowercase and drop every space, so a predicate cannot hide behind
    casing or spacing. The same normalisation FIG-2844's gate applies."""
    return "".join(character.lower() for character in text if not character.isspace())


# Every `<column> <op> '<literal>` shape a spelled vocabulary predicate takes,
# as (squeezed needle, readable spelling).
VOCABULARY_LITERAL_SHAPES = (
    ("in('", "IN ('…')"),
    ("notin('", "NOT IN ('…')"),
    ("='", "= '…'"),
    ("<>'", "<> '…'"),
)


def spelled_vocabulary_literals(sql: str, column: str) -> list[str]:
    """Every place `sql` compares `column` against a quoted literal.

    A `{{term(column)}}` token is not such a comparison, which is the whole
    point: the token is how a statement names the predicate without spelling
    the vocabulary.
    """
    squeezed = squeeze(sql)
    needle_column = squeeze(column)
    found: list[str] = []
    for operator, spelling in VOCABULARY_LITERAL_SHAPES:
        if f"{needle_column}{operator}" in squeezed:
            found.append(f"{column} {spelling}")
    return found


def parse_statement_sets(source: str, path: str) -> list[Declaration]:
    """Every statement declared by a `statements!` block in `source`."""
    declarations: list[Declaration] = []
    for opening in re.finditer(r"statements!\s*\{", source):
        cursor = opening.end()
        depth = 1
        while cursor < len(source) and depth > 0:
            if source[cursor] == "{":
                depth += 1
            elif source[cursor] == "}":
                depth -= 1
            cursor += 1
        block = source[opening.end() : cursor - 1]
        header = STRUCT_HEADER.search(block)
        if header is None:
            raise SystemExit(f"{path}: a statements! block has no `struct Name @ \"family\"` header")
        family = header.group("family")
        body = block[header.end() :]
        for entry in re.finditer(r"(\w+)\s*=\s*\"", body):
            cursor = entry.end()
            literal: list[str] = []
            while cursor < len(body):
                if body[cursor] == "\\":
                    literal.append(body[cursor : cursor + 2])
                    cursor += 2
                    continue
                if body[cursor] == '"':
                    break
                literal.append(body[cursor])
                cursor += 1
            declarations.append(
                Declaration(
                    name=f"{family}.{entry.group(1)}",
                    sql="".join(literal),
                    path=path,
                    line=line_of(source, opening.end() + header.end() + entry.start()),
                )
            )
    return declarations


def declared_column_lists(source: str) -> dict[str, str]:
    """The column-list constants a table module declares, by constant name."""
    return {
        match.group("name"): canonical(match.group("value"))
        for match in COLUMN_CONST.finditer(source)
        if match.group("name") != "TABLE"
    }


FROM_TABLE = re.compile(r"\bFROM\s+(?P<table>\w+)", re.IGNORECASE)
# `DELETE FROM t` names the relation a statement writes, not a projection out
# of it. Pairing it with whatever `SELECT` happened to come earlier — a CTE's,
# or the outer one in a `WITH` — reads a column list out of text that is not a
# column list at all.
DELETE_BEFORE_FROM = re.compile(r"\bDELETE\s*$", re.IGNORECASE)
SELECT_KEYWORD = re.compile(r"\bSELECT\b", re.IGNORECASE)
INSERT_COLUMNS = re.compile(
    r"\bINSERT\s+INTO\s+(?P<table>\w+)\s*\((?P<columns>[^)]*)\)", re.IGNORECASE | re.DOTALL
)


def projections(sql: str, table: str) -> list[str]:
    """Every column list of two or more columns this statement uses on `table`.

    The read side is found from the `FROM` backwards to its own `SELECT`,
    rather than forwards from a `SELECT`, because a statement that selects out
    of a subquery (`SELECT scope_id, ?2, 0 FROM ( SELECT … FROM t )`) would
    otherwise pair the outer projection with the inner table. A `FROM` that a
    `DELETE` introduces is skipped for the same reason in reverse: it names
    what the statement writes, and the nearest earlier `SELECT` — a sibling
    CTE's, or the `WITH` statement's own — has nothing to do with it.
    """
    found: list[str] = []
    for match in FROM_TABLE.finditer(sql):
        if match.group("table") != table:
            continue
        if DELETE_BEFORE_FROM.search(sql[: match.start()]):
            continue
        keywords = list(SELECT_KEYWORD.finditer(sql, 0, match.start()))
        if not keywords:
            continue
        columns = canonical(sql[keywords[-1].end() : match.start()])
        if columns.upper().startswith("DISTINCT "):
            columns = columns[len("DISTINCT ") :]
        if "," in columns:
            found.append(columns)
    for match in INSERT_COLUMNS.finditer(sql):
        if match.group("table") != table:
            continue
        found.append(canonical(match.group("columns")))
    return found


def check(root: Path) -> list[str]:
    findings = Findings()
    manifest = tomllib.loads(read_text(root, str(MANIFEST)))
    converted = manifest["converted"]
    families = manifest["families"]
    exempt = {entry["path"]: entry["reason"] for entry in manifest.get("exempt", [])}
    for path, reason in exempt.items():
        if not reason.strip():
            findings.refuse(f"{MANIFEST}: exemption for `{path}` carries no reason")
        # A path ending in `/` exempts a subtree: an out-of-runtime harness is
        # a whole crate, and naming its files one by one would make the list
        # drift instead of the exemption being one decision.
        target = root / path.rstrip("/")
        if not (target.is_dir() if path.endswith("/") else target.is_file()):
            findings.refuse(f"{MANIFEST}: exempted path `{path}` does not exist")

    def is_exempt(relative: str) -> bool:
        return any(
            relative.startswith(path) if path.endswith("/") else relative == path
            for path in exempt
        )

    cross_family: dict[str, dict] = {}
    for entry in manifest.get("cross_family", []):
        name = entry["statement"]
        if name in cross_family:
            findings.refuse(f"{MANIFEST}: cross-family entry `{name}` is listed twice")
        if not entry.get("reason", "").strip():
            findings.refuse(f"{MANIFEST}: cross-family entry `{name}` carries no reason")
        cross_family[name] = entry

    manifest_entries: dict[str, dict] = {}
    for entry in manifest.get("dialect_only", []):
        name = entry["statement"]
        if name in manifest_entries:
            findings.refuse(f"{MANIFEST}: `{name}` is listed twice")
        if not entry.get("reason", "").strip():
            findings.refuse(f"{MANIFEST}: `{name}` carries no reason")
        manifest_entries[name] = entry

    claimed_names: set[str] = set()
    owner_paths: set[str] = set()
    # Filled per family, consumed by the stray-SQL pass once every family's
    # declarations are known: a family module may legitimately read another
    # family's table (quiescence spans both), so the rule is "inside an owner
    # module, every SQL literal over a converted table IS a declaration",
    # rather than a per-family path list.
    converted_tables: dict[str, str] = {}
    declared_texts: set[str] = set()
    owner_modules: set[str] = set()
    schema_modules: set[str] = set()
    # Every declaration, with the family that declared it: the cross-family
    # rule needs every family's tables known before it can say which of them a
    # statement reaches beyond its own.
    declarations_by_family: list[tuple[str, Declaration]] = []
    # table -> the columns over it whose values are domain vocabulary.
    vocabulary_columns: dict[str, list[str]] = {}

    for family in converted:
        if family not in families:
            findings.refuse(f"{MANIFEST}: converted family `{family}` has no `[families.{family}]`")
            continue
        spec = families[family]
        tables = spec["tables"]
        prefixes = tuple(spec["statement_prefixes"])
        backends = {"sqlite": spec["sqlite"], "postgres": spec["postgres"]}
        shared_paths = spec["shared"]
        owner_paths.update(shared_paths)
        for paths in backends.values():
            owner_paths.update(paths)
        owner_paths.update(spec.get("schema", []))

        def belongs(name: str) -> bool:
            return any(name.startswith(f"{prefix}.") for prefix in prefixes)

        shared: dict[str, Declaration] = {}
        for relative in shared_paths:
            for declaration in parse_statement_sets(read_text(root, relative), relative):
                if not belongs(declaration.name):
                    findings.refuse(
                        f"{relative}:{declaration.line}: `{declaration.name}` is declared in the "
                        f"`{family}` family's shared module but is not one of its statement "
                        f"prefixes {list(prefixes)}"
                    )
                if declaration.name in shared:
                    findings.refuse(
                        f"{relative}:{declaration.line}: `{declaration.name}` is declared twice "
                        "in the shared crate"
                    )
                shared[declaration.name] = declaration

        per_backend: dict[str, dict[str, Declaration]] = {}
        for backend, paths in backends.items():
            declared: dict[str, Declaration] = {}
            for relative in paths:
                source = strip_cfg_test(read_text(root, relative))
                for declaration in parse_statement_sets(source, relative):
                    if not belongs(declaration.name):
                        findings.refuse(
                            f"{relative}:{declaration.line}: `{declaration.name}` is declared in "
                            f"the `{family}` family's {backend} module but is not one of its "
                            f"statement prefixes {list(prefixes)}"
                        )
                    if declaration.name in declared:
                        findings.refuse(
                            f"{relative}:{declaration.line}: `{declaration.name}` is declared "
                            f"twice in the {backend} store"
                        )
                    declared[declaration.name] = declaration
            per_backend[backend] = declared

        # 3. Shadowing.
        for backend, declared in per_backend.items():
            for name, declaration in declared.items():
                if name in shared:
                    findings.refuse(
                        f"{declaration.path}:{declaration.line}: `{name}` shadows the shared "
                        f"statement declared at {shared[name].path}. A shared statement is the "
                        "one both backends issue; a per-backend copy of its name hides the fork "
                        "instead of declaring it — rename it, or delete the shared one and "
                        "manifest both."
                    )

        # 2. Duplicate text within a backend.
        for backend, declared in per_backend.items():
            seen: dict[str, str] = {}
            for declaration in sorted(
                list(shared.values()) + list(declared.values()), key=lambda item: item.name
            ):
                text = canonical(declaration.sql)
                if text in seen and seen[text] != declaration.name:
                    findings.refuse(
                        f"{declaration.path}:{declaration.line}: `{declaration.name}` has the "
                        f"same text as `{seen[text]}` in the {backend} store. One statement, one "
                        "name: give the operation the existing name instead of a second copy."
                    )
                seen.setdefault(text, declaration.name)

        # 4. Manifest coverage.
        for backend, declared in per_backend.items():
            for name, declaration in declared.items():
                claimed_names.add(name)
                entry = manifest_entries.get(name)
                if entry is None:
                    findings.refuse(
                        f"{declaration.path}:{declaration.line}: `{name}` is a dialect-only "
                        f"statement in the {backend} store and is missing from {MANIFEST}. Add a "
                        "[[dialect_only]] entry naming its backends and why the text forks."
                    )
                elif backend not in entry["backends"]:
                    findings.refuse(
                        f"{declaration.path}:{declaration.line}: `{name}` is declared in the "
                        f"{backend} store but {MANIFEST} lists it for {entry['backends']}."
                    )
        for name, entry in manifest_entries.items():
            if not belongs(name):
                continue
            for backend in entry["backends"]:
                if name not in per_backend.get(backend, {}):
                    findings.refuse(
                        f"{MANIFEST}: `{name}` is listed for `{backend}` but that store declares "
                        "no such statement."
                    )

        # 5. Column-list discipline.
        table_modules = spec["table_modules"]
        lists_by_table = {
            table: declared_column_lists(read_text(root, relative))
            for table, relative in table_modules.items()
        }
        for table, relative in table_modules.items():
            source = read_text(root, relative)
            declared_table = TABLE_CONST.search(source)
            if declared_table is None or declared_table.group("value") != table:
                findings.refuse(
                    f"{relative}: no `pub const TABLE: &str = \"{table}\";`. A table module names "
                    "its table."
                )
        all_declarations = list(shared.values())
        for declared in per_backend.values():
            all_declarations.extend(declared.values())
        for declaration in all_declarations:
            for table in tables:
                allowed = set(lists_by_table.get(table, {}).values())
                for columns in projections(declaration.sql, table):
                    if columns not in allowed:
                        findings.refuse(
                            f"{declaration.path}:{declaration.line}: `{declaration.name}` reads or "
                            f"writes `{columns}` over `{table}`, which is not one of the column "
                            f"lists {table_modules[table]} declares. Name the projection there, "
                            "with why it is narrow, or use an existing one."
                        )

        # 7. Spelled vocabulary.
        for table, columns in spec.get("vocabulary_columns", {}).items():
            if table not in tables:
                findings.refuse(
                    f"{MANIFEST}: `{family}` declares vocabulary columns on `{table}`, which is "
                    "not one of its tables"
                )
                continue
            vocabulary_columns[table] = columns

        converted_tables.update((table, family) for table in tables)
        declarations_by_family.extend((family, declaration) for declaration in all_declarations)
        for declaration in all_declarations:
            declared_texts.add(canonical(declaration.sql))
        owner_modules.update(shared_paths)
        for paths in backends.values():
            owner_modules.update(paths)
        schema_modules.update(spec.get("schema", []))

    # 6. Cross-family statements, once every family's tables are known.
    claimed_cross_family: set[str] = set()
    for family, declaration in declarations_by_family:
        touched = sorted(
            table
            for table, owner in converted_tables.items()
            if owner != family and is_sql_over(declaration.sql, table)
        )
        entry = cross_family.get(declaration.name)
        if entry is not None:
            claimed_cross_family.add(declaration.name)
        if not touched:
            if entry is not None:
                findings.refuse(
                    f"{MANIFEST}: `{declaration.name}` is listed as cross-family but reaches no "
                    "converted table outside its own family. Delete the entry."
                )
            continue
        if entry is None:
            findings.refuse(
                f"{declaration.path}:{declaration.line}: `{declaration.name}` is SQL over "
                f"{touched}, which belong to other converted families, and is not declared in "
                f"{MANIFEST}. A statement that spans families has one owner module and a "
                "[[cross_family]] entry naming the tables it reaches and why."
            )
            continue
        if entry["owner"] != declaration.path:
            findings.refuse(
                f"{MANIFEST}: `{declaration.name}` names owner `{entry['owner']}` but is declared "
                f"in {declaration.path}."
            )
        if sorted(entry["touches"]) != touched:
            findings.refuse(
                f"{MANIFEST}: `{declaration.name}` touches {touched}, but the cross-family entry "
                f"lists {sorted(entry['touches'])}."
            )
    for name in sorted(set(cross_family) - claimed_cross_family):
        findings.refuse(f"{MANIFEST}: cross-family entry `{name}` names no declared statement")

    # 7. A statement over a vocabulary-valued column may not spell it.
    for _family, declaration in declarations_by_family:
        for table, columns in vocabulary_columns.items():
            if not is_sql_over(declaration.sql, table):
                continue
            for column in columns:
                for spelling in spelled_vocabulary_literals(declaration.sql, column):
                    findings.refuse(
                        f"{declaration.path}:{declaration.line}: `{declaration.name}` spells "
                        f"`{spelling}` over `{table}`. `{column}` carries domain vocabulary: name "
                        "the predicate as a `{{term(column)}}` token and let the backend's "
                        "vocabulary expand it, so one enum edit still reaches every statement."
                    )

    # 1. Stray SQL, over every converted family at once.
    for relative in sorted(production_sources(root)):
        if relative in schema_modules or is_exempt(relative):
            continue
        inside_owner = relative in owner_modules
        source = strip_cfg_test((root / relative).read_text(encoding="utf-8"))
        for offset, literal in string_literals(source):
            for table, family in converted_tables.items():
                if not is_sql_over(literal, table):
                    continue
                if inside_owner and canonical(literal) in declared_texts:
                    break
                where = f"{relative}:{line_of(source, offset)}"
                if inside_owner:
                    findings.refuse(
                        f"{where}: a production SQL literal names `{table}` (converted family "
                        f"`{family}`) but is not one of this module's declared statements. Every "
                        "statement over a converted table is named: move it into a "
                        "`lash_store_sql::statements!` block."
                    )
                else:
                    findings.refuse(
                        f"{where}: a production SQL literal names `{table}`, which belongs to the "
                        f"converted `{family}` family. Its statements live in that family's owner "
                        "modules; call the named statement instead of spelling a new one."
                    )
                break

    for name in manifest_entries:
        if name not in claimed_names and any(
            name.startswith(f"{prefix}.")
            for family in converted
            for prefix in families.get(family, {}).get("statement_prefixes", [])
        ):
            findings.refuse(f"{MANIFEST}: `{name}` is listed but no store declares it")

    unknown = set(manifest_entries) - claimed_names
    for name in sorted(unknown):
        if not any(
            name.startswith(f"{prefix}.")
            for family in converted
            for prefix in families.get(family, {}).get("statement_prefixes", [])
        ):
            findings.refuse(
                f"{MANIFEST}: `{name}` belongs to no converted family's statement prefixes"
            )

    return findings.failures


def production_sources(root: Path) -> list[str]:
    """Every production Rust source the gate reads.

    Crate and example `src/` trees, minus the in-crate test modules: a file
    named `tests.rs`, anything under a `tests/` directory, and the
    `#[cfg(test)]` blocks that `strip_cfg_test` blanks out.
    """
    sources: list[str] = []
    for base in ("crates", "examples", "runbooks"):
        for path in sorted((root / base).glob("*/src/**/*.rs")):
            relative = path.relative_to(root).as_posix()
            parts = path.relative_to(root).parts
            if path.name == "tests.rs" or path.name.endswith("_tests.rs"):
                continue
            if "tests" in parts[2:]:
                continue
            sources.append(relative)
    return sources


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=ROOT,
        help="repository root to check (defaults to this script's repository)",
    )
    arguments = parser.parse_args()
    failures = check(arguments.root.resolve())
    if failures:
        print("store SQL ownership gate refused this tree:\n", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        print(
            f"\n{len(failures)} finding(s). The layout is described in "
            "docs/store-sql-authoring.md.",
            file=sys.stderr,
        )
        return 1
    print("store SQL ownership: converted families hold the single-owner layout")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
