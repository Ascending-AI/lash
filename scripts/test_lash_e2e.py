#!/usr/bin/env python3
"""R8: missing, stale or unreconciled evidence cannot certify an E2E tier.

These are selector/receipt laws with synthetic artifacts, not host scenarios.
"""

import copy
import importlib.util
import io
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
                "evidence_error": None,
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
        # Held-row refusal is a receipt rule independent of which scenarios
        # currently have replay oracles registered.
        manifest = copy.deepcopy(self.manifest)
        for scenario in manifest["scenarios"]:
            if scenario["id"] == "S26":
                for row in scenario["cases"]:
                    if row["leg"] == "replay":
                        row.update(state="held", hold_reason="synthetic missing replay oracle",
                                   registration=None)
        held = e2e.plan(manifest, "b" * 64, "full", ["S26"], SOURCE)
        self.assertEqual(held["held"], [
            "S26/observer-reconnect/sqlite_file/replay/standard",
            "S26/partial-stream-reset/sqlite_file/replay/standard",
            "S26/recorded-429-retry/sqlite_file/replay/standard",
        ])
        with self.assertRaisesRegex(ValueError, "held cases"):
            e2e.reconcile(held, {}, self.root, manifest)
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
            self.assertEqual(command[command.index("--case") + 1], key)
            self.assertEqual(command[command.index("--store") + 1], "sqlite_file")
            self.assertEqual(command[command.index("--leg") + 1], "live")
            directory = Path(command[command.index("--artifacts") + 1])
            directory.mkdir()
            e2e.write(directory / "execution.json", {
                "scenario": command[3], "label": command[2], "source_sha": SOURCE,
                "counts": {"executed": 1, "passed": 0, "failed": 1},
                "exit_code": 32,
            })
            return 32

        with patch.object(e2e.subprocess, "check_output", side_effect=[SOURCE + "\n", ""]), \
             patch.object(e2e.subprocess, "call", side_effect=runner_call):
            result = e2e.run_cases(expected, self.root, self.manifest)
        self.assertEqual(result["counts"], {"selected": 1, "executed": 1, "passed": 0, "failed": 1, "not_run": 0})
        self.assertIs(result["certified"], False)
        self.assertTrue(result["reason"])
        conclusion = json.loads((self.root / "conclusion.json").read_text())
        self.assertIs(conclusion["certified"], False)
        self.assertEqual(conclusion["reason"], result["reason"])
        self.assertTrue((self.root / "receipt.json").exists())
        held = e2e.plan(self.manifest, "b" * 64, "full", ["S28"], SOURCE)
        with self.assertRaisesRegex(ValueError, "unavailable registrations"), \
             patch.object(e2e.subprocess, "call") as command:
            e2e.run_cases(held, self.root, self.manifest)
            command.assert_not_called()

    def test_r8_runner_splits_a_case_receipt_into_reconcilable_role_files_and_names_missing_evidence(self):
        gate = e2e.GATE
        admin = "http://127.0.0.1:61046"
        key = "S30/default/sqlite_memory/live/standard"
        test_name = "s30_external_consumer_accept_follow_cancel"
        outputs = {
            name: self.root / f"built-{name}"
            for name in ("workbench", "workbench_e2e", "node", "consumer", "node_next",
                         "lashctl_n", "lashctl_next", "vm_worker", "vm_worker_next", "server")
        }
        for name, path in outputs.items():
            path.write_text(f"built {name}")
        evidence = {
            "case": "S30",
            "artifacts": [{
                "role": "external-consumer", "path": str(outputs["workbench"]),
                "sha256": e2e.digest(outputs["workbench"]),
                "candidate_sha": SOURCE, "generation": "1",
            }],
            "journals": [{
                "work": {"ingress": "k", "run": "k", "segment": "inv-1", "call": None, "ordinal": None},
                "invocation": "inv-1", "index": 0, "entry_type": "Run", "name": None,
                "value": {}, "decoded": None, "admin_url": admin, "protocol": 7,
            }],
            "native_records": [], "barriers": [], "faults": [],
            "stores": [{"binding": True}], "effects": [{"body": 1}],
            "outputs": [{"observation": "ok"}],
            "cleanup": [{"resource": "restate-1", "closed": True, "detail": "stopped"}],
            "transfers": [],
        }
        case_dir = self.root / "case-0"
        (case_dir / "case").mkdir(parents=True)
        (case_dir / "case" / "receipt.json").write_text(json.dumps({
            "counts": {"selected": 1, "executed": 1, "passed": 1, "failed": 0, "not_run": 0},
            "case": {"evidence": evidence, "verdict": "Passed"},
        }))
        junit_source = self.root / "kiln-junit.xml"
        junit_source.write_text(f'<testsuite><testcase name="{test_name}" /></testsuite>')
        base = {"scenario": test_name, "label": "//crates/lash-upgrade-harness:e2e_hosts__test",
                "source_sha": SOURCE, "gate": "law", "port_base": 61000, "generation": "1",
                "playwright": "1.62.0", "workbench": {"path": "w", "sha256": "0" * 64}}
        provenance = gate.certify_case(case_dir, junit_source, outputs, SOURCE, key, admin, base)
        self.assertIsNone(provenance["evidence_error"])
        self.assertEqual(provenance["protocol"], "V7")
        self.assertEqual(provenance["server_nodes"], 1)
        self.assertEqual(set(provenance["binaries"]), {"host", "vm_worker"})
        roles = {"journal": "journal.json", "store": "store.json", "host": "host.json",
                 "trace": "trace.json", "cleanup": "cleanup.json", "junit": "junit.xml",
                 "provenance": "provenance.json"}
        spec = {"scenario": "S30", "variant": "default", "store": "sqlite_memory",
                "leg": "live", "channel": "standard", "server_nodes": 1,
                "binaries": ["host", "vm_worker"], "artifacts": list(roles),
                "state": "ready",
                "registration": {"label": "//x:t", "test": test_name}}
        expected = {"source_sha": SOURCE, "manifest_sha256": "b" * 64, "tier": "smoke",
                    "selectors": [], "selected": 1, "cases": [spec],
                    "guarded": [], "excluded_held": [], "tier_complete": True, "held": []}
        manifest = copy.deepcopy(self.manifest)
        manifest["server"]["executable_sha256"] = provenance["server"]["artifact"]["sha256"]
        row = {"key": key, "status": "passed", "executed": True,
               "artifacts": {role: {"path": name, "sha256": e2e.digest(case_dir / name)}
                             for role, name in roles.items()},
               "quarantine": None}
        receipt = {"source_sha": SOURCE, "manifest_sha256": "b" * 64, "tier": "smoke",
                   "cases": [row], "counts": e2e.counts([row]),
                   "groups": e2e.store_leg_groups(expected["cases"], [row]),
                   "audits": {}, "gates": {}}
        self.assertEqual(e2e.reconcile(expected, receipt, case_dir, manifest)["status"], "passed")

        # The upgrade-node pair case also certifies the operator binaries.
        upgrade = self.root / "case-upgrade"
        (upgrade / "case").mkdir(parents=True)
        upgrade_receipt = json.loads((case_dir / "case" / "receipt.json").read_text())
        upgrade_receipt["case"]["evidence"]["artifacts"].append({
            "role": "synthetic-next", "path": str(outputs["node_next"]),
            "sha256": e2e.digest(outputs["node_next"]),
            "candidate_sha": SOURCE, "generation": "synthetic-next",
        })
        (upgrade / "case" / "receipt.json").write_text(json.dumps(upgrade_receipt))
        provenance = gate.certify_case(upgrade, junit_source, outputs, SOURCE, key, admin, dict(base))
        self.assertIsNone(provenance["evidence_error"])
        self.assertEqual(set(provenance["binaries"]), {
            "host", "vm_worker", "synthetic_next_host",
            "operator", "synthetic_next_operator", "synthetic_next_vm_worker",
        })

        missing = self.root / "case-1"
        (missing / "case").mkdir(parents=True)
        provenance = gate.certify_case(missing, junit_source, outputs, SOURCE, key, admin, dict(base))
        self.assertIsNotNone(provenance["evidence_error"])
        self.assertFalse((missing / "journal.json").exists())
        row = {"key": key, "status": "passed", "executed": True,
               "artifacts": {role: {"path": name, "sha256": e2e.digest(missing / name)}
                             for role, name in roles.items() if (missing / name).exists()},
               "quarantine": None}
        receipt = {"source_sha": SOURCE, "manifest_sha256": "b" * 64, "tier": "smoke",
                   "cases": [row], "counts": e2e.counts([row]),
                   "groups": e2e.store_leg_groups(expected["cases"], [row]),
                   "audits": {}, "gates": {}}
        with self.assertRaises(ValueError):
            e2e.reconcile(expected, receipt, missing, manifest)

    def test_r8_ready_selection_excludes_held_rows_and_marks_the_tier_incomplete(self):
        planned = e2e.plan(self.manifest, "b" * 64, "full", [], SOURCE, [], True)
        held_keys = {e2e.case_key({"scenario": scenario["id"], **row})
                     for scenario in self.manifest["scenarios"] for row in scenario["cases"]
                     if "full" in row["tiers"] and row["state"] == "held"}
        self.assertTrue(held_keys)
        self.assertEqual(set(planned["excluded_held"]), held_keys)
        self.assertIs(planned["tier_complete"], False)
        self.assertEqual(planned["held"], [])
        self.assertEqual(planned["selected"], len(planned["cases"]))
        with self.assertRaisesRegex(ValueError, "release certification"):
            e2e.plan(self.manifest, "b" * 64, "release", [], SOURCE, [], True)

        manifest = copy.deepcopy(self.manifest)
        for scenario in manifest["scenarios"]:
            scenario["arc_guards"] = []
        # This law needs one ready row and explicit held siblings, independent
        # of which S17 permutations the real catalogue has implemented.
        for scenario in manifest["scenarios"]:
            if scenario["id"] == "S17":
                for case in scenario["cases"]:
                    if (case["store"], case["leg"]) != ("sqlite_file", "live"):
                        case.update(state="held", hold_reason="R8 fixture: missing sibling oracle",
                                    registration=None)
        server_file = self.root / "server-bin"
        server_file.write_text("synthetic server")
        manifest["server"]["executable_sha256"] = e2e.digest(server_file)
        planned = e2e.plan(manifest, "b" * 64, "full", ["S17"], SOURCE, [], True)
        self.assertIs(planned["tier_complete"], False)
        self.assertEqual(len(planned["cases"]), 1)
        row = planned["cases"][0]
        key = e2e.case_key(row)
        test_name = row["registration"]["test"]

        def runner_call(command, **kwargs):
            self.assertEqual(command[command.index("--case") + 1], key)
            directory = Path(command[command.index("--artifacts") + 1])
            (directory / "binaries").mkdir(parents=True)
            (directory / "junit.xml").write_text(f'<testsuite><testcase name="{test_name}" /></testsuite>')
            for role in ("journal", "store", "host", "trace"):
                (directory / f"{role}.json").write_text(json.dumps({role: []}))
            e2e.write(directory / "cleanup.json", {"complete": True, "errors": [], "remaining": []})
            binaries = {}
            for name in row["binaries"]:
                binary = directory / "binaries" / name
                binary.write_text(f"synthetic {name}")
                binaries[name] = {"source_sha": SOURCE,
                                  "artifact": {"path": f"binaries/{name}", "sha256": e2e.digest(binary)}}
            server_link = directory / "binaries" / "restate-server"
            server_link.write_text("synthetic server")
            e2e.write(directory / "provenance.json", {
                "source_sha": SOURCE, "case": key, "protocol": "V7",
                "server_nodes": row["server_nodes"], "binaries": binaries,
                "server": {"version": manifest["server"]["version"],
                           "archive_sha256": manifest["server"]["archive_sha256"],
                           "artifact": {"path": "binaries/restate-server", "sha256": e2e.digest(server_link)}},
                "evidence_error": None})
            e2e.write(directory / "execution.json", {
                "scenario": test_name, "label": row["registration"]["label"], "source_sha": SOURCE,
                "counts": {"executed": 1, "passed": 1, "failed": 0}, "exit_code": 0})
            return 0

        with patch.object(e2e.subprocess, "check_output", side_effect=[SOURCE + "\n", ""]), \
             patch.object(e2e.subprocess, "call", side_effect=runner_call):
            result = e2e.run_cases(planned, self.root, manifest)
        self.assertIs(result["certified"], True)
        self.assertIs(result["tier_complete"], False)
        conclusion = json.loads((self.root / "conclusion.json").read_text())
        self.assertIs(conclusion["certified"], True)
        self.assertEqual(conclusion["excluded_held"], planned["excluded_held"])
        self.assertTrue((self.root / "receipt.json").exists())


class RunnerLaws(unittest.TestCase):
    """The executor's Unix socket must fit sun_path for every registration."""

    def test_socket_path_stays_under_107_for_the_longest_registration(self):
        registrations = [
            row["registration"]
            for scenario in e2e.load_manifest()["scenarios"]
            for row in scenario["cases"]
            if row["registration"] is not None
        ]
        label, test = max(
            ((row["label"], row["test"]) for row in registrations),
            key=lambda row: len(row[1]),
        )
        artifacts = (e2e.ROOT / "target/e2e-gate" / label.rsplit(":", 1)[-1]
                     / test / "0").resolve()
        # test_runner.py binds executor/orchestrator sockets under
        # <TMPDIR>/lash-tests-XXXXXXXX/.
        socket_path = Path(e2e.GATE.scratch_dir(artifacts)) / "lash-tests-xxxxxxxx" / "executor"
        self.assertLess(len(str(socket_path)), 107)


class WorkbenchReadinessLaws(unittest.TestCase):
    def test_initial_state_waits_for_health_within_the_case_deadline(self):
        spec = importlib.util.spec_from_file_location(
            "workbench_provider", Path(__file__).with_name("e2e-workbench-provider.py"))
        provider = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(provider)
        now = 10.0
        requests = []
        snapshot = {"settings": {"session_id": "ready-session"}, "active_turns": []}

        # A ready host's first session read can take longer than five seconds.
        # Virtual transport latency reproduces the captured socket timeout
        # without a wall-clock sleep or a live workbench.
        def delayed_response(url, *, timeout):
            nonlocal now
            requests.append((url, timeout))
            latency = 2.0 if url.endswith("/healthz") else 6.0
            if timeout < latency:
                raise TimeoutError("timed out")
            now += latency
            value = ({"service": "agent-workbench", "status": "ok"}
                     if url.endswith("/healthz") else snapshot)
            return io.StringIO(json.dumps(value))

        with patch.object(provider.time, "monotonic", side_effect=lambda: now), \
                patch.object(provider.urllib.request, "urlopen", side_effect=delayed_response):
            actual = provider.initial_state("http://workbench", deadline=30.0)
        self.assertEqual(actual, snapshot)
        self.assertEqual(requests, [("http://workbench/healthz", 20.0),
                                    ("http://workbench/api/state", 18.0)])


if __name__ == "__main__":
    unittest.main()
