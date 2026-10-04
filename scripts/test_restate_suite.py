#!/usr/bin/env python3
"""Unit tests for scripts/ci/restate_suite.py and scripts/restate-suites.toml."""

from __future__ import annotations

import argparse
import contextlib
import importlib.util
import io
import http.server
import json
import itertools
import os
import pathlib
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import textwrap
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("restate_suite", ROOT / "scripts" / "ci" / "restate_suite.py")
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules["restate_suite"] = MODULE
SPEC.loader.exec_module(MODULE)

sys.path.insert(0, str(ROOT / "scripts/ci"))
import restate_matrix


class ServerCleanupTests(unittest.TestCase):
    def test_readiness_failure_reaps_the_server_and_removes_its_data(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            work = pathlib.Path(raw)
            binary = work / "server"
            binary.write_text("#!/usr/bin/python3\nimport time\ntime.sleep(60)\n")
            binary.chmod(0o755)
            server = MODULE.RestateServer("refuses-readiness", work, {})
            try:
                with mock.patch.object(MODULE, "server_path", return_value=binary), \
                     mock.patch.object(MODULE, "http_ok", return_value=False), \
                     mock.patch.object(MODULE, "SERVER_READY_SECONDS", -1):
                    with self.assertRaisesRegex(SystemExit, "not ready"):
                        server.start()
                self.assertIsNotNone(server.process)
                self.assertIsNotNone(server.process.poll(), "the server outlived failed readiness")
                self.assertFalse(pathlib.Path(server.data_dir).exists(), "the data directory leaked")
                for reservation in server.reserved.values():
                    self.assertIsNone(reservation._socket)
            finally:
                server.stop()


class MatrixTests(unittest.TestCase):
    def test_every_registered_leg_is_reached_by_the_full_run(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())
        self.assertEqual([], restate_matrix.workflow_problems(workflow))

    def test_independent_service_jobs_allow_sixteen_slots_and_reject_overcommit(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())
        strategy = workflow["jobs"]["restate-suites"]["strategy"]
        self.assertEqual(strategy["max-parallel"], 16)
        for parallel in (1, 3, 16):
            with self.subTest(parallel=parallel):
                strategy["max-parallel"] = parallel
                self.assertEqual([], restate_matrix.workflow_problems(workflow))
        for parallel in (0, 17):
            with self.subTest(parallel=parallel):
                strategy["max-parallel"] = parallel
                self.assertIn(
                    "Restate jobs must use between one and sixteen independent service slots",
                    restate_matrix.workflow_problems(workflow),
                )

    def test_registering_a_suite_adds_both_jobs_without_other_edits(self) -> None:
        with mock.patch.object(restate_matrix, "load_registry", return_value={"new-suite": {}}):
            self.assertEqual(
                {"include": [{"suite": "new-suite", "leg": "live"}, {"suite": "new-suite", "leg": "replay"}]},
                restate_matrix.matrix(),
            )
            self.assertEqual(["unreached Restate leg: new-suite/replay"],
                             restate_matrix.coverage_problems([{"suite": "new-suite", "leg": "live"}]))

    def test_a_missing_leg_or_broken_matrix_edge_fails_the_check(self) -> None:
        import copy
        import yaml

        workflow = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())
        for mutate in (
            lambda w: w["jobs"]["restate-suites"]["strategy"].update(matrix={"include": []}),
            lambda w: w["jobs"]["plan"]["outputs"].pop("restate_matrix"),
            lambda w: w["jobs"]["plan"]["steps"].remove(next(s for s in w["jobs"]["plan"]["steps"] if s.get("id") == "restate-matrix")),
            lambda w: w["jobs"]["restate-suites"].update({"if": "false"}),
            lambda w: w["jobs"]["ci-conclusion"]["needs"].remove("restate-suites"),
            lambda w: next(s for s in w["jobs"]["restate-suites"]["steps"] if s.get("name") == "Run registered Restate leg").update({"if": "false"}),
        ):
            with self.subTest(mutation=mutate):
                changed = copy.deepcopy(workflow)
                mutate(changed)
                self.assertTrue(restate_matrix.workflow_problems(changed))
        rows = restate_matrix.matrix()["include"]
        self.assertTrue(restate_matrix.coverage_problems(rows[:-1]))
        self.assertTrue(restate_matrix.coverage_problems(rows + rows[:1]))

    def test_ci_runs_the_registered_remote_suite_entrypoint(self) -> None:
        with mock.patch.object(restate_matrix.subprocess, "call", return_value=19) as call:
            self.assertEqual(19, restate_matrix.run("effect-group", "replay"))
        command = call.call_args.args[0]
        self.assertEqual([sys.executable, str(ROOT / "scripts/ci/restate_suite.py"),
                          "suite", "effect-group", "--leg", "replay", "--keep-test-logs"], command)

    def test_the_registered_workbench_driver_retains_its_cleanup(self) -> None:
        with mock.patch.object(restate_matrix.subprocess, "call", return_value=0) as call:
            restate_matrix.run("agent-workbench", "replay")
        self.assertEqual(["bash", str(ROOT / "scripts/agent-workbench-restate-e2e.sh")], call.call_args.args[0])
        self.assertEqual("replay", call.call_args.kwargs["env"]["LASH_RESTATE_SUITE_LEG"])

    def test_remote_legs_reach_every_registered_suite_without_a_custom_driver(self) -> None:
        inventory = json.loads((ROOT / "tools/buck2/target-inventory.json").read_text())
        expected = {
            name: {leg: spec["label"].split(":", 1)[0] + ":restate_" + name.replace("-", "_") + "_" + leg
                   for leg in MODULE.LEGS}
            for name, spec in MODULE.load_registry().items() if not spec.get("ci_driver")
        }
        self.assertEqual(expected, inventory["restate_suite_targets"])
        self.assertEqual(sorted(label for legs in expected.values() for label in legs.values()),
                         inventory["service_test_targets"]["restate"])

    def test_the_suite_entrypoint_selects_the_remote_leg_and_passes_its_filters(self) -> None:
        args = argparse.Namespace(artifacts="artifacts", only=["tests::law"], shards=None,
                                  timeout=None, server_env=[], include_divergent=False)
        with mock.patch.object(MODULE.subprocess, "call", return_value=17) as call:
            self.assertEqual(17, MODULE.remote_suite(MODULE.load_suite("server-double"), "live", args))
        self.assertEqual([str(ROOT / "scripts/hermetic-build.sh"), "test",
                          "//crates/lash-restate-test:restate_server_double_live",
                          "--test-output-dir", "artifacts", "--test_arg=tests::law"], call.call_args.args[0])


# A stand-in libtest binary: it records its argv, lists four ignored tests,
# and runs a test by its name's verb; an unknown name runs nothing and exits 0.
FAKE_LIBTEST = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import os, sys, time
    with open(os.environ["FAKE_ARGV_LOG"], "a") as log:
        log.write(" ".join(sys.argv[1:]) + "\\n")
    names = [
        "tests::passes",
        "tests::fails",
        "tests::hangs",
        "tests::panics_in_background",
        "tests::progresses_past_its_bound",
        "tests::progresses_then_hangs",
        "tests::chatters_without_progress",
    ]
    if "--list" in sys.argv:
        chosen = sys.argv[1]
        for name in names:
            if chosen in name:
                print(f"{name}: test")
        sys.exit(0)
    name = sys.argv[1]
    if name not in names:
        print("test result: ok. 0 passed; 0 failed; 4 filtered out")
        sys.exit(0)
    marker = "[restate-suite progress] "
    if name.endswith("progresses_past_its_bound"):
        # Twelve steps a quarter second apart: three seconds of work, every
        # step well inside a one-second bound.
        for step in range(12):
            time.sleep(0.25)
            print(f"{marker}step {step}", flush=True)
    if name.endswith("progresses_then_hangs"):
        print(f"{marker}step 0", flush=True)
        time.sleep(60)
    if name.endswith("chatters_without_progress"):
        for _ in range(240):
            print("retrying: connection refused", flush=True)
            time.sleep(0.25)
    if name.endswith("hangs"):
        time.sleep(60)
    if name.endswith("fails"):
        print("test result: FAILED. 0 passed; 1 failed")
        sys.exit(101)
    if name.endswith("panics_in_background"):
        print("thread 'tokio-rt-worker' panicked at src/lib.rs:1:1:")
    print("test result: ok. 1 passed; 0 failed")
    """
)


class FakeBinary:
    def __init__(self, directory: pathlib.Path) -> None:
        self.path = directory / "fake-libtest"
        self.path.write_text(FAKE_LIBTEST, encoding="utf-8")
        self.path.chmod(self.path.stat().st_mode | stat.S_IEXEC)
        self.argv_log = directory / "argv.log"
        self.argv_log.write_text("", encoding="utf-8")
        self.env = {**os.environ, "FAKE_ARGV_LOG": str(self.argv_log)}

    def calls(self) -> list[list[str]]:
        return [line.split() for line in self.argv_log.read_text(encoding="utf-8").splitlines()]


class RegistryTests(unittest.TestCase):
    def test_every_registered_suite_names_a_label_and_a_directory_that_exist(self) -> None:
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
        )
        labels = {
            target["label"]
            for package in inventory["packages"]
            for target in package["targets"]
            if isinstance(target.get("label"), str)
        }
        # A suite may build a feature lane's test binary (a `__fv_` label):
        # those live in the inventory's feature-lane units, not the packages.
        labels.update(
            unit["label"]
            for unit in inventory["feature_lane_units"]
            if isinstance(unit.get("label"), str)
        )
        for name in MODULE.load_registry():
            with self.subTest(suite=name):
                suite = MODULE.load_suite(name)
                self.assertIn(suite.label, labels)
                self.assertTrue((ROOT / suite.cwd).is_dir())
                self.assertTrue(suite.filters)
                self.assertGreaterEqual(suite.shards, 1)

    def test_every_replay_divergence_names_the_ticket_that_brings_it_back(self) -> None:
        for name in MODULE.load_registry():
            for law, reason in MODULE.load_suite(name).replay_divergent.items():
                with self.subTest(law=law):
                    self.assertRegex(reason, r"\(FIG-\d+\)$")

    def test_a_report_only_leg_says_why(self) -> None:
        for name in MODULE.load_registry():
            for leg, reason in MODULE.load_suite(name).report_only.items():
                with self.subTest(suite=name, leg=leg):
                    self.assertIn(leg, MODULE.LEGS)
                    self.assertRegex(reason, r"FIG-\d+")

    def test_legs_differ_only_in_the_inactivity_timeout(self) -> None:
        self.assertEqual({}, MODULE.LEGS["live"])
        self.assertEqual({"RESTATE_WORKER__INVOKER__INACTIVITY_TIMEOUT": "0s"}, MODULE.LEGS["replay"])

    def test_every_shard_retries_on_a_short_bounded_schedule(self) -> None:
        self.assertEqual("30", MODULE.RETRIES_BOUNDED["RESTATE_DEFAULT_RETRY_POLICY__MAX_ATTEMPTS"])
        self.assertEqual("kill", MODULE.RETRIES_BOUNDED["RESTATE_DEFAULT_RETRY_POLICY__ON_MAX_ATTEMPTS"])


class DivergenceShardTests(unittest.TestCase):
    """Per-ticket shards fold into the registry; a malformed shard is refused."""

    REGISTRY = """\
[suites.fake]
label = "//fake:fake"
cwd = "."
filters = ["tests::"]
"""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self._tmp.name)
        self._old_registry = MODULE.REGISTRY
        self._old_dir = MODULE.DIVERGENCE_DIR
        MODULE.REGISTRY = self.root / "restate-suites.toml"
        MODULE.REGISTRY.write_text(self.REGISTRY, encoding="utf-8")
        MODULE.DIVERGENCE_DIR = self.root / "restate-divergences"
        MODULE.DIVERGENCE_DIR.mkdir()

    def tearDown(self) -> None:
        MODULE.REGISTRY = self._old_registry
        MODULE.DIVERGENCE_DIR = self._old_dir
        self._tmp.cleanup()

    def shard(self, name: str, text: str) -> None:
        (MODULE.DIVERGENCE_DIR / name).write_text(text, encoding="utf-8")

    def test_a_ticket_shard_folds_into_its_suite(self) -> None:
        self.shard(
            "FIG-9.toml",
            '[suites.fake.replay.divergent]\n"tests::held" = "why (FIG-9)"\n',
        )
        self.assertEqual(
            MODULE.load_suite("fake").replay_divergent,
            {"tests::held": "why (FIG-9)"},
        )

    def test_a_shard_naming_a_suite_the_registry_lacks_is_refused(self) -> None:
        self.shard(
            "FIG-9.toml",
            '[suites.gone.replay.divergent]\n"tests::held" = "why (FIG-9)"\n',
        )
        with self.assertRaises(SystemExit):
            MODULE.load_registry()

    def test_a_duplicate_divergence_is_refused(self) -> None:
        for shard in ("FIG-8.toml", "FIG-9.toml"):
            self.shard(
                shard,
                f'[suites.fake.replay.divergent]\n"tests::held" = "why ({shard.removesuffix(".toml")})"\n',
            )
        with self.assertRaises(SystemExit):
            MODULE.load_registry()

    def test_a_reason_must_name_its_shard_ticket(self) -> None:
        self.shard(
            "FIG-9.toml",
            '[suites.fake.replay.divergent]\n"tests::held" = "why (FIG-8)"\n',
        )
        with self.assertRaises(SystemExit):
            MODULE.load_registry()

    def test_a_shard_carries_only_divergent_tables(self) -> None:
        self.shard(
            "FIG-9.toml",
            '[suites.fake.replay]\nreport_only = "sneaky (FIG-9)"\n',
        )
        with self.assertRaises(SystemExit):
            MODULE.load_registry()


class StageBinariesTests(unittest.TestCase):

    def test_cargo_segment_artifacts_include_the_vm_worker(self) -> None:
        import yaml

        workflow = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())
        producer = workflow["jobs"]["worker-artifacts"]
        build = next(step["run"] for step in producer["steps"] if step["name"] == "Build worker binaries once")
        self.assertIn("cargo build --locked --release -p lash-internal-vm-worker --bin lash-vm-worker", build)
        self.assertIn('cp target/release/lash-vm-worker "${RUNNER_TEMP}/worker-artifacts/"', build)

    def test_segment_hosts_mount_the_staged_vm_worker(self) -> None:
        import yaml

        compose = yaml.safe_load((ROOT / "runbooks/restate-postgres-workers/docker-compose.yml").read_text())
        for name in ("worker-a", "worker-b", "runner"):
            with self.subTest(service=name):
                mounts = compose["services"][name]["volumes"]
                self.assertTrue(any("/lash-vm-worker:/usr/local/bin/lash-vm-worker:ro" in mount for mount in mounts))

    def test_the_workers_package_stages_every_cargo_binary(self) -> None:
        import tomllib

        manifest = tomllib.loads((ROOT / "runbooks/restate-postgres-workers/Cargo.toml").read_text(encoding="utf-8"))
        cargo_bins = {entry["name"] for entry in manifest["bin"]}
        binaries = MODULE.package_binaries("//runbooks/restate-postgres-workers")
        self.assertEqual(cargo_bins, set(binaries.values()))

    def test_staging_names_each_binary_by_its_cargo_name_not_its_crate_output(self) -> None:
        # Buck2 names a binary's output after its crate, with underscores.
        def build(labels):
            outputs = []
            for label in labels:
                crate = label.rpartition(":")[2].removesuffix("__bin").replace("-", "_")
                output = pathlib.Path(built_dir, crate)
                output.write_bytes(b"\x7fELF")
                outputs.append(output)
            return outputs

        with tempfile.TemporaryDirectory() as built_dir, tempfile.TemporaryDirectory() as stage:
            with mock.patch.object(MODULE, "build", side_effect=build), mock.patch.object(MODULE.subprocess, "run"):
                staged = MODULE.stage_binaries("//runbooks/restate-postgres-workers", pathlib.Path(stage))
            names = sorted(path.name for path in staged)
            self.assertEqual(sorted(os.listdir(stage)), names)
            self.assertIn("lash-e2e-worker", names)
            self.assertIn("lash-vm-worker", names)
            self.assertFalse([name for name in names if "_" in name])
            for path in staged:
                self.assertTrue(os.access(path, os.X_OK), path)


class ReservationTests(unittest.TestCase):
    """A reserved port stays bound -- unbindable by anyone else -- until claimed."""

    def test_a_held_reservation_refuses_a_second_bind(self) -> None:
        reservation = MODULE.ReservedPort()
        try:
            with (
                self.assertRaises(OSError),
                socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe,
            ):
                probe.bind(("127.0.0.1", reservation.port))
        finally:
            reservation.close()

    def test_reservations_never_hand_out_a_port_another_holds(self) -> None:
        held = [MODULE.ReservedPort() for _ in range(16)]
        try:
            self.assertEqual(len({reservation.port for reservation in held}), len(held))
        finally:
            for reservation in held:
                reservation.close()

    def test_a_claimed_port_binds_again_and_claims_once(self) -> None:
        reservation = MODULE.ReservedPort()
        port = reservation.claim()
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", port))
        with self.assertRaises(RuntimeError):
            reservation.claim()


    def test_a_gate_port_base_binds_the_gates_own_block(self) -> None:
        server = MODULE.RestateServer("gate", pathlib.Path(tempfile.gettempdir()), {}, port_base=61230)
        self.assertEqual(
            {"ingress": 61230, "admin": 61231, "node": 61232},
            {role: port.claim() for role, port in server.reserved.items()},
        )


class RunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.directory.name)
        self.fake = FakeBinary(self.root)
        self.rows = []
        self.killed = []
        self.admin_failure = False
        owner = self

        class Admin(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                owner.assertEqual("/query", self.path)
                query = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                owner.assertIn("status != 'completed'", query["query"])
                owner.assertTrue(owner.fake.calls(), "the law must exit before its census")
                self.send_response(503 if owner.admin_failure else 200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps({"rows": owner.rows}).encode())

            def do_PATCH(self):
                invocation = self.path.split("/")[2]
                owner.assertEqual(f"/invocations/{invocation}/kill", self.path)
                owner.killed.append(invocation)
                owner.rows = [row for row in owner.rows if row["id"] != invocation]
                self.send_response(200)
                self.end_headers()

            def log_message(self, *args):
                pass

        self.admin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Admin)
        self.admin_thread = threading.Thread(target=self.admin.serve_forever, kwargs={"poll_interval": 0.01})
        self.admin_thread.start()
        self.fake.env["RESTATE_ADMIN_URL"] = f"http://127.0.0.1:{self.admin.server_port}"
        self.patch_env = mock.patch.dict(os.environ, {
            "FAKE_ARGV_LOG": str(self.fake.argv_log),
            "LASH_VM_WORKER": str(self.fake.path),
            "RESTATE_ADMIN_URL": self.fake.env["RESTATE_ADMIN_URL"],
        })
        self.patch_env.start()

    def tearDown(self) -> None:
        self.admin.shutdown()
        self.admin_thread.join()
        self.admin.server_close()
        self.patch_env.stop()
        self.directory.cleanup()

    def run_one(self, name: str, *, timeout: float = 20, panic_gate: bool = False) -> str:
        status, _ = MODULE.run_one(
            self.fake.path, self.root, name, self.fake.env, timeout, self.root / "out.log", panic_gate
        )
        return status

    def test_listing_and_running_ask_for_ignored_tests_only(self) -> None:
        self.assertEqual(["tests::passes"], MODULE.list_tests(self.fake.path, self.root, ["passes"], []))
        self.run_one("tests::passes")
        listing, run = self.fake.calls()
        self.assertIn("--ignored", listing)
        self.assertIn("--ignored", run)
        self.assertIn("--exact", run)
        self.assertNotIn("--include-ignored", run)

    def test_a_law_is_ok_only_when_it_reports_one_pass(self) -> None:
        self.assertEqual("ok", self.run_one("tests::passes"))
        self.assertEqual("failed", self.run_one("tests::fails"))
        # A name that matched nothing exits 0 with nothing run.
        self.assertEqual("failed", self.run_one("tests::absent"))

    def test_a_hung_law_is_killed_at_its_bound(self) -> None:
        self.assertEqual("timeout", self.run_one("tests::hangs", timeout=1))

    def test_a_law_that_keeps_progressing_outlasts_its_bound(self) -> None:
        # The bound is time without progress: three seconds of steps, each
        # well inside a one-second bound, is not a hang (FIG-4309).
        self.assertEqual("ok", self.run_one("tests::progresses_past_its_bound", timeout=1))

    def test_a_law_that_stops_progressing_is_killed_at_its_bound(self) -> None:
        self.assertEqual("timeout", self.run_one("tests::progresses_then_hangs", timeout=1))

    def test_output_that_is_not_a_progress_marker_does_not_restart_the_bound(self) -> None:
        self.assertEqual("timeout", self.run_one("tests::chatters_without_progress", timeout=1))

    def test_the_panic_gate_fails_a_green_law_with_a_panic_in_its_output(self) -> None:
        self.assertEqual("ok", self.run_one("tests::panics_in_background"))
        self.assertEqual("panicked", self.run_one("tests::panics_in_background", panic_gate=True))

    def test_leftovers_fail_a_passing_law_and_are_named_and_killed_before_the_next(self) -> None:
        self.rows = [
            dict(id="inv_b", target="Flow/b/run", status="suspended", retry_count=0, last_failure=None),
            dict(id="inv_a", target="Flow/a/run", status="backing-off", retry_count=3, last_failure="broken"),
        ]
        self.assertEqual("leftovers", self.run_one("tests::passes"))
        self.assertEqual(["inv_a", "inv_b"], self.killed)
        output = (self.root / "out.log").read_text()
        self.assertIn("tests::passes", output)
        self.assertIn("Flow/a/run", output)
        self.assertIn("backing-off", output)
        self.assertIn("attempt=4", output)
        self.assertIn("broken", output)
        self.assertLess(output.index("inv_a"), output.index("inv_b"))
        self.assertEqual("ok", self.run_one("tests::passes"))

    def test_leftovers_are_killed_after_a_law_panics(self) -> None:
        self.rows = [dict(id="inv_failed", target="Flow/fail/run", status="running")]
        self.assertEqual("leftovers", self.run_one("tests::fails"))
        self.assertEqual(["inv_failed"], self.killed)

    def test_leftovers_are_killed_after_a_law_times_out(self) -> None:
        self.rows = [dict(id="inv_timeout", target="Flow/hang/run", status="running")]
        self.assertEqual("leftovers", self.run_one("tests::hangs", timeout=0.1))
        self.assertEqual(["inv_timeout"], self.killed)

    def test_an_admin_failure_cannot_certify_a_clean_law(self) -> None:
        self.admin_failure = True
        with self.assertRaisesRegex(RuntimeError, "tests::passes.*teardown"):
            self.run_one("tests::passes")

    def suite(self, replay_divergent: dict[str, str]) -> object:
        return MODULE.Suite(
            name="fake",
            label="//fake:fake",
            cwd=str(self.root),
            filters=("tests::",),
            skips=(),
            endpoints=(),
            env={},
            shards=1,
            timeout_seconds=5,
            panic_gate=False,
            leg_server_env={"live": {}, "replay": {}},
            replay_divergent=replay_divergent,
            report_only={},
        )

    def args(self, **overrides) -> argparse.Namespace:
        return argparse.Namespace(
            **{
                **dict(
                    binary=str(self.fake.path),
                    artifacts=str(self.root / "artifacts"),
                    only=[],
                    shards=None,
                    timeout=None,
                    include_divergent=False,
                    keep_test_logs=False,
                    server_env=[],
                    tail_lines=5,
                ),
                **overrides,
            }
        )

    def test_a_registry_entry_naming_no_test_is_refused_before_any_server_starts(self) -> None:
        suite = self.suite({"tests::renamed_long_ago": "held back (FIG-9)"})
        output = io.StringIO()
        with mock.patch.object(MODULE, "RestateServer") as server, contextlib.redirect_stdout(output):
            self.assertEqual(1, MODULE.run_suite(suite, "replay", self.args()))
        server.assert_not_called()
        self.assertIn("STALE: tests::renamed_long_ago", output.getvalue())

    def test_a_held_law_that_passes_fails_the_replay_leg(self) -> None:
        suite = self.suite({"tests::passes": "held back (FIG-9)"})
        output = io.StringIO()
        with mock.patch.object(MODULE, "RestateServer"), contextlib.redirect_stdout(output):
            self.assertEqual(1, MODULE.run_suite(suite, "replay", self.args(only=["tests::passes"])))
        self.assertIn("tests::passes: held law now passes: remove it from ", output.getvalue())
        self.assertIn("FIG-9.toml", output.getvalue())

    def test_a_held_law_that_still_fails_does_not_fail_the_replay_leg(self) -> None:
        suite = self.suite({"tests::fails": "held back (FIG-9)"})
        output = io.StringIO()
        with mock.patch.object(MODULE, "RestateServer"), contextlib.redirect_stdout(output):
            self.assertEqual(0, MODULE.run_suite(suite, "replay", self.args(only=["tests::fails"])))
        self.assertIn("held", output.getvalue())

    def test_a_replay_divergence_cannot_excuse_leftovers(self) -> None:
        self.rows = [dict(id="inv_held", target="Flow/held/run", status="suspended")]
        suite = self.suite({"tests::fails": "held back (FIG-9)"})
        with mock.patch.object(MODULE, "RestateServer"), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(1, MODULE.run_suite(suite, "replay", self.args(only=["tests::fails"])))
        self.assertEqual(["inv_held"], self.killed)

    def test_a_failed_teardown_is_attributed_and_stops_the_shard(self) -> None:
        self.admin_failure = True
        with mock.patch.object(MODULE, "RestateServer"), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(1, MODULE.run_suite(self.suite({}), "live", self.args(only=["passes", "fails"])))
        summary = json.loads((self.root / "artifacts/fake-live/summary.json").read_text())
        self.assertEqual(["tests::passes"], [test["name"] for test in summary["tests"]])
        self.assertEqual("teardown_failed", summary["tests"][0]["status"])
        self.assertEqual(["tests::fails"], summary["not_run"])
        runs = [call[0] for call in self.fake.calls() if "--list" not in call]
        self.assertEqual(["tests::passes"], runs)

    def test_a_held_law_failing_the_live_leg_fails_the_run(self) -> None:
        suite = self.suite({"tests::fails": "held back (FIG-9)"})
        with mock.patch.object(MODULE, "RestateServer"), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(1, MODULE.run_suite(suite, "live", self.args(only=["tests::fails"])))

    def test_include_divergent_gates_held_laws_ordinarily(self) -> None:
        suite = self.suite({"tests::passes": "held back (FIG-9)"})
        output = io.StringIO()
        with mock.patch.object(MODULE, "RestateServer"), contextlib.redirect_stdout(output):
            self.assertEqual(
                0,
                MODULE.run_suite(
                    suite, "replay", self.args(only=["tests::passes"], include_divergent=True)
                ),
            )
        self.assertNotIn("held law now passes", output.getvalue())


if __name__ == "__main__":
    unittest.main()
