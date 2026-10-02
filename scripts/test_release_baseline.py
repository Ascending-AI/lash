#!/usr/bin/env python3
"""Release-cut inventory and reset laws."""

import json
import os
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

    def test_identity_versions_are_discovered_from_source(self):
        rows = baseline.inventory(ROOT)
        values = {row["default"] for row in rows}
        at_cut = not baseline.mismatches(rows)
        for value in ("frame-key/v2/", "lash.agent-frame-key/v2", "frame-node/v3/",
                      "lash-frame-node/v3", "lashlang:v2:blake3:",
                      "process-env:v6:blake3:", "lash-process-env/v6",
                      "lash-stable-identity/v2"):
            self.assertIn(baseline.baseline_of(value) if at_cut else value, values)
        self.assertTrue(any(row["key"].endswith(":TOOL_INTENT_IDENTITY_FAMILY_VERSION")
                            for row in rows))

    def test_new_unregistered_identity_versions_are_refused(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            (repo / "scripts").mkdir()
            (repo / "crates/demo/src").mkdir(parents=True)
            (repo / baseline.REGISTRY).write_text(
                '\n[[surface]]\nconstant = "KNOWN_VERSION"\n'
                'constant_path = "crates/demo/src/lib.rs"\nupgrade = "coexist"\n'
                '\n[[excluded_class]]\nsuffix = "_FAMILY_VERSION"\nreason = "hash tag"\n'
            )
            source = repo / "crates/demo/src/lib.rs"
            for addition in ('const UNDECLARED_FAMILY_VERSION: u8 = 2;',
                             'const CONTROL_INTENT_FORMAT: u32 = 1;',
                             'fn encode() { IdentityEncoder::new("demo", 2); }',
                             'fn encode() { hash("new-identity/v2"); }',
                             'const TAG: &str = r#"new-identity:v2:blake3:"#;'):
                source.write_text('const KNOWN_VERSION: u8 = 1;\n' + addition)
                with self.subTest(addition=addition), self.assertRaises(baseline.BaselineError):
                    baseline.inventory(repo)

    def test_registered_surfaces_are_resolved_in_both_tiers(self):
        result = self.command("inventory")
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = json.loads(result.stdout)
        self.assertEqual(len(rows), len(baseline.surfaces(ROOT)))
        by_name = {row["key"]: row for row in rows}
        at_cut = not baseline.mismatches(rows)
        # A store schema version is one constant in both tiers: the synthetic
        # build's descriptor writes the version after it.
        compat = "crates/lash-core-store/src/compat.rs"
        for path, constant, default, synthetic in [
            (compat, "POSTGRES_SCHEMA_VERSION", 141, 141),
            (compat, "SQLITE_CORE_SCHEMA_VERSION", 99, 99),
            (compat, "SQLITE_REGISTRY_SCHEMA_VERSION", 44, 44),
            (compat, "SQLITE_TRIGGERS_SCHEMA_VERSION", 12, 12),
            ("crates/lash-restate/src/process/admission.rs", "JOURNAL_LOGIC_EPOCH", 1, 2),
            ("crates/lashlang/src/workflow_graph.rs", "WORKFLOW_GRAPH_SCHEMA_VERSION", 21, 22),
        ]:
            row = by_name[f"{path}:{constant}"]
            if at_cut:
                default, synthetic = 1, 1 if default == synthetic else 2
            self.assertEqual((row["default"], row["synthetic"]), (default, synthetic))
        self.assertTrue(all(row["upgrade"] and row["default"] is not None for row in rows))

    def test_draft_table_detects_pre_cut_constants(self):
        result = self.command("check")
        if not baseline.mismatches(baseline.inventory(ROOT)):
            self.assertEqual(result.returncode, 0, result.stderr)
            return
        self.assertEqual(result.returncode, 1, result.stderr)
        for name in ["REMOTE_PROTOCOL_VERSION", "RESTATE_PROCESS_JOURNAL_VERSION",
                     "WORKFLOW_GRAPH_SCHEMA_VERSION", "SQLITE_REGISTRY_SCHEMA_VERSION",
                     "SQLITE_TRIGGERS_SCHEMA_VERSION", "compat.rs:POSTGRES_SCHEMA_VERSION"]:
            self.assertIn(name, result.stderr)

    @unittest.skipUnless(os.environ.get("LASH_RELEASE_CUT") == "1", "FIG-4485: release baseline activates at the 1.0 cut")
    def test_release_values_match_declared_baseline(self):
        result = self.command("check")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_sqlite_catalogs_are_in_their_compat_versions(self):
        self.assertEqual(baseline.sqlite_stamp_mismatches(ROOT), [])

    def test_postgres_schema_and_catalog_are_in_its_compat_version(self):
        self.assertEqual(baseline.postgres_stamp_mismatches(ROOT), [])

    def test_each_store_component_has_one_schema_version_constant(self):
        versions = baseline.store_versions(ROOT)
        self.assertEqual(
            {component: constant for component, (constant, _) in versions.items()},
            {"POSTGRES": "POSTGRES_SCHEMA_VERSION", "SQLITE_CORE": "SQLITE_CORE_SCHEMA_VERSION",
             "SQLITE_REGISTRY": "SQLITE_REGISTRY_SCHEMA_VERSION",
             "SQLITE_TRIGGERS": "SQLITE_TRIGGERS_SCHEMA_VERSION"})
        registered = {f'{row["constant_path"]}:{row["constant"]}' for row in baseline.surfaces(ROOT)}
        for constant, _ in versions.values():
            self.assertIn(f"{baseline.STORE_VERSIONS}:{constant}", registered)

    def test_a_step_bound_is_an_integer_or_the_store_s_own_constant(self):
        names = {"SQLITE_CORE_SCHEMA_VERSION": 7}
        self.assertEqual(baseline.step_bound(" 3 ", names, "t"), 3)
        self.assertEqual(baseline.step_bound("compat::SQLITE_CORE_SCHEMA_VERSION", names, "t"), 7)
        self.assertEqual(baseline.step_bound("compat::SQLITE_CORE_SCHEMA_VERSION + 1", names, "t"), 8)
        for expression in ["compat::SQLITE_REGISTRY_SCHEMA_VERSION", "next()", "1 + 1"]:
            with self.subTest(expression=expression), self.assertRaises(baseline.BaselineError):
                baseline.step_bound(expression, names, "t")

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

    def test_baseline_is_derived_for_every_discovered_value(self):
        for value, expected in [(7, 1), ("frame-key/v2/", "frame-key/v1/"),
                                ("lashlang:v2:blake3:", "lashlang:v1:blake3:"),
                                ("lashlang-vm-abi-v14", "lashlang-vm-abi-v1"),
                                ("wait-index/v2/wait/", "wait-index/v1/wait/")]:
            self.assertEqual(baseline.baseline_of(value), expected)
        with self.assertRaises(baseline.BaselineError):
            baseline.baseline_of("unversioned")

    def test_scratch_reset_plan_covers_changes_and_retained_old_value_is_red(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            paths = {row["constant_path"] for row in baseline.surfaces(ROOT)}
            paths.update(["scripts/versioned-surfaces.toml",
                          "crates/lash-core-store/src/store/state_version.rs",
                          "crates/lash-sansio/src/core_support.rs",
                          "crates/lash-core-store/src/store/synthetic_next.rs",
                          "crates/lash-postgres-store/src/postgres/migrate.rs",
                          "crates/lash-sqlite-store/src/migration.rs",
                          "crates/lash-sqlite-store/src/schema.rs",
                          "crates/lash-postgres-store/src/lib.rs",
                          "crates/lash-postgres-store/schema.sql",
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
            domains = (repo / "crates/lash-sansio/src/core_support.rs").read_text()
            self.assertIn('"lash-process-env/v1"', domains)
            self.assertIn('"lash-process-env/v6"', domains.split('const RETIRED_BLAKE3_DOMAINS:')[1])
            source = repo / "crates/lash-remote-protocol/src/lib.rs"
            source.write_text(source.read_text().replace("REMOTE_PROTOCOL_VERSION: u32 = 1;",
                                                       "REMOTE_PROTOCOL_VERSION: u32 = 100;"))
            errors = baseline.mismatches(baseline.inventory(repo))
            self.assertTrue(any("REMOTE_PROTOCOL_VERSION" in error for error in errors))

            # After the reset every store is at version 1 in compat.rs alone,
            # schema.sql states it, and the catalogs are numbered in it. An
            # artifact that keeps an old number, a backend that restates the
            # version, and a catalog step numbered past it are red.
            self.assertEqual(baseline.sqlite_stamp_mismatches(repo), [])
            self.assertEqual(baseline.postgres_stamp_mismatches(repo), [])
            self.assertEqual({value for _, value in baseline.store_versions(repo).values()}, {1})
            check = subprocess.run(
                [sys.executable, str(ROOT / "scripts/release_baseline.py"), "--repo", str(repo), "check"],
                capture_output=True, text=True, cwd=ROOT,
            )
            self.assertIn("REMOTE_PROTOCOL_VERSION", check.stderr)
            self.assertNotIn("stamp", check.stderr)
            artifact = repo / "crates/lash-postgres-store/schema.sql"
            reset_artifact = artifact.read_text()
            self.assertTrue(reset_artifact.startswith("-- lash-postgres-store schema, component version 1.\n"))
            self.assertIn("VALUES ('lash-postgres-store', 1, 1)", reset_artifact)
            artifact.write_text(reset_artifact.replace("VALUES ('lash-postgres-store', 1, 1)",
                                                       "VALUES ('lash-postgres-store', 141, 141)"))
            errors = baseline.postgres_stamp_mismatches(repo)
            self.assertTrue(any("seed stamp 141/141" in error and "POSTGRES_SCHEMA_VERSION = 1" in error
                                for error in errors), errors)
            check = subprocess.run(
                [sys.executable, str(ROOT / "scripts/release_baseline.py"), "--repo", str(repo), "check"],
                capture_output=True, text=True, cwd=ROOT,
            )
            self.assertIn("seed stamp 141/141", check.stderr)
            artifact.write_text(reset_artifact)
            lib = repo / "crates/lash-postgres-store/src/lib.rs"
            reset_lib = lib.read_text()
            alias = "const SCHEMA_VERSION: i32 = lash_core_execution::compat::POSTGRES_SCHEMA_VERSION as i32;"
            self.assertIn(alias, reset_lib)
            lib.write_text(reset_lib.replace(alias, "const SCHEMA_VERSION: i32 = 1;"))
            errors = baseline.postgres_stamp_mismatches(repo)
            self.assertTrue(any("must be the i32 alias" in error for error in errors), errors)
            lib.write_text(reset_lib)
            expand = repo / "crates/lash-postgres-store/src/postgres/migrate.rs"
            reset_expand = expand.read_text()
            self.assertIn("static EXPAND_MIGRATIONS: &[ExpandMigration] = &[", reset_expand)
            # The reset removes the production chain and keeps the synthetic
            # successor's row, cfg and all.
            self.assertNotIn('id: "0141-begin-session-close"', reset_expand)
            self.assertIn('    #[cfg(feature = "synthetic-next")]\n    ExpandMigration {\n'
                          '        id: "synthetic-next-expand",\n'
                          "        from_version: SCHEMA_VERSION,\n"
                          "        to_version: SCHEMA_VERSION + 1,", reset_expand)
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
            schema.write_text(reset_schema + "\nconst PROCESS_SCHEMA_VERSION: i32 = 44;\n")
            errors = baseline.sqlite_stamp_mismatches(repo)
            self.assertTrue(errors and all("PROCESS_SCHEMA_VERSION" in error and "restated" in error
                                           for error in errors), errors)
            schema.write_text(reset_schema)
            catalog = repo / "crates/lash-sqlite-store/src/migration.rs"
            reset_catalog = catalog.read_text()
            step = ("from: compat::SQLITE_CORE_SCHEMA_VERSION,\n"
                    "        to: compat::SQLITE_CORE_SCHEMA_VERSION + 1,")
            self.assertIn(step, reset_catalog)
            catalog.write_text(reset_catalog.replace(step, "from: 2,\n        to: 3,", 1))
            errors = baseline.sqlite_stamp_mismatches(repo)
            self.assertTrue(any("DurableCore step 2 to 3 is outside" in error for error in errors), errors)
            self.assertTrue(any("DurableCore has no step chain from the default stamp 1" in error
                                for error in errors), errors)
            catalog.write_text(reset_catalog.replace(
                step, "from: compat::SQLITE_REGISTRY_SCHEMA_VERSION,\n"
                      "        to: compat::SQLITE_REGISTRY_SCHEMA_VERSION + 1,", 1))
            with self.assertRaises(baseline.BaselineError):
                baseline.sqlite_stamp_mismatches(repo)


if __name__ == "__main__":
    unittest.main()
