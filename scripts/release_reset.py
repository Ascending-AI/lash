#!/usr/bin/env python3
"""Plan or apply the 1.0 baseline reset, including schema and fixture generation.

Run through `kiln gate` in an isolated fork. Dry-run emits every source edit,
schema rename, generator output and cut-only law. Apply executes that plan,
regenerates schemas and retained current-tree fixtures through their owning
generators, checks the declared baseline and the production catalogs, and runs
every cut-only law it unmarked. It does not capture a tagged release corpus.
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
import release_cut_laws as cut_laws
from fixture_regenerators import discover

SCHEMA_GENERATORS = [
    "//crates/lashlang:workflow_schema_generator__bin",
    "//crates/lash-trace:trace_schema_generator__bin",
    "//crates/lash-remote-protocol:remote_schema_generator__bin",
    "//crates/lash-core-execution:process_event_schema_generator__bin",
]


def plan(repo: Path):
    # The freeze changes stored shapes in place, so no production catalog
    # gains a step before the cut, and the reset deletes none: a step there
    # is a pre-1.0 predecessor the release must not carry.
    catalog = baseline.production_catalog_mismatches(repo)
    if catalog:
        raise baseline.BaselineError("\n".join(catalog))
    fixture_generators = discover(repo)
    laws = cut_laws.discover(repo)
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

    # schema.sql states the PostgreSQL schema version in its header and in
    # the stamp its seed row writes; both follow the constant.
    path = repo / baseline.POSTGRES_SCHEMA
    constant, _ = baseline.store_versions(repo)["POSTGRES"]
    version = declared[f"{baseline.STORE_VERSIONS}:{constant}"]
    text = baseline.postgres_schema_at(path.read_text(), version)
    if text != path.read_text():
        edits[path] = text

    # From the cut on, a cut-only law runs with its suite.
    for law in laws:
        path = repo / law["source"]
        text = cut_laws.unmarked(edits.get(path, path.read_text()), path.suffix)
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
    generated = sorted({str(path.relative_to(repo)) for path in schemas} | set(renames.values()))
    fixtures = []
    for generator in fixture_generators:
        root = repo / generator["output"]
        fixtures.append(generator["output"])
        if root.is_dir():
            fixtures.extend(str(path.relative_to(repo)) for path in root.rglob("*") if path.is_file())
    public = {"source_edits": sorted(str(path.relative_to(repo)) for path in edits),
              "schema_renames": renames, "generated_paths": generated,
              "hardcoded_workflow_schema_paths_after_reset": sorted(hardcoded),
              "fixture_paths": sorted(set(fixtures)),
              "build_inventory_paths": ["BUCK", "tools/buck2/target-inventory.json"],
              "schema_generators": SCHEMA_GENERATORS,
              "fixture_generators": fixture_generators,
              "cut_laws": laws}
    return public, edits


# `--apply` wrote, regenerated and checked the reset tree, and a cut-only law
# is red on it.
CUT_LAWS_FAILED = 3


def run(repo: Path, argv: list[str], environment=None):
    subprocess.run(argv, cwd=repo, env={**os.environ, **(environment or {})}, check=True)


INCLUDE = re.compile(r'\binclude_(?:str|bytes)!\(\s*"([^"\n]+)"\s*\)')


def compiled_inputs(repo: Path):
    """The repository files Rust compiles in through `include_str!` or
    `include_bytes!`, relative to the repository."""
    inputs = set()
    for path in (repo / "crates").rglob("*.rs"):
        for literal in INCLUDE.findall(path.read_text()):
            target = os.path.normpath(path.parent / literal)
            if target.startswith(str(repo) + os.sep):
                inputs.add(os.path.relpath(target, repo))
    return inputs


def generation_order(repo: Path, generators: list[dict]):
    """`generators`, those whose output another build compiles in first: a
    generator that compiles a stale artifact in fails before it writes."""
    compiled = compiled_inputs(repo)

    def compiled_in(generator):
        output = generator["output"]
        return any(path == output or path.startswith(output.rstrip("/") + "/") for path in compiled)

    return sorted(generators, key=lambda generator: not compiled_in(generator))


def regenerate_fixtures(repo: Path):
    for generator in generation_order(repo, discover(repo)):
        run(repo, ["kiln", "test", generator["target"], "--local-test-execution", "--no-test-cache",
                   "--test_arg=--ignored", "--test_arg=--exact", f"--test_arg={generator['law']}",
                   "--test_env=LASH_REGENERATE=1", f"--test_env=BUILD_WORKSPACE_DIRECTORY={repo}",
                   *[f"--test_env={key}" for key in ("LASH_POSTGRES_DATABASE_URL",)
                     if key in os.environ]])


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
    regenerate_fixtures(repo)
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
        undiscovered = cut_laws.undiscovered(repo, public["cut_laws"])
        if undiscovered:
            raise baseline.BaselineError("cut-only laws the reset cannot run:\n" + "\n".join(undiscovered))
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
        errors += baseline.production_catalog_mismatches(repo)
        errors += baseline.table_mismatches(repo, rows)
        if errors:
            raise baseline.BaselineError("\n".join(errors))
        if not args.source_only:
            cut_laws.run(repo, public["cut_laws"])
        return 0
    except cut_laws.CutLawFailure as error:
        # The reset tree is written and checked; only its laws are red.
        print(f"release reset error: {error}", file=sys.stderr)
        return CUT_LAWS_FAILED
    except (baseline.BaselineError, OSError, KeyError, subprocess.CalledProcessError) as error:
        print(f"release reset error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
