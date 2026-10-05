#!/usr/bin/env python3
"""R8: missing, stale or unreconciled evidence cannot certify an E2E tier.

These are selector/receipt laws with synthetic artifacts, not host scenarios.
"""

import copy
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import sys


SPEC = importlib.util.spec_from_file_location("lash_e2e", Path(__file__).with_name("lash-e2e.py"))
e2e = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(e2e)
SOURCE = "a" * 40


class ReceiptLaws(unittest.TestCase):
    def setUp(self):
        (e2e.ROOT / "target").mkdir(exist_ok=True)
        self.directory = tempfile.TemporaryDirectory(dir=e2e.ROOT / "target")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.manifest = e2e.load_manifest()

    def artifact(self, name, value):
        path = self.root / name
        path.write_text(value if isinstance(value, str) else json.dumps(value))
        return {"path": name, "sha256": e2e.digest(path)}

    def fixture(self, tier="smoke"):
        manifest = copy.deepcopy(self.manifest)
        for scenario in manifest["scenarios"]:
            scenario["arc_guards"] = []
            for row in scenario["cases"]:
                row.update(state="ready", hold_reason=None, registration={
                    "label": "//crates/lash-upgrade-harness:e2e__test",
                    "test": f"{scenario['owner'].lower()}::{scenario['id'].lower()}::{row['variant'].replace('-', '_')}",
                })
        server = self.artifact("server", "synthetic server artifact")
        manifest["server"]["executable_sha256"] = server["sha256"]
        expected = e2e.plan(manifest, "b" * 64, tier, [], SOURCE)
        rows = []
        for index, row in enumerate(expected["cases"]):
            key = e2e.case_key(row)
            artifacts = {role: self.artifact(f"{index}-{role}", {"synthetic": True})
                         for role in row["artifacts"]}
            artifacts["cleanup"] = self.artifact(f"{index}-cleanup", {"complete": True, "errors": [], "remaining": []})
            artifacts["junit"] = self.artifact(f"{index}-junit", f'<testsuite><testcase name="{row["registration"]["test"]}" /></testsuite>')
            binaries = {role: {"source_sha": SOURCE, "artifact": self.artifact(f"{index}-{role}-binary", "synthetic binary")}
                        for role in row["binaries"]}
            artifacts["provenance"] = self.artifact(f"{index}-provenance", {
                "source_sha": SOURCE, "case": key, "protocol": "V7",
                "server_nodes": row["server_nodes"], "binaries": binaries,
                "server": {"version": manifest["server"]["version"],
                           "archive_sha256": manifest["server"]["archive_sha256"], "artifact": server},
            })
            rows.append({"key": key, "status": "passed", "executed": True, "artifacts": artifacts, "quarantine": None})
        groups = {}
        for group in {f"{r['store']}/{r['leg']}" for r in expected["cases"]}:
            keys = {e2e.case_key(r) for r in expected["cases"] if f"{r['store']}/{r['leg']}" == group}
            groups[group] = e2e.counts([r for r in rows if r["key"] in keys])
        receipt = {"source_sha": SOURCE, "manifest_sha256": "b" * 64, "tier": tier,
                   "cases": rows, "counts": e2e.counts(rows), "groups": groups, "audits": {}, "gates": {}}
        if tier == "release":
            receipt["audits"] = {name: {"commit": SOURCE, "receipt": self.artifact(f"audit-{name}", {
                "ticket": ticket, "source_sha": SOURCE, "status": "passed"})}
                for name, ticket in manifest["release_audits"].items()}
            receipt["gates"] = {name: self.artifact(f"gate-{name}", {
                "gate": name, "source_sha": SOURCE, "status": "passed"}) for name in e2e.GATES}
        return manifest, expected, receipt

    def test_r8_smoke_is_six_rows_and_held_rows_never_certify(self):
        planned = e2e.plan(self.manifest, "b" * 64, "smoke", [], SOURCE)
        self.assertEqual(planned["selected"], 6)
        self.assertEqual(planned["held"], [])
        self.assertEqual({r["scenario"] for r in planned["cases"]}, e2e.SMOKE)
        live = e2e.plan(self.manifest, "b" * 64, "live", [], SOURCE)
        self.assertEqual(live["guarded"], [])
        with patch.object(e2e, "ancestor", return_value=True):
            final = e2e.plan(self.manifest, "b" * 64, "full", ["S22"], SOURCE)
        self.assertEqual(final["guarded"], [])
        self.assertEqual({g["ticket"] for r in final["cases"] for g in r["arc_guards"]}, {f"FIG-{i}" for i in range(4896, 4901)})
        held = e2e.plan(self.manifest, "b" * 64, "full", ["S26"], SOURCE)
        self.assertEqual(held["held"], [
            "S26/observer-reconnect/sqlite_file/replay/standard",
            "S26/partial-stream-reset/sqlite_file/replay/standard",
            "S26/recorded-429-retry/sqlite_file/replay/standard",
        ])
        with self.assertRaisesRegex(ValueError, "held cases"):
            e2e.reconcile(held, {}, self.root, self.manifest)
        manifest, expected, receipt = self.fixture()
        expected["guarded"] = [e2e.case_key(expected["cases"][0])]
        with self.assertRaisesRegex(ValueError, "arc guards"):
            e2e.reconcile(expected, receipt, self.root, manifest)

    def test_r8_unknown_empty_stale_and_subset_release_selectors_refuse(self):
        for tier, selectors in [("smoke", ["S99"]), ("smoke", ["S14"]), ("smoke", ["S01", "S01"]), ("release", ["S01"])]:
            with self.subTest(tier=tier, selectors=selectors), self.assertRaises(ValueError):
                e2e.plan(self.manifest, "b" * 64, tier, selectors, SOURCE)
        args = ["lash-e2e.py", "run", "--tier", "smoke", "--sha", SOURCE, "--artifacts", str(self.root)]
        with patch.object(sys, "argv", args), patch.object(e2e.subprocess, "call") as command:
            self.assertEqual(e2e.main(), 1)
            command.assert_not_called()

    def test_r8_counts_and_identity_reconcile_per_store_and_leg(self):
        manifest, expected, receipt = self.fixture()
        result = e2e.reconcile(expected, receipt, self.root, manifest)
        self.assertEqual(result["counts"]["passed"], 6)
        for mutate in (
            lambda r: r["cases"].pop(),
            lambda r: r["cases"].append(r["cases"][0]),
            lambda r: r["counts"].update(executed=5),
            lambda r: r["groups"]["sqlite_file/live"].update(selected=True),
            lambda r: r.update(source_sha="c" * 40),
            lambda r: r.update(manifest_sha256="c" * 64),
            lambda r: r["cases"][0].update(status="not_run", executed=False),
            lambda r: r["cases"][0].update(quarantine="bug"),
        ):
            altered = copy.deepcopy(receipt)
            mutate(altered)
            with self.subTest(altered=altered["counts"]), self.assertRaises(ValueError):
                e2e.reconcile(expected, altered, self.root, manifest)

    def test_r8_artifacts_cleanup_and_junit_are_required_after_teardown(self):
        manifest, expected, receipt = self.fixture()
        for role, value in (
            ("cleanup", {"complete": True, "errors": [], "remaining": ["listener"]}),
            ("cleanup", {"complete": 1, "errors": [], "remaining": []}),
            ("junit", '<testsuite><testcase name="wrong::test" /></testsuite>'),
            ("junit", '<testsuite><testcase name="h2::s01::default"><skipped /></testcase></testsuite>'),
        ):
            altered = copy.deepcopy(receipt)
            altered["cases"][0]["artifacts"][role] = self.artifact("bad-artifact", value)
            with self.subTest(role=role), self.assertRaises(ValueError):
                e2e.reconcile(expected, altered, self.root, manifest)
        altered = copy.deepcopy(receipt)
        altered["cases"][0]["artifacts"].pop("journal")
        with self.assertRaisesRegex(ValueError, "incomplete artifacts"):
            e2e.reconcile(expected, altered, self.root, manifest)
        path = self.root / receipt["cases"][0]["artifacts"]["trace"]["path"]
        path.write_text("changed after receipt")
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            e2e.reconcile(expected, receipt, self.root, manifest)
        descriptor = self.artifact("outside", "outside")
        descriptor["path"] = "../outside"
        with self.assertRaisesRegex(ValueError, "inside receipt"):
            e2e.artifact(self.root, descriptor)

    def test_r8_real_substrate_and_exact_binary_provenance_cannot_be_substituted(self):
        manifest, expected, receipt = self.fixture()
        descriptor = receipt["cases"][0]["artifacts"]["provenance"]
        original = json.loads((self.root / descriptor["path"]).read_text())
        for mutate in (
            lambda p: p.update(protocol="V6"),
            lambda p: p.update(server_nodes=3),
            lambda p: p["binaries"].pop("host"),
            lambda p: p["binaries"]["host"].update(source_sha="c" * 40),
            lambda p: p["server"].update(archive_sha256="c" * 64),
        ):
            altered = copy.deepcopy(receipt)
            provenance = copy.deepcopy(original)
            mutate(provenance)
            altered["cases"][0]["artifacts"]["provenance"] = self.artifact("bad-provenance", provenance)
            with self.subTest(provenance=provenance), self.assertRaises(ValueError):
                e2e.reconcile(expected, altered, self.root, manifest)

    def test_r8_release_requires_landed_audits_and_exact_candidate_gate_receipts(self):
        manifest, expected, receipt = self.fixture("release")
        with patch.object(e2e, "ancestor", return_value=True):
            e2e.reconcile(expected, receipt, self.root, manifest)
            for field, name in [("audits", "Z04"), ("gates", "phase_a")]:
                altered = copy.deepcopy(receipt)
                altered[field].pop(name)
                with self.subTest(field=field), self.assertRaises(ValueError):
                    e2e.reconcile(expected, altered, self.root, manifest)
            altered = copy.deepcopy(receipt)
            altered["gates"]["schema"] = self.artifact("wrong-schema", {"gate": "schema", "source_sha": "c" * 40, "status": "passed"})
            with self.assertRaisesRegex(ValueError, "candidate gate"):
                e2e.reconcile(expected, altered, self.root, manifest)
        with patch.object(e2e, "ancestor", return_value=False), self.assertRaisesRegex(ValueError, "not landed"):
            e2e.reconcile(expected, receipt, self.root, manifest)

    def test_r8_browser_execution_selects_its_exact_oracle_without_certifying_a_tier(self):
        key = "S28/default/sqlite_file/live/rlm"
        expected = e2e.plan(self.manifest, "b" * 64, "full", ["S28"], SOURCE, [key])
        self.assertEqual([e2e.case_key(row) for row in expected["cases"]], [key])
        self.assertEqual(expected["held"], [])
        with self.assertRaisesRegex(ValueError, "release certification"):
            e2e.plan(self.manifest, "b" * 64, "release", [], SOURCE, [key])
        with self.assertRaisesRegex(ValueError, "absent"):
            e2e.plan(self.manifest, "b" * 64, "full", ["S29"], SOURCE, [key])

        def runner_call(command, **kwargs):
            self.assertEqual(command[2], "//crates/lash-upgrade-harness:e2e_hosts__test")
            self.assertEqual(command[3], "s28_workbench_mcp_peer_restart")
            directory = Path(command[-1])
            directory.mkdir()
            e2e.write(directory / "execution.json", {
                "scenario": command[3], "label": command[2], "source_sha": SOURCE,
                "counts": {"executed": 1, "passed": 0, "failed": 1},
            })
            return 32

        with patch.object(e2e.subprocess, "check_output", side_effect=[SOURCE + "\n", ""]), \
             patch.object(e2e.subprocess, "call", side_effect=runner_call):
            result = e2e.run_cases(expected, self.root)
        self.assertEqual(result["counts"], {"selected": 1, "executed": 1, "passed": 0, "failed": 1, "not_run": 0})
        self.assertIs(result["certified"], False)
        self.assertFalse((self.root / "conclusion.json").exists())
        held = e2e.plan(self.manifest, "b" * 64, "full", ["S28"], SOURCE)
        with self.assertRaisesRegex(ValueError, "unavailable registrations"), \
             patch.object(e2e.subprocess, "call") as command:
            e2e.run_cases(held, self.root)
            command.assert_not_called()


if __name__ == "__main__":
    unittest.main()
