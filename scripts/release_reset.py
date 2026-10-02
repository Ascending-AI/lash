#!/usr/bin/env python3
"""Plan or apply the 1.0 baseline reset, including schema and fixture generation.

Run through `kiln gate` in an isolated fork. Dry-run emits every source edit,
schema rename, and generator output. Apply executes that plan, regenerates
schemas and retained current-tree fixtures through their owning generators,
and checks the declared baseline. It does not capture a tagged release corpus.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

import release_baseline as baseline

SCHEMA_GENERATORS = [
    "//crates/lashlang:workflow_schema_generator__bin",
    "//crates/lash-trace:trace_schema_generator__bin",
    "//crates/lash-remote-protocol:remote_schema_generator__bin",
    "//crates/lash-core-execution:process_event_schema_generator__bin",
]
SHAPE_TARGET = "//crates/lash-postgres-store:lash-postgres-store__unit_test"
SHAPE_LAW = "schema_shape::tests::committed_shape_artifact_matches_the_ddl_artifact"
SHAPE_PATH = "crates/lash-postgres-store/schema-shape.txt"
FIXTURE_GENERATORS = [
    ("//crates/lash-sqlite-store:durable_read_fixture__test", "regenerate_sqlite_durable_fixture",
     {"LASH_REGENERATE_DURABLE_READ_FIXTURES": "1"}, "fixtures/durable-read/v1/sqlite"),
    ("//crates/lash-postgres-store:durable_read_fixture__test", "regenerate_postgres_durable_fixture",
     {"LASH_REGENERATE_DURABLE_READ_FIXTURES": "1"}, "fixtures/durable-read/v1/postgres"),
    ("//crates/lash-restate:lash-restate__unit_test", "tests::replay_corpus::regenerate_replay_corpus_fixtures",
     {"LASH_REGENERATE_REPLAY_CORPUS": "1"}, "crates/lash-restate/testdata/replay-corpus"),
]


def plan(repo: Path):
    rows = baseline.inventory(repo)
    declared = {row["key"]: baseline.baseline_of(row["default"]) for row in rows}
    edits = {}
    replacements = {}

    def constant(path: Path, name: str, default, synthetic):
        text = path.read_text()
        changes = replacements.setdefault(path, {})
        for match in baseline.definitions(text):
            if match["name"] != name:
                continue
            old = match["value"].strip()
            target = default if baseline.enabled(match["attrs"], False) else synthetic
            if re.fullmatch(r"[A-Z][A-Z0-9_]*(?:\s*\+\s*1)?", old):
                alias = old.split()[0]
                constant(path, alias, default, default)
                continue
            changes[match.span("value")] = json.dumps(target)

    for row in rows:
        expected = declared[row["key"]]
        path, name = row["key"].rsplit(":", 1)
        successor = expected + 1 if type(expected) is int and row["synthetic"] != row["default"] else expected
        constant(repo / path, name, expected, successor)

    # An admission floor the registry ties to a surface moves with it.
    for path, floor, surface in baseline.floors(repo):
        expected = declared[f"{path}:{surface}"]
        constant(repo / path, floor, expected, expected)

    for path, changes in replacements.items():
        text = path.read_text()
        for (start, end), value in sorted(changes.items(), reverse=True):
            text = text[:start] + value + text[end:]
        if text != path.read_text():
            edits[path] = text

    # The value tables follow the reset constants. A hash domain the reset
    # supersedes stays reserved: it joins the registry's retired names.
    after = [{**row, "default": declared[row["key"]],
              "synthetic": declared[row["key"]] + (row["synthetic"] != row["default"])
              if type(row["default"]) is int else declared[row["key"]]} for row in rows]
    retired = baseline.retired_hash_domains(repo)
    superseded = sorted(set(baseline.hash_domains(rows)) - set(baseline.hash_domains(after)) - set(retired))
    if superseded:
        path = repo / baseline.REGISTRY
        edits[path] = path.read_text().rstrip("\n") + "\n" + "".join(
            f'\n[[retired_hash_domain]]\nname = {json.dumps(name)}\n'
            'reason = "superseded by the 1.0 baseline reset"\n' for name in superseded)
    for relative, text in baseline.generated_tables(after, [*retired, *superseded]).items():
        path = repo / relative
        if not path.is_file() or text != path.read_text():
            edits[path] = text

    # Delete the production predecessor chain, preserving cfg-gated N+1 rows.
    path = repo / baseline.POSTGRES_CATALOG
    text = path.read_text()
    for table, item in [("EXPAND_MIGRATIONS", "ExpandMigration"),
                        ("BACKFILL_MIGRATIONS", "BackfillMigration"),
                        ("CONTRACT_MIGRATIONS", "ContractMigration")]:
        pattern = r"(static " + table + r":.*?= &\[)(.*?)(\s*\];)"
        match = re.search(pattern, text, re.DOTALL)
        if match is None:
            raise baseline.BaselineError(f"cannot find {table}")
        # A cfg attribute belongs to the following row: only unconditional
        # rows are the production chain.
        body = re.sub(r'(?m)^(    #\[cfg\([^\n]*\)\]\n)?    ' + item + r' \{.*?\n    \},\n?',
                      lambda row: row[0] if row[1] else "", match[2], flags=re.DOTALL)
        # Unconditional rows are removed only from EXPAND; the other current
        # catalogs are N+1.
        if table != "EXPAND_MIGRATIONS":
            body = match[2]
            if body.strip() and '#[cfg(feature = "synthetic-next")]' not in body:
                raise baseline.BaselineError(f"unrecognized production rows in {table}")
        text = text[:match.start(2)] + body + text[match.end(2):]
    if text != path.read_text():
        edits[path] = text

    # schema.sql states the PostgreSQL schema version in its header and in
    # the stamp its seed row writes; both follow the constant.
    path = repo / baseline.POSTGRES_SCHEMA
    constant, _ = baseline.store_versions(repo)["POSTGRES"]
    version = declared[f"{baseline.STORE_VERSIONS}:{constant}"]
    text = baseline.postgres_schema_at(path.read_text(), version)
    if text != path.read_text():
        edits[path] = text

    # Generator-owned schema documents are renamed by regeneration. Rewrite
    # references, including include_str! paths, before compiling the cut tree.
    schemas = sorted((repo / "schemas/host").glob("*/*.schema.json"))
    renames = {str(path.relative_to(repo)): str(path.with_name("v1.schema.json").relative_to(repo))
               for path in schemas if path.name != "v1.schema.json"}
    references = {old: new for old, new in renames.items()}
    references.update({old.removesuffix(".schema.json"): new.removesuffix(".schema.json")
                       for old, new in renames.items()})
    hardcoded = []
    for directory in ("crates", "docs", "runbooks", "scripts", "examples"):
        for path in (repo / directory).rglob("*"):
            if path.suffix not in (".rs", ".md", ".html", ".py", ".json", ".ts", ".mjs"):
                continue
            text = edits.get(path, path.read_text())
            before = text
            for old, new in references.items():
                text = text.replace(old, new)
            if text != before:
                edits[path] = text
            if re.search(r"workflow-(?:graph|type-facets)/v\d+\.schema\.json", text):
                hardcoded.append(str(path.relative_to(repo)))
    generated = sorted({str(path.relative_to(repo)) for path in schemas} | set(renames.values()) | {SHAPE_PATH})
    fixtures = []
    for _, _, _, directory in FIXTURE_GENERATORS:
        root = repo / directory
        fixtures.extend(str(path.relative_to(repo)) for path in root.rglob("*") if path.is_file())
    # The store generators also create these paths in a fresh checkout.
    fixtures.extend("fixtures/durable-read/v1/sqlite/" + name for name in
                    ("durable-core.db", "processes.db", "triggers.db", "expected.json", "versions.json"))
    fixtures.extend("fixtures/durable-read/v1/postgres/" + name for name in
                    ("fixture.sql", "expected.json", "version.json"))
    public = {"source_edits": sorted(str(path.relative_to(repo)) for path in edits),
              "schema_renames": renames, "generated_paths": generated,
              "hardcoded_workflow_schema_paths_after_reset": sorted(hardcoded),
              "fixture_paths": sorted(set(fixtures)),
              "build_inventory_paths": ["BUCK", "tools/buck2/target-inventory.json"],
              "schema_generators": SCHEMA_GENERATORS,
              "schema_shape_generator": dict(target=SHAPE_TARGET, law=SHAPE_LAW,
                                             environment={"LASH_UPDATE_SCHEMA_SHAPE": "1"}),
              "fixture_generators": [dict(target=t, law=l, environment=e, output=d)
                                     for t, l, e, d in FIXTURE_GENERATORS]}
    return public, edits


def run(repo: Path, argv: list[str], environment=None):
    subprocess.run(argv, cwd=repo, env={**os.environ, **(environment or {})}, check=True)


def regenerate_schema_shape(repo: Path):
    path = repo / SHAPE_PATH
    before = path.read_bytes()
    argv = ["kiln", "test", SHAPE_TARGET, "--local-test-execution", "--no-test-cache",
            "--test_arg=--exact", f"--test_arg={SHAPE_LAW}",
            *[f"--test_env={key}" for key in ("LASH_POSTGRES_DATABASE_URL",)
              if key in os.environ]]
    result = subprocess.run([*argv, "--test_env=LASH_UPDATE_SCHEMA_SHAPE=1"],
                            cwd=repo, capture_output=True, text=True)
    output = result.stdout + result.stderr
    print(output, end="", flush=True)
    if result.returncode:
        # The owning generator deliberately fails its rewrite run, then asks
        # for a fresh build so include_str! verifies the newly generated bytes.
        if (path.read_bytes() == before or "schema-shape.txt -- rerun the suite to confirm" not in output
                or "test result: FAILED. 0 passed; 1 failed;" not in output):
            raise subprocess.CalledProcessError(result.returncode, argv)
    run(repo, [*argv, "--test_env=LASH_UPDATE_SCHEMA_SHAPE=0"])


def regenerate(repo: Path):
    run(repo, ["kiln", "sync"])
    report = repo / ".buck2/release-reset-build.json"
    run(repo, ["kiln", "build", *SCHEMA_GENERATORS, "--materializations", "final", "--build-report", str(report)])
    generators = []
    for label in SCHEMA_GENERATORS:
        output = subprocess.check_output(
            [sys.executable, "tools/buck2/outputs.py", "--report", str(report), "--label", label, "--single"],
            cwd=repo, text=True,
        ).strip()
        generators.extend(["--generator", output])
    run(repo, [sys.executable, "scripts/generate-workflow-schemas.py", *generators])
    run(repo, ["kiln", "sync"])
    regenerate_schema_shape(repo)
    for target, law, environment, _ in FIXTURE_GENERATORS:
        run(repo, ["kiln", "test", target, "--local-test-execution", "--no-test-cache",
                   "--test_arg=--ignored", "--test_arg=--exact", f"--test_arg={law}",
                   *[f"--test_env={key}={value}" for key, value in environment.items()],
                   *[f"--test_env={key}" for key in ("LASH_POSTGRES_DATABASE_URL",)
                     if key in os.environ]])
    run(repo, ["kiln", "sync"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=baseline.ROOT)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--dry-run", action="store_true")
    action.add_argument("--apply", action="store_true")
    parser.add_argument("--source-only", action="store_true", help="scratch proof only; skip generation")
    args = parser.parse_args()
    try:
        repo = args.repo.resolve()
        public, edits = plan(repo)
        print(json.dumps(public, indent=2), flush=True)
        if public["hardcoded_workflow_schema_paths_after_reset"]:
            raise baseline.BaselineError("hard-coded workflow schema version paths remain after reset")
        if args.dry_run:
            return 0
        if not args.source_only and not os.environ.get("LASH_POSTGRES_DATABASE_URL"):
            raise baseline.BaselineError("fixture generation requires an isolated PostgreSQL gate")
        for path, text in edits.items():
            path.write_text(text)
        if not args.source_only:
            regenerate(repo)
        rows = baseline.inventory(repo)
        errors = baseline.mismatches(rows)
        errors += baseline.sqlite_stamp_mismatches(repo)
        errors += baseline.postgres_stamp_mismatches(repo)
        errors += baseline.table_mismatches(repo, rows)
        if errors:
            raise baseline.BaselineError("\n".join(errors))
        return 0
    except (baseline.BaselineError, OSError, KeyError, subprocess.CalledProcessError) as error:
        print(f"release reset error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
