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

    def test_registered_surfaces_are_resolved_in_both_tiers(self):
        result = self.command("inventory")
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = json.loads(result.stdout)
        self.assertEqual(len(rows), len(baseline.surfaces(ROOT)))
        by_name = {row["key"]: row for row in rows}
        at_cut = not baseline.mismatches(rows, baseline.load_baseline(ROOT / baseline.BASELINE))
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
        result = self.command("check", "--baseline", "scripts/release-baseline.toml")
        if not baseline.mismatches(baseline.inventory(ROOT), baseline.load_baseline(ROOT / baseline.BASELINE)):
            self.assertEqual(result.returncode, 0, result.stderr)
            return
        self.assertEqual(result.returncode, 1, result.stderr)
        for name in ["REMOTE_PROTOCOL_VERSION", "RESTATE_PROCESS_JOURNAL_VERSION",
                     "WORKFLOW_GRAPH_SCHEMA_VERSION", "PROCESS_SCHEMA_VERSION",
                     "TRIGGER_SCHEMA_VERSION", "lash-postgres-store/src/lib.rs:SCHEMA_VERSION"]:
            self.assertIn(name, result.stderr)

    @unittest.skipUnless(os.environ.get("LASH_RELEASE_CUT") == "1", "FIG-4485: release baseline activates at the 1.0 cut")
    def test_release_values_match_declared_baseline(self):
        result = self.command("check", "--baseline", "scripts/release-baseline.toml")
        self.assertEqual(result.returncode, 0, result.stderr)

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

    def test_baseline_omissions_extras_and_wrong_types_are_refused(self):
        rows = [{"key": "owner:V", "default": 1}]
        self.assertIn("omitted", baseline.mismatches(rows, {})[0])
        self.assertIn("unregistered", baseline.mismatches(rows, {"owner:V": 1, "other:V": 1})[0])
        self.assertTrue(baseline.mismatches(rows, {"owner:V": "1"}))

    def test_scratch_reset_plan_covers_changes_and_retained_old_value_is_red(self):
        scratch_root = ROOT / ".buck2/release-baseline-tests"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as temporary:
            repo = Path(temporary)
            paths = {row["constant_path"] for row in baseline.surfaces(ROOT)}
            paths.update(["scripts/versioned-surfaces.toml", "scripts/release-baseline.toml",
                          "crates/lash-core-store/src/store/state_version.rs",
                          "crates/lash-core-store/src/store/synthetic_next.rs",
                          "crates/lash-postgres-store/src/postgres/migrate.rs",
                          "crates/lash-typescript/tests/workflow_graph_schema.rs",
                          "examples/workflow-graph-roundtrip/frontend/scripts/generate-contract-types.mjs",
                          "examples/workflow-graph-roundtrip/CONTRACT.md"])
            for relative in paths:
                destination = repo / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / relative, destination)
            before = {path: (repo / path).read_bytes() for path in paths}
            public, edits = reset.plan(repo, repo / baseline.BASELINE)
            self.assertEqual(public["hardcoded_workflow_schema_paths_after_reset"], [])
            self.assertEqual(before, {path: (repo / path).read_bytes() for path in paths}, "dry-run writes nothing")
            for path, text in edits.items():
                path.write_text(text)
            changed = {path for path in paths if before[path] != (repo / path).read_bytes()}
            self.assertEqual(changed, set(public["source_edits"]))
            declared = baseline.load_baseline(repo / baseline.BASELINE)
            self.assertEqual(baseline.mismatches(baseline.inventory(repo), declared), [])
            source = repo / "crates/lash-remote-protocol/src/lib.rs"
            source.write_text(source.read_text().replace("REMOTE_PROTOCOL_VERSION: u32 = 1;",
                                                       "REMOTE_PROTOCOL_VERSION: u32 = 100;"))
            errors = baseline.mismatches(baseline.inventory(repo), declared)
            self.assertTrue(any("REMOTE_PROTOCOL_VERSION" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
