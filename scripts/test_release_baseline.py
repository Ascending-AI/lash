#!/usr/bin/env python3
"""Release-cut inventory and reset laws."""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import release_baseline as baseline
import release_reset as reset

ROOT = Path(__file__).resolve().parents[1]


class ReleaseBaselineTests(unittest.TestCase):
    def command(self, *args):
        return subprocess.run(
            [sys.executable, str(ROOT / "scripts/release_baseline.py"), *args],
            capture_output=True, text=True, cwd=ROOT,
        )

    def test_registered_surfaces_are_resolved_in_both_tiers(self):
        result = self.command("inventory")
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = json.loads(result.stdout)
        self.assertEqual(len(rows), len(baseline.surfaces(ROOT)))
        by_name = {row["key"]: row for row in rows}
        at_cut = not baseline.mismatches(rows)
        for path, constant, default, synthetic in [
            ("crates/lash-sqlite-store/src/schema.rs", "SCHEMA_VERSION", 99, 100),
            ("crates/lash-sqlite-store/src/schema.rs", "PROCESS_SCHEMA_VERSION", 44, 45),
            ("crates/lash-sqlite-store/src/schema.rs", "TRIGGER_SCHEMA_VERSION", 12, 13),
            ("crates/lash-restate/src/process/admission.rs", "JOURNAL_LOGIC_EPOCH", 1, 2),
            ("crates/lashlang/src/workflow_graph.rs", "WORKFLOW_GRAPH_SCHEMA_VERSION", 21, 22),
        ]:
            row = by_name[f"{path}:{constant}"]
            if at_cut:
                default, synthetic = 1, 2
            self.assertEqual((row["default"], row["synthetic"]), (default, synthetic))
        self.assertTrue(all(row["upgrade"] and row["default"] is not None for row in rows))

    def test_draft_table_detects_pre_cut_constants(self):
        result = self.command("check")
        if not baseline.mismatches(baseline.inventory(ROOT)):
            self.assertEqual(result.returncode, 0, result.stderr)
            return
        self.assertEqual(result.returncode, 1, result.stderr)
        for name in ["REMOTE_PROTOCOL_VERSION", "RESTATE_PROCESS_JOURNAL_VERSION",
                     "WORKFLOW_GRAPH_SCHEMA_VERSION", "PROCESS_SCHEMA_VERSION",
                     "TRIGGER_SCHEMA_VERSION", "lash-postgres-store/src/lib.rs:SCHEMA_VERSION"]:
            self.assertIn(name, result.stderr)

    def test_release_values_match_declared_baseline(self):
        result = self.command("check")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_sqlite_stamps_equal_their_catalog_numbers(self):
        self.assertEqual(baseline.sqlite_stamp_mismatches(ROOT), [])

    def test_postgres_stamp_equals_its_catalog_number(self):
        self.assertEqual(baseline.postgres_stamp_mismatches(ROOT), [])

    def test_pre_cut_postgres_stamp_is_named_against_its_descriptor(self):
        errors = baseline.postgres_stamp_mismatches(ROOT)
        if not baseline.mismatches(baseline.inventory(ROOT)):
            self.assertEqual(errors, [])
            return
        self.assertTrue(
            any(":SCHEMA_VERSION: default stamp" in error and "POSTGRES" in error
                for error in errors), errors)
        self.assertTrue(
            any(":SCHEMA_VERSION: synthetic-next stamp" in error and "POSTGRES" in error
                for error in errors), errors)

    def test_pre_cut_sqlite_stamps_are_named_against_their_catalog_numbers(self):
        errors = baseline.sqlite_stamp_mismatches(ROOT)
        if not baseline.mismatches(baseline.inventory(ROOT)):
            self.assertEqual(errors, [])
            return
        for stamp, component in [("SCHEMA_VERSION", "SQLITE_CORE"),
                                 ("PROCESS_SCHEMA_VERSION", "SQLITE_REGISTRY"),
                                 ("TRIGGER_SCHEMA_VERSION", "SQLITE_TRIGGERS")]:
            self.assertTrue(any(f":{stamp}: default stamp" in error and component in error
                                for error in errors), errors)

    def test_resolver_rejects_ambiguous_missing_and_unsupported_constants(self):
        for text, name in [("const V: u32 = 1;\nconst V: u32 = 2;", "V"),
                           ("const V: u32 = 1;", "MISSING"),
                           ("const V: u32 = compute();", "V"),
                           ("const V: u32 = OTHER;", "V"),
                           ("const V: u32 = V;", "V"),
                           ('#[cfg(feature = "unknown")]\nconst V: u32 = 1;', "V")]:
            with self.subTest(text=text), self.assertRaises(baseline.BaselineError):
                baseline.resolve(text, name, False)

    def test_comments_cannot_supply_constants_or_hide_cfg(self):
        text = '''
// const V: u32 = 99;
/* /* nested */ const V: u32 = 88; */
#[cfg(not(feature = "synthetic-next"))]
/// const V: u32 = 77;
pub const V: u32 = BASE;
#[cfg(feature = "synthetic-next")]
pub const V: u32 = BASE + 1;
const BASE: u32 = 7u32;
const RAW: &str = r#"// const V: u32 = 66;"#;
'''
        self.assertEqual(baseline.resolve(text, "V", False), 7)
        self.assertEqual(baseline.resolve(text, "V", True), 8)

    def test_the_baseline_is_one_rule_over_the_inventory(self):
        self.assertEqual(baseline.baseline_of(141), 1)
        self.assertEqual(baseline.baseline_of("lashlang-vm-abi-v14"), "lashlang-vm-abi-v1")
        self.assertEqual(baseline.mismatches([{"key": "owner:V", "default": 1},
                                              {"key": "owner:ABI", "default": "abi-v1"}]), [])
        self.assertIn("default 2, release baseline 1",
                      baseline.mismatches([{"key": "owner:V", "default": 2}])[0])
        self.assertIn("release baseline 'abi-v1'",
                      baseline.mismatches([{"key": "owner:ABI", "default": "abi-v7"}])[0])
        # A value the rule cannot place is refused, never passed.
        for unplaced in ("abi", 1.5, None):
            self.assertTrue(baseline.mismatches([{"key": "owner:V", "default": unplaced}]))

    def test_scratch_reset_plan_covers_changes_and_retained_old_value_is_red(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            paths = {row["constant_path"] for row in baseline.surfaces(ROOT)}
            paths.update(["scripts/versioned-surfaces.toml",
                          "crates/lash-core-store/src/store/state_version.rs",
                          "crates/lash-core-store/src/store/synthetic_next.rs",
                          "crates/lash-postgres-store/src/postgres/migrate.rs",
                          "crates/lash-postgres-store/schema.sql",
                          "crates/lash-sqlite-store/src/migration.rs",
                          "crates/lash-core-store/src/compat.rs",
                          "crates/lash-typescript/tests/workflow_graph_schema.rs",
                          "examples/workflow-graph-roundtrip/frontend/scripts/generate-contract-types.mjs",
                          "examples/workflow-graph-roundtrip/CONTRACT.md"])
            for relative in paths:
                destination = repo / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / relative, destination)
            before = {path: (repo / path).read_bytes() for path in paths}
            public, edits = reset.plan(repo)
            self.assertEqual(public["hardcoded_workflow_schema_paths_after_reset"], [])
            self.assertEqual(before, {path: (repo / path).read_bytes() for path in paths}, "dry-run writes nothing")
            for path, text in edits.items():
                path.write_text(text)
            changed = {path for path in paths if before[path] != (repo / path).read_bytes()}
            self.assertEqual(changed, set(public["source_edits"]))
            self.assertEqual(baseline.mismatches(baseline.inventory(repo)), [])
            source = repo / "crates/lash-remote-protocol/src/lib.rs"
            source.write_text(source.read_text().replace("REMOTE_PROTOCOL_VERSION: u32 = 1;",
                                                       "REMOTE_PROTOCOL_VERSION: u32 = 100;"))
            errors = baseline.mismatches(baseline.inventory(repo))
            self.assertTrue(any("REMOTE_PROTOCOL_VERSION" in error for error in errors))

            # After the reset every SQLite stamp is its catalog's number, in
            # both tiers; a stamp that keeps an old value, and a catalog step
            # numbered past its stamp, are red. The PostgreSQL stamp's law is
            # the same.
            self.assertEqual(baseline.sqlite_stamp_mismatches(repo), [])
            self.assertEqual(baseline.postgres_stamp_mismatches(repo), [])
            check = subprocess.run(
                [sys.executable, str(ROOT / "scripts/release_baseline.py"), "--repo", str(repo), "check"],
                capture_output=True, text=True, cwd=ROOT,
            )
            self.assertIn("REMOTE_PROTOCOL_VERSION", check.stderr)
            self.assertNotIn("stamp", check.stderr)
            lib = repo / "crates/lash-postgres-store/src/lib.rs"
            reset_lib = lib.read_text()
            self.assertIn("const SCHEMA_VERSION: i32 = 1;", reset_lib)
            self.assertTrue((repo / "crates/lash-postgres-store/schema.sql").read_text().startswith(
                "-- lash-postgres-store schema, DDL revision 1; compatibility stamp "))
            lib.write_text(reset_lib.replace("const SCHEMA_VERSION: i32 = 1;",
                                             "const SCHEMA_VERSION: i32 = 2;"))
            errors = baseline.postgres_stamp_mismatches(repo)
            self.assertTrue(errors and all("SCHEMA_VERSION" in error for error in errors), errors)
            self.assertTrue(any("stamp 2" in error and "POSTGRES" in error for error in errors))
            check = subprocess.run(
                [sys.executable, str(ROOT / "scripts/release_baseline.py"), "--repo", str(repo), "check"],
                capture_output=True, text=True, cwd=ROOT,
            )
            self.assertIn("POSTGRES", check.stderr)
            lib.write_text(reset_lib)
            expand = repo / "crates/lash-postgres-store/src/postgres/migrate.rs"
            reset_expand = expand.read_text()
            self.assertIn("static EXPAND_MIGRATIONS: &[ExpandMigration] = &[", reset_expand)
            expand.write_text(reset_expand.replace(
                "static EXPAND_MIGRATIONS: &[ExpandMigration] = &[",
                "static EXPAND_MIGRATIONS: &[ExpandMigration] = &[\n"
                "    ExpandMigration {\n"
                "        id: \"0002-past-the-stamp\",\n"
                "        from_version: 1,\n"
                "        to_version: 9,\n"
                "        statements: \"-- past the stamp\",\n"
                "    },", 1))
            errors = baseline.postgres_stamp_mismatches(repo)
            self.assertTrue(any("expand step 1 to 9 is outside" in error for error in errors), errors)
            expand.write_text(reset_expand)
            schema = repo / "crates/lash-sqlite-store/src/schema.rs"
            reset_schema = schema.read_text()
            self.assertIn("const BASE_PROCESS_SCHEMA_VERSION: i32 = 1;", reset_schema)
            schema.write_text(reset_schema.replace("const BASE_PROCESS_SCHEMA_VERSION: i32 = 1;",
                                                   "const BASE_PROCESS_SCHEMA_VERSION: i32 = 44;"))
            errors = baseline.sqlite_stamp_mismatches(repo)
            self.assertTrue(errors and all("PROCESS_SCHEMA_VERSION" in error for error in errors), errors)
            self.assertTrue(any("default stamp 44" in error and "SQLITE_REGISTRY" in error for error in errors))
            schema.write_text(reset_schema)
            catalog = repo / "crates/lash-sqlite-store/src/migration.rs"
            reset_catalog = catalog.read_text()
            self.assertIn("from: 1,\n        to: 2,", reset_catalog)
            catalog.write_text(reset_catalog.replace("from: 1,\n        to: 2,", "from: 2,\n        to: 3,", 1))
            errors = baseline.sqlite_stamp_mismatches(repo)
            self.assertTrue(any("DurableCore step 2 to 3 is outside" in error for error in errors), errors)
            self.assertTrue(any("DurableCore has no step chain from the default stamp 1" in error
                                for error in errors), errors)


if __name__ == "__main__":
    unittest.main()
