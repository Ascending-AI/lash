#!/usr/bin/env python3
"""Executable fixtures for filtered runners and the store gate's shard union."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / "tools/buck2"

MEMBER = r'''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
if args.count("--list") > 1 or sum(a.split("=", 1)[0] == "--format" for a in args) > 1:
    sys.exit("a libtest option was given more than once")
names = json.loads(os.environ.get("FIXTURE_CASES", '[["law", false]]'))
filters, skips = [], []
i = 0
while i < len(args):
    arg = args[i]
    if arg in ("--skip", "--format", "--test-threads", "--color"):
        i += 1
        if arg == "--skip": skips.append(args[i])
    elif not arg.startswith("-"): filters.append(arg)
    i += 1
names = [(n, ignored) for n, ignored in names
         if (not filters or any(n == f if "--exact" in args else f in n for f in filters))
         and not any(s in n for s in skips)
         and ("--ignored" not in args or ignored)]
if "--list" in args:
    for n, _ in names: print(n + ": test")
    print(str(len(names)) + " tests, 0 benchmarks")
    sys.exit(0)
if os.environ.get("FIXTURE_EMPTY_EXECUTION") == "1": names = []
if int(os.environ.get("TEST_TOTAL_SHARDS", "0")) > 1:
    if os.environ.get("TEST_SHARD_INDEX") == "0": names = []
print("running " + str(len(names)) + " tests")
passed = ignored_count = 0
for n, ignored in names:
    skip = ignored and "--ignored" not in args and "--include-ignored" not in args
    print("test " + n + " ... " + ("ignored" if skip else "ok"))
    ignored_count += int(skip)
    passed += int(not skip)
print(f"test result: ok. {passed} passed; 0 failed; {ignored_count} ignored; 0 measured; 0 filtered out;")
'''

HERMETIC = r'''#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
filters, labels, report = [], [], None
i = 0
while i < len(args):
    arg = args[i]
    if arg in ("--test-report", "--build-report", "--test-output-dir", "--event-log"):
        if arg == "--test-report": report = pathlib.Path(args[i + 1])
        i += 2
        continue
    if arg == "--test_arg":
        filters.append(args[i + 1])
        i += 2
        continue
    if arg.startswith("--test_arg="):
        filters.append(arg.split("=", 1)[1])
    elif arg.startswith("//crates/"):
        labels.append(arg)
    i += 1
if report is None:
    sys.exit("fixture received no --test-report")
results = {}
for label in labels:
    for shard in range(2):
        xml = pathlib.Path(os.environ["FIXTURE_ROOT"], f"{len(results)}.xml")
        xml.unlink(missing_ok=True)
        env = dict(os.environ, XML_OUTPUT_FILE=str(xml), TEST_BINARY=label,
                   TEST_TOTAL_SHARDS="2", TEST_SHARD_INDEX=str(shard))
        code = subprocess.call(["bash", os.environ["FIXTURE_RUNNER"], os.environ["FIXTURE_MEMBER"], *filters], env=env)
        if code: sys.exit(code)
        results[f"{label}#{shard}"] = {"outputs": {"junit_xml": str(xml)}}
if os.environ.get("FIXTURE_NO_REPORT") != "1":
    report.parent.mkdir(parents=True, exist_ok=True)
    report.write_text(json.dumps({"schema": 1, "session_complete": True, "results": results}))
'''


CARGO = r'''#!/usr/bin/env python3
import os, sys
args = sys.argv[2:]
libtest = []
i = 0
while i < len(args):
    arg = args[i]
    if arg == "--":
        libtest.extend(args[i+1:])
        break
    if arg in ("-p", "--test", "--features"):
        i += 1
    elif not arg.startswith("-"):
        libtest.append(arg)
    i += 1
os.execv(os.environ["FIXTURE_MEMBER"], [os.environ["FIXTURE_MEMBER"], *libtest])
'''


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.member = self.root / "member"
        self.member.write_text(MEMBER)
        self.member.chmod(0o755)
        self.env = dict(os.environ, XML_OUTPUT_FILE=str(self.root / "test.xml"),
                        TEST_BINARY="fixture", TEST_TMPDIR=str(self.root))
        for key in ("TEST_TOTAL_SHARDS", "TEST_SHARD_INDEX", "TEST_SHARD_STATUS_FILE"):
            self.env.pop(key, None)
        for key in tuple(self.env):
            if key.startswith("FIXTURE_"):
                self.env.pop(key)

    def run_command(self, argv):
        return subprocess.run(argv, env=self.env, capture_output=True, text=True, timeout=30)

    def single(self, *args):
        return self.run_command(["bash", str(TOOLS / "test_xml_runner.sh"), str(self.member), *args])

    def launcher(self, *args, prefix=(), runinfo_prefix=()):
        Path(self.env["XML_OUTPUT_FILE"]).unlink(missing_ok=True)
        undeclared = self.root / "undeclared"
        undeclared.mkdir(exist_ok=True)
        self.env.update(
            LASH_TEST_EXECUTION_PREFIX_ARG_COUNT=str(len(prefix)),
            LASH_TEST_TIMEOUT_SECONDS="10",
            TEST_UNDECLARED_OUTPUTS_DIR=str(undeclared),
        )
        return self.run_command([
            "bash", str(TOOLS / "test_launcher.sh"),
            *prefix, *runinfo_prefix, str(self.member),
            "--lash-libtest-args", *args,
        ])

    def batch(self, *args):
        members = [self.member]
        if self.env.get("FIXTURE_BATCH_EMPTY_MEMBER") == "1":
            empty = self.root / "empty"
            empty.write_text(MEMBER.replace('names = json.loads', 'os.environ["FIXTURE_CASES"] = "[]"\nnames = json.loads'))
            empty.chmod(0o755)
            members.append(empty)
        self.env.update(TEST_SRCDIR=str(self.root), TEST_WORKSPACE=".", LASH_BATCH_JOBS="1")
        return self.run_command([
            "bash", str(TOOLS / "test_batch_runner.sh"), str(len(members)),
            *(part for member in members for part in ("0", str(member))), *args,
        ])

    def gate(self, suite, trusted=True, **cache_env):
        hermetic = self.root / "hermetic-build"
        hermetic.write_text(HERMETIC)
        hermetic.chmod(0o755)
        cargo = self.root / "cargo"
        cargo.write_text(CARGO)
        cargo.chmod(0o755)
        self.env.update(PATH=str(self.root) + os.pathsep + os.environ["PATH"],
                        BUCK2_TRUSTED="true" if trusted else "false",
                        HERMETIC_BUILD=str(hermetic), RUNNER_TEMP=str(self.root / "runner-temp"),
                        FIXTURE_ROOT=str(self.root), FIXTURE_MEMBER=str(self.member),
                        FIXTURE_RUNNER=str(TOOLS / "test_xml_runner.sh"))
        self.env.update(cache_env)
        return self.run_command(["bash", str(ROOT / "scripts/ci/store-tests.sh"), suite])

    def assert_failed(self, result, reason=None):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        if reason is not None:
            self.assertIn(reason, result.stdout + result.stderr)

    def assert_passed(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


class FilteredRunnerTests(Fixture):
    def test_launcher_separates_watchdog_and_shard_prefixes_from_libtest_arguments(self):
        self.assert_passed(self.launcher())
        self.assert_passed(self.launcher("law"))
        self.assert_passed(self.launcher("law", "--exact"))
        self.assert_failed(
            self.launcher("missing", "--exact"),
            "no executable tests matched the runner arguments",
        )

        injector = self.root / "inject-test-env"
        injector.write_text(
            "#!/usr/bin/env python3\n"
            "import os, sys\n"
            "os.execv(sys.argv[1], sys.argv[1:])\n",
            encoding="utf-8",
        )
        injector.chmod(0o755)
        injected = ("python3", str(injector))
        self.assert_passed(self.launcher("law", "--exact", runinfo_prefix=injected))

        self.env.update(TEST_TOTAL_SHARDS="2", TEST_SHARD_INDEX="0")
        self.assert_passed(self.launcher(
            "law", "--exact",
            prefix=("python3", str(TOOLS / "test_shard.py"), "2", "0"),
            runinfo_prefix=injected,
        ))

    def test_selector_typo_fails(self):
        self.assert_failed(self.single("law_typo"))

    def test_ignored_only_selection_fails(self):
        self.env["FIXTURE_CASES"] = '[["law", true]]'
        self.assert_failed(self.single("law"))

    def test_discovered_but_unobserved_selection_fails(self):
        self.env["FIXTURE_EMPTY_EXECUTION"] = "1"
        self.assert_failed(self.single("law"))

    def test_each_explicit_selector_must_exist(self):
        self.assert_failed(self.single("law", "law_typo"))

    def test_skip_excluding_the_selection_fails(self):
        self.assert_failed(self.single("law", "--skip", "law"))

    def test_listing_a_valid_selection_preserves_the_requested_format(self):
        self.assert_passed(self.single("law", "--list", "--format", "terse"))

    def test_listing_a_typo_fails(self):
        self.assert_failed(self.single("law_typo", "--list"))

    def test_unfiltered_empty_binary_with_output_flags_passes(self):
        self.env["FIXTURE_CASES"] = "[]"
        self.assert_passed(self.single("--nocapture", "--test-threads", "1"))
        self.assert_passed(self.batch("--nocapture"))

    def test_one_exact_case_passes(self):
        self.assert_passed(self.single("law", "--exact"))

    def test_ignored_inclusion_executes_and_passes(self):
        self.env["FIXTURE_CASES"] = '[["law", true]]'
        for flag in ("--ignored", "--include-ignored"):
            with self.subTest(flag=flag):
                self.assert_passed(self.single("law", flag))

    def test_legitimate_empty_unfiltered_binary_passes_without_synthetic_case(self):
        self.env["FIXTURE_CASES"] = "[]"
        self.assert_passed(self.single())
        suite = ET.parse(self.env["XML_OUTPUT_FILE"]).getroot()[0]
        self.assertEqual(suite.get("tests"), "0")
        self.assertEqual(list(suite.iter("testcase")), [])

    def test_non_libtest_command_with_arguments_passes(self):
        self.assert_passed(self.run_command([
            "bash", str(TOOLS / "test_xml_runner.sh"), "python3", "-c", "print('command passed')",
        ]))
        self.assertEqual(ET.parse(self.env["XML_OUTPUT_FILE"]).getroot()[0].get("tests"), "1")

    def test_non_libtest_list_command_passes(self):
        command = self.root / "command"
        command.write_text("#!/bin/sh\necho 'command listing'\n")
        command.chmod(0o755)
        self.assert_passed(self.run_command([
            "bash", str(TOOLS / "test_xml_runner.sh"), str(command), "--list",
        ]))

    def test_empty_individual_shard_passes(self):
        self.env.update(TEST_TOTAL_SHARDS="2", TEST_SHARD_INDEX="0")
        self.assert_passed(self.single("law"))

    def test_batch_ignored_only_selection_fails(self):
        self.env["FIXTURE_CASES"] = '[["law", true]]'
        self.assert_failed(self.batch("law"))

    def test_batch_each_explicit_selector_must_exist(self):
        self.assert_failed(self.batch("law", "law_typo"))

    def test_batch_one_empty_member_does_not_fail_the_union(self):
        self.env["FIXTURE_BATCH_EMPTY_MEMBER"] = "1"
        self.assert_passed(self.batch("law"))

    def test_batch_one_case_passes(self):
        self.assert_passed(self.batch("law", "--exact"))

    def test_batch_legitimate_empty_unfiltered_binary_passes(self):
        self.env["FIXTURE_CASES"] = "[]"
        self.assert_passed(self.batch())
        self.assertEqual(ET.parse(self.env["XML_OUTPUT_FILE"]).getroot()[0].get("tests"), "0")


class StoreGateTests(Fixture):
    SUITES = {
        "pg-pool-wait": ["postgres_pool_checkout_wait_is_recorded_for_runtime_store_reads"],
        "pg-sim-backend-faults": ["postgres_backend_fault_seed_set_covers_every_fault_and_oracle"],
        "pg-facade-laws": ["a_facade_law_on_postgres"],
        "s3-attachment-differential": ["attachment_blob_store_differential_agrees"],
    }

    def test_pg_s3_selector_rename_fails(self):
        for suite in self.SUITES:
            with self.subTest(suite=suite):
                self.env["FIXTURE_CASES"] = '[["renamed_law", false]]'
                self.assert_failed(self.gate(suite), "no executable tests matched the runner arguments")

    def test_missing_buck2_execution_report_fails(self):
        self.env.update(FIXTURE_CASES=json.dumps([[name, False] for name in self.SUITES["pg-pool-wait"]]),
                        FIXTURE_NO_REPORT="1")
        self.assert_failed(self.gate("pg-pool-wait"), "No such file or directory")

    def test_pg_service_gate_executes_ignored_selection(self):
        self.env["FIXTURE_CASES"] = json.dumps([[name, True] for name in self.SUITES["pg-pool-wait"]])
        self.assert_passed(self.gate("pg-pool-wait"))

    def test_pg_s3_empty_shard_union_fails(self):
        for suite, names in self.SUITES.items():
            with self.subTest(suite=suite):
                self.env.update(FIXTURE_CASES=json.dumps([[name, True] for name in names]),
                                FIXTURE_EMPTY_EXECUTION="1")
                self.assert_failed(self.gate(suite),
                                   "no non-ignored test execution observed in the selected shard union")

    def test_pg_s3_one_case_union_with_empty_shard_passes(self):
        for suite, names in self.SUITES.items():
            with self.subTest(suite=suite):
                self.env["FIXTURE_CASES"] = json.dumps([[name, True] for name in names])
                result = self.gate(suite)
                self.assert_passed(result)
                self.assertIn("PASS: 1 non-ignored test executions across 2 test results", result.stdout)


class CargoStoreGateTests(Fixture):
    SUITES = {
        "s3-attachment-differential": ["attachment_blob_store_differential_agrees"],
        "pg-facade-laws": ["a_facade_law_on_postgres"],
    }

    def test_pg_s3_untrusted_selector_rename_fails(self):
        for suite in self.SUITES:
            with self.subTest(suite=suite):
                self.env["FIXTURE_CASES"] = '[["renamed_law", false]]'
                self.assert_failed(self.gate(suite, trusted=False),
                                   "no executable tests matched the Cargo gate selection")

    def test_untrusted_ignored_only_selection_fails(self):
        """A Cargo selection that matches only ignored cases, without an ignore flag, runs nothing."""
        self.env["FIXTURE_CASES"] = '[["law", true]]'
        cargo = self.root / "cargo"
        cargo.write_text(CARGO)
        cargo.chmod(0o755)
        log = self.root / "cargo.log"
        log.write_text("")
        self.env["FIXTURE_MEMBER"] = str(self.member)
        self.assert_failed(
            self.run_command(["python3", str(TOOLS / "libtest_selection.py"), "cargo", str(log),
                              str(cargo), "test", "-p", "fixture", "law"]),
            "no executable tests matched the Cargo gate selection",
        )

    def test_pg_s3_untrusted_empty_execution_fails(self):
        for suite, names in self.SUITES.items():
            with self.subTest(suite=suite):
                # An ignored-only suite needs an ignored fixture case to reach
                # the execution check at all.
                ignored = suite == "pg-facade-laws"
                self.env.update(FIXTURE_CASES=json.dumps([[name, ignored] for name in names]),
                                FIXTURE_EMPTY_EXECUTION="1")
                self.assert_failed(self.gate(suite, trusted=False),
                                   "no non-ignored test execution observed in the selected union")

    def test_pg_s3_untrusted_one_case_passes(self):
        for suite, names in self.SUITES.items():
            with self.subTest(suite=suite):
                self.env["FIXTURE_CASES"] = json.dumps([[name, True] for name in names])
                result = self.gate(suite, trusted=False)
                self.assert_passed(result)
                for name in names:
                    self.assertIn(f"test {name} ... ok", result.stdout)


if __name__ == "__main__":
    unittest.main()
