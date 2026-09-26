#!/usr/bin/env python3
"""Unit tests for scripts/ci/restate_suite.py and scripts/restate-suites.toml."""

from __future__ import annotations

import argparse
import contextlib
import importlib.util
import io
import os
import pathlib
import socket
import stat
import sys
import tempfile
import textwrap
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("restate_suite", ROOT / "scripts" / "ci" / "restate_suite.py")
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules["restate_suite"] = MODULE
SPEC.loader.exec_module(MODULE)

# A stand-in libtest binary: it records its argv, lists four ignored tests,
# and runs a test by its name's verb; an unknown name runs nothing and exits 0.
FAKE_LIBTEST = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import os, sys, time
    with open(os.environ["FAKE_ARGV_LOG"], "a") as log:
        log.write(" ".join(sys.argv[1:]) + "\\n")
    names = ["tests::passes", "tests::fails", "tests::hangs", "tests::panics_in_background"]
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
        for name in MODULE.load_registry():
            with self.subTest(suite=name):
                suite = MODULE.load_suite(name)
                package, _, target = suite.label.removeprefix("//").partition(":")
                build_file = ROOT / package / "BUILD.bazel"
                self.assertTrue(build_file.is_file(), build_file)
                self.assertIn(f'name = "{target}"', build_file.read_text(encoding="utf-8"))
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

    def test_plain_shards_kill_a_failed_invocation_at_once(self) -> None:
        self.assertEqual("1", MODULE.RETRIES_OFF["RESTATE_DEFAULT_RETRY_POLICY__MAX_ATTEMPTS"])
        self.assertEqual("kill", MODULE.RETRIES_OFF["RESTATE_DEFAULT_RETRY_POLICY__ON_MAX_ATTEMPTS"])
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
    def test_the_workers_package_stages_every_cargo_binary(self) -> None:
        import tomllib

        manifest = tomllib.loads((ROOT / "runbooks/restate-postgres-workers/Cargo.toml").read_text(encoding="utf-8"))
        cargo_bins = {entry["name"] for entry in manifest["bin"]}
        labels = MODULE.package_binaries("//runbooks/restate-postgres-workers")
        staged = {label.rpartition(":")[2].removesuffix("__bin") for label in labels}
        self.assertEqual(cargo_bins, staged)


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


class RunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.directory.name)
        self.fake = FakeBinary(self.root)
        self.patch_env = mock.patch.dict(os.environ, {"FAKE_ARGV_LOG": str(self.fake.argv_log)})
        self.patch_env.start()

    def tearDown(self) -> None:
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

    def test_the_panic_gate_fails_a_green_law_with_a_panic_in_its_output(self) -> None:
        self.assertEqual("ok", self.run_one("tests::panics_in_background"))
        self.assertEqual("panicked", self.run_one("tests::panics_in_background", panic_gate=True))

    def test_a_registry_entry_naming_no_test_is_refused_before_any_server_starts(self) -> None:
        suite = MODULE.Suite(
            name="fake",
            label="//fake:fake",
            cwd=str(self.root),
            filters=("tests::",),
            skips=(),
            parked_crate=None,
            endpoints=(),
            env={},
            shards=1,
            timeout_seconds=5,
            redelivery_laws=("tests::renamed_long_ago",),
            panic_gate=False,
            leg_server_env={"live": {}, "replay": {}},
            replay_divergent={},
            report_only={},
        )
        args = argparse.Namespace(
            binary=str(self.fake.path),
            artifacts=str(self.root / "artifacts"),
            only=[],
            shards=None,
            timeout=None,
            include_divergent=False,
            server_env=[],
            tail_lines=5,
        )
        output = io.StringIO()
        with mock.patch.object(MODULE, "RestateServer") as server, contextlib.redirect_stdout(output):
            self.assertEqual(1, MODULE.run_suite(suite, "live", args))
        server.assert_not_called()
        self.assertIn("STALE: tests::renamed_long_ago", output.getvalue())


if __name__ == "__main__":
    unittest.main()
