#!/usr/bin/env python3
"""Exercise selection, concurrent serialization and stale-snapshot refusal with a fake executor."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest


SOURCE = Path(__file__).with_name("dev-test.py")


def dev_test_module():
    """`dev-test.py` loaded as a module, for its pure summary helpers."""
    spec = importlib.util.spec_from_file_location("dev_test_script", SOURCE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class DevTestTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("LASH_", "KILN_", "BUCK2_", "CARGO_", "GIT_"))
        }
        self.env.update({
            "GIT_AUTHOR_NAME": "Test", "GIT_AUTHOR_EMAIL": "test@example.test",
            "GIT_COMMITTER_NAME": "Test", "GIT_COMMITTER_EMAIL": "test@example.test",
        })
        (self.root / "scripts").mkdir()
        shutil.copyfile(SOURCE, self.root / "scripts/dev-test.py")
        shutil.copyfile(SOURCE.with_name("ci_plan.py"), self.root / "scripts/ci_plan.py")
        (self.root / "scripts/ci").mkdir()
        shutil.copyfile(SOURCE.parent / "ci/repository_gate_commands.py",
                        self.root / "scripts/ci/repository_gate_commands.py")
        self.source = self.root / "crates/example/src/lib.rs"
        self.source.parent.mkdir(parents=True)
        self.source.write_text("pub fn example() {}\n")
        (self.source.parents[1] / "BUCK").write_text("# fixture\n")
        inventory = self.root / "tools/buck2/target-inventory.json"
        inventory.parent.mkdir(parents=True)
        inventory.write_text(json.dumps({
            "workspace_dev_test_targets": [
                "//crates/example:first",
                "//crates/example:second",
                "//crates/dependent:dependent__test",
            ],
            "workspace_test_batches": {"//crates/example:test_batch": ["//crates/example:first", "//crates/example:second"]},
            "packages": [
            {"manifest": "crates/example/Cargo.toml", "targets": [
                {"kind": "test", "label": "//crates/example:first", "build_label": "//crates/example:first", "tags": []},
                {"kind": "test", "label": "//crates/example:second", "build_label": "//crates/example:second", "tags": []},
            ]},
            {"manifest": "crates/dependent/Cargo.toml", "targets": [
                {"kind": "test", "label": "//crates/dependent:dependent__test", "build_label": "//crates/dependent:dependent__test", "tags": []},
            ]},
            {"manifest": "crates/slow/Cargo.toml", "targets": [
                {"kind": "test", "label": "//crates/slow:slow__test", "build_label": "//crates/slow:slow__test", "tags": ["dev-deferred"]},
                {"kind": "test", "label": "//crates/slow:service__test", "build_label": "//crates/slow:service__test", "tags": ["manual"]},
                {"kind": "test", "label": "//crates/slow:trunk__test", "build_label": "//crates/slow:trunk__test", "tags": ["pr-deferred"]},
            ]},
            {"manifest": "examples/sample/Cargo.toml", "targets": [
                {"kind": "test", "label": "//examples/sample:leaf__test", "build_label": "//examples/sample:leaf__test", "tags": ["dev-deferred"]},
            ]},
        ]}))
        # A package whose only Buck2 test label is dev-deferred.
        slow = self.root / "crates/slow/src"
        slow.mkdir(parents=True)
        (slow / "lib.rs").write_text("pub fn slow() {}\n")
        (self.root / "crates/slow/BUCK").write_text("# fixture\n")
        workflow = self.root / ".github/workflows/ci.yml"
        workflow.parent.mkdir(parents=True)
        workflow.write_text("bash scripts/ci/run-gate-commands.sh --jobs 4 <<'GATES'\n"
                            "python3 scripts/test_dev_test.py\nGATES\n")
        (self.root / "scripts/test_dev_test.py").write_text("raise SystemExit(0)\n")
        (self.root / ".gitignore").write_text(".buckconfig.local\n")
        (self.root / ".buckconfig.local").write_text(
            "[buck2_re_client]\n"
            "engine_address = grpcs://executor.fixture\n"
            "tls_client_cert = .kiln/client.pem\n"
        )
        (self.root / "tools/buck2/bootstrap.py").write_text(
            "from pathlib import Path\n"
            "print(Path(__file__).resolve().parents[2] / '.git/bin/buck2')\n"
        )
        self.git("init", "-q")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.source.write_text("pub fn example() { let _ = 1; }\n")
        self.bin = self.root / ".git/bin"
        self.bin.mkdir()
        query = self.bin / "buck2"
        query.write_text("#!/bin/sh\nexit 1\n")
        query.chmod(0o755)
        executor = self.bin / "kiln"
        executor.write_text(
            '#!/usr/bin/env python3\nimport os,time,sys\nfrom pathlib import Path\n'
            'root=Path.cwd()/".git"\n'
            'with (root/"calls").open("a") as f: f.write("run\\n")\n'
            '(root/"pid").write_text(str(os.getpid()))\n'
            '(root/"started").touch()\n'
            'canned=os.environ.get("TEST_STDOUT_FILE")\n'
            'if canned: sys.stdout.write(Path(canned).read_text())\n'
            'if "--test-report" in sys.argv:\n'
            ' report=Path(sys.argv[sys.argv.index("--test-report")+1])\n'
            ' fixture=os.environ.get("TEST_REPORT_FILE")\n'
            ' report.write_text(Path(fixture).read_text() if fixture else \'{"schema":1,"results":{}}\\n\')\n'
            'while (root/"hold").exists(): time.sleep(0.01)\n'
            'status=os.environ.get("TEST_EXIT", "0") if len(sys.argv)>1 and sys.argv[1]=="test" else "0"\n'
            'raise SystemExit(int(status))\n'
        )
        executor.chmod(0o755)
        self.env["PATH"] = str(self.bin) + os.pathsep + self.env["PATH"]

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, env=self.env)

    def command(self, *args):
        return ["python3", str(self.root / "scripts/dev-test.py"), *args]

    def invoke(self, *args):
        return subprocess.run(self.command(*args), cwd=self.root, env=self.env,
                              capture_output=True, text=True, timeout=20)

    def start(self):
        process = subprocess.Popen(self.command(), cwd=self.root, env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: process.kill() if process.poll() is None else None)
        return process

    def wait_started(self):
        deadline = time.monotonic() + 10
        while not (self.root / ".git/started").exists():
            if time.monotonic() > deadline:
                self.fail("executor never started")
            time.sleep(0.01)

    def test_worktree_edits_and_shared_manifest_selection(self):
        result = self.invoke("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//crates/example:test_batch"],
        ])
        (self.source.parents[1] / "Cargo.toml").write_text("[package]\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//:dev_tests"],
        ])

    def test_a_package_diff_also_selects_its_dev_deferred_labels(self):
        (self.root / "crates/slow/src/lib.rs").write_text("pub fn slow() { let _ = 1; }\n")
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        self.assertEqual(commands, [[
            "kiln", "build", "//:schema_checks",
        ], [
            "kiln", "test",
            "//crates/example:test_batch",
            "//crates/slow:slow__test",
        ]])
        # The manual and pr-deferred labels of the same package stay out; so
        # does the dev-deferred examples leaf. A package manifest widens to
        # the suite but is still a deferred test's input.
        (self.root / "crates/slow/Cargo.toml").write_text("[package]\n")
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        self.assertEqual(commands, [[
            "kiln", "build", "//:schema_checks",
        ], [
            "kiln", "test",
            "//:dev_tests",
            "//crates/slow:slow__test",
        ]])

    def test_query_cannot_select_deferred_manual_or_duplicate_batch_members(self):
        query = self.bin / "buck2"
        query.write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" > .git/query-args\nprintf '%s\\n' root//crates/example:first root//crates/example:second "
                         "root//crates/dependent:dependent__test root//crates/example:test_batch root//crates/example:deferred root//crates/example:manual\n")
        query.chmod(0o755)
        commands = json.loads(self.invoke("--dependents", "--dry-run").stdout)["commands"]
        self.assertEqual(commands, [
            ["kiln", "build", "//:schema_checks"],
            [
                "kiln",
                "test",
                "//crates/dependent:dependent__test",
                "//crates/example:test_batch",
            ],
        ])
        self.assertIn(
            "rdeps(set(//crates/... //examples/... //runbooks/...),",
            (self.root / ".git/query-args").read_text(),
        )
        query.write_text("#!/bin/sh\necho root//crates/example:first\n")
        self.assertEqual(json.loads(self.invoke("--dependents", "--dry-run").stdout)["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//crates/example:first"],
        ])

    def test_known_script_edit_runs_its_ci_proof_and_propagates_failure(self):
        self.git("checkout", "--", "crates/example/src/lib.rs")
        (self.root / "scripts/test_dev_test.py").write_text("raise SystemExit(7)\n")
        planned = json.loads(self.invoke("--dry-run").stdout)
        self.assertEqual(planned["commands"], [["python3", "scripts/test_dev_test.py"]])
        self.assertEqual(self.invoke().returncode, 7)
        self.assertFalse((self.root / ".git/calls").exists())
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertEqual(receipt["exit_code"], 7)

    def test_shared_tooling_runs_repository_proof_and_rust_suite(self):
        (self.root / "scripts/unknown.py").write_text("# new tooling\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["bash", "scripts/ci/repository-gates.sh"],
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//:dev_tests"],
        ])

    def test_service_only_package_builds_without_running_manual_tests(self):
        self.git("checkout", "--", "crates/example/src/lib.rs")
        package = self.root / "crates/service"
        package.mkdir()
        (package / "BUCK").write_text("# fixture\n")
        inventory = json.loads((self.root / "tools/buck2/target-inventory.json").read_text())
        inventory["packages"].append({
            "manifest": "crates/service/Cargo.toml",
            "targets": [{"kind": "lib", "label": "//crates/service:service", "build_label": "//crates/service:service[static]"}],
        })
        (self.root / "tools/buck2/target-inventory.json").write_text(json.dumps(inventory))
        self.git("add", "crates/service/BUCK", "tools/buck2/target-inventory.json")
        self.git("commit", "-qm", "service fixture")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        (package / "service.rs").write_text("// service change\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["kiln", "build", "//crates/service:service[static]", "//:schema_checks"],
        ])
        self.source.write_text("pub fn example() { let _ = 2; }\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["kiln", "build", "//crates/service:service[static]", "//:schema_checks"],
            ["kiln", "test", "//crates/example:test_batch"],
        ])

    def test_facade_keeps_explicit_manual_seal(self):
        (self.root / "Cargo.toml").write_text("[workspace]\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//:dev_tests", "//crates/lash:ui_fixtures",
             "//crates/lash:facade_completeness"],
        ])

    def test_untracked_content_changes_identity(self):
        untracked = self.root / "crates/example/src/new.rs"
        untracked.write_text("first")
        before = json.loads(self.invoke("--dry-run").stdout)["inputs"]
        untracked.write_text("second")
        self.assertNotEqual(before, json.loads(self.invoke("--dry-run").stdout)["inputs"])

    def test_failed_query_widens_selection(self):
        query = self.bin / "buck2"
        query.write_text("#!/bin/sh\nexit 7\n")
        query.chmod(0o755)
        result = self.invoke("--dependents", "--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        planned = json.loads(result.stdout)
        # Query failure still runs the whole dev suite, leaving deferred tests
        # to main unless explicitly requested.
        self.assertEqual(planned["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//:dev_tests"],
        ])
        self.assertEqual(planned["skipped_deferred"], ["//crates/slow:slow__test"])
        included = json.loads(self.invoke("--dependents", "--include-deferred", "--dry-run").stdout)
        self.assertEqual(included["commands"][-1], [
            "kiln", "test", "//:dev_tests", "//crates/slow:slow__test",
        ])
        self.assertEqual(included["skipped_deferred"], [])
        self.assertEqual(planned["selection"], "suite")
        self.assertIn("reverse-dependency query failed", result.stderr)

    def test_dependents_skip_deferred_reverse_dependencies_unless_included(self):
        query = self.bin / "buck2"
        query.write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > .git/query-args\n"
            "printf '%s\\n' 'root//crates/example:first (cfg:linux#0123)' "
            "'root//crates/dependent:dependent__test (cfg:linux#0123)' "
            "'root//crates/slow:slow__test (cfg:linux#0123)' "
            "'root//crates/slow:service__test (cfg:linux#0123)' "
            "'root//crates/slow:trunk__test (cfg:linux#0123)' "
            "'root//examples/sample:leaf__test (cfg:linux#0123)' "
            "'root//crates/slow:slow__test__fv_0a1b2c3d (cfg:linux#0123)'\n")
        query.chmod(0o755)
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//crates/dependent:dependent__test",
             "//crates/example:first"],
        ])
        self.assertEqual(planned["skipped_deferred"], ["//crates/slow:slow__test"])
        included = json.loads(self.invoke("--dependents", "--include-deferred", "--dry-run").stdout)
        self.assertEqual(included["commands"][-1], [
            "kiln", "test", "//crates/dependent:dependent__test",
            "//crates/example:first", "//crates/slow:slow__test",
        ])
        self.assertEqual(included["skipped"], planned["skipped"])
        self.assertEqual(included["skipped_deferred"], [])
        self.assertNotEqual(included["id"], planned["id"])
        self.assertEqual(planned["skipped"], [
            "//crates/slow:service__test",
            "//crates/slow:trunk__test",
            "//examples/sample:leaf__test",
        ])
        # The configured query runs in the daemon `kiln test` uses.
        arguments = (self.root / ".git/query-args").read_text()
        self.assertTrue(arguments.startswith("--isolation-dir kiln cquery "), arguments)
        result = self.invoke("--dependents")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("dev-test: SKIPPED 3 affected tests", result.stdout)
        self.assertIn("//crates/slow:service__test //crates/slow:trunk__test", result.stdout)
        self.assertIn("dev-test: skipped 1 dev-deferred targets (run hourly on main): "
                      "//crates/slow:slow__test", result.stdout)
        result = self.invoke("--dependents", "--include-deferred")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("dev-deferred targets", result.stdout)
        # Without `--dependents` nothing was queried, so nothing is named.
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["skipped"], [])

    def test_broad_dependents_skip_the_tail_and_the_flag_restores_it(self):
        (self.root / "scripts/unknown.py").write_text("# shared tooling\n")
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"][-1], ["kiln", "test", "//:dev_tests"])
        self.assertEqual(planned["skipped_deferred"], ["//crates/slow:slow__test"])
        included = json.loads(self.invoke("--dependents", "--include-deferred", "--dry-run").stdout)
        self.assertEqual(included["commands"][-1], [
            "kiln", "test", "//:dev_tests", "//crates/slow:slow__test",
        ])
        self.assertEqual(included["skipped_deferred"], [])

    def test_touched_deferred_only_package_is_built_when_its_test_is_skipped(self):
        self.git("checkout", "--", "crates/example/src/lib.rs")
        (self.root / "crates/slow/src/lib.rs").write_text("pub fn slow() { let _ = 1; }\n")
        query = self.bin / "buck2"
        query.write_text("#!/bin/sh\necho root//crates/slow:slow__test\n")
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"], [[
            "kiln", "build", "//crates/slow:service__test",
            "//crates/slow:slow__test", "//crates/slow:trunk__test", "//:schema_checks",
        ]])
        self.assertEqual(planned["skipped_deferred"], ["//crates/slow:slow__test"])
        included = json.loads(self.invoke("--dependents", "--include-deferred", "--dry-run").stdout)
        self.assertEqual(included["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//crates/slow:slow__test"],
        ])

    def report(self, statuses):
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        report = Path(outside.name) / "test-report.json"
        report.write_text(json.dumps({"schema": 1, "results": {
            "root" + label: {"status": status, "outputs": {}, "cache": cache}
            for label, (status, cache) in statuses.items()
        }}))
        self.env["TEST_REPORT_FILE"] = str(report)

    def test_summary_counts_the_targets_of_a_passing_gate(self):
        self.report({
            "//crates/example:first": ("PASS", True),
            "//crates/example:second": ("PASS", False),
        })
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("dev-test: PASS: 2 test targets (1 from cache), 2 commands", result.stdout)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertIn("dev-test: PASS: 2 test targets", receipt["summary"][0])

    def test_summary_names_every_fail_and_timeout(self):
        statuses = {f"//crates/example:t{index:02}": ("FAIL", False) for index in range(20)}
        statuses["//crates/example:hung"] = ("TIMEOUT", False)
        statuses["//crates/example:lost"] = ("INFRA_FAILURE", False)
        statuses["//crates/example:fine"] = ("PASS", True)
        self.report(statuses)
        self.env["TEST_EXIT"] = "32"
        result = self.invoke()
        self.assertEqual(result.returncode, 32)
        self.assertIn("dev-test: FAIL: 22 of 23 test targets did not pass", result.stdout)
        for index in range(20):
            self.assertIn(f"  FAIL //crates/example:t{index:02}\n", result.stdout)
        self.assertIn("  TIMEOUT //crates/example:hung\n", result.stdout)
        self.assertIn("  INFRA_FAILURE //crates/example:lost\n", result.stdout)
        self.assertNotIn("dev-test: PASS", result.stdout)

    def test_a_failing_report_is_never_a_green_gate(self):
        self.report({"//crates/example:first": ("TIMEOUT", False)})
        result = self.invoke()
        self.assertEqual(result.returncode, 1)
        self.assertIn("  TIMEOUT //crates/example:first", result.stdout)

    def test_a_failure_without_a_failing_target_is_an_error_not_a_stale_verdict(self):
        self.report({"//crates/example:first": ("FAIL", False)})
        self.env["TEST_EXIT"] = "32"
        self.assertEqual(self.invoke().returncode, 32)
        # The next run dies before it reports: the earlier report must not
        # be read as this run's verdict.
        executor = self.bin / "kiln"
        executor.write_text("#!/bin/sh\n[ \"$1\" = test ] && exit 3\nexit 0\n")
        result = self.invoke()
        self.assertEqual(result.returncode, 3)
        self.assertIn("dev-test: ERROR: `kiln test", result.stdout)
        self.assertIn("exit 3 without a failing test target", result.stdout)
        self.assertNotIn("dev-test: FAIL", result.stdout)
        self.assertNotIn("dev-test: PASS", result.stdout)

    def test_a_failed_command_does_not_hide_the_later_verdicts(self):
        self.report({"//crates/example:first": ("FAIL", False)})
        executor = self.bin / "kiln"
        executor.write_text(
            "#!/bin/sh\n[ \"$1\" = build ] && exit 4\n"
            "cp \"$TEST_REPORT_FILE\" \"$3\"\nexit 32\n")
        result = self.invoke()
        # The first failure is the exit code; the test verdict is still named.
        self.assertEqual(result.returncode, 4)
        self.assertIn("dev-test: ERROR: `kiln build //:schema_checks` exit 4", result.stdout)
        self.assertIn("dev-test: FAIL: 1 of 1 test targets did not pass", result.stdout)
        self.assertIn("  FAIL //crates/example:first", result.stdout)
        self.assertNotIn("NOT RUN", result.stdout)

    def test_waiting_callers_recheck_buck2_inputs_instead_of_reusing_receipts(self):
        self.env["TEST_EXIT"] = "7"
        hold = self.root / ".git/hold"
        hold.touch()
        first = self.start()
        self.wait_started()
        second = self.start()
        self.assertIn("waiting", second.stdout.readline())
        hold.unlink()
        first.communicate(timeout=10)
        second.communicate(timeout=10)
        self.assertEqual((first.returncode, second.returncode), (7, 7))
        self.assertEqual(
            (self.root / ".git/calls").read_text().splitlines(),
            ["run", "run", "run", "run"],
        )
        self.assertEqual(self.invoke().returncode, 7)
        self.assertEqual(len((self.root / ".git/calls").read_text().splitlines()), 6)

    def test_edit_during_validation_cannot_produce_green_receipt(self):
        hold = self.root / ".git/hold"
        hold.touch()
        process = self.start()
        self.wait_started()
        self.source.write_text("changed during validation\n")
        hold.unlink()
        process.communicate(timeout=10)
        self.assertEqual(process.returncode, 2)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertFalse(receipt["inputs_unchanged"])

    def test_driver_managed_concurrency_change_does_not_stale_result(self):
        hold = self.root / ".git/hold"
        hold.touch()
        process = self.start()
        self.wait_started()
        config = self.root / ".buckconfig.local"
        config.write_text(
            config.read_text()
            + "execution_concurrency_limit = 16\n"
        )
        hold.unlink()
        _stdout, stderr = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, stderr)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertTrue(receipt["inputs_unchanged"])

    def test_snapshot_keeps_executor_identity_and_credentials(self):
        module = dev_test_module()
        path = Path(".buckconfig.local")
        original = (
            b"[buck2_re_client]\n"
            b"engine_address = grpcs://one\n"
            b"tls_client_cert = .kiln/one.pem\n"
        )
        with_limit = original + b"execution_concurrency_limit = 32\n"
        self.assertEqual(
            module.snapshot_contents(path, original),
            module.snapshot_contents(path, with_limit),
        )
        self.assertNotEqual(
            module.snapshot_contents(path, original),
            module.snapshot_contents(
                path,
                original.replace(b"grpcs://one", b"grpcs://two"),
            ),
        )
        self.assertNotEqual(
            module.snapshot_contents(path, original),
            module.snapshot_contents(
                path,
                original.replace(b".kiln/one.pem", b".kiln/two.pem"),
            ),
        )


    def test_ignored_input_change_still_requires_each_waiter_to_invoke_buck2(self):
        ignore = self.root / ".gitignore"
        ignore.write_text(ignore.read_text() + "generated-input\n")
        ignored = self.root / "generated-input"
        ignored.write_text("first\n")
        hold = self.root / ".git/hold"
        hold.touch()
        first = self.start()
        self.wait_started()
        second = self.start()
        self.assertIn("waiting", second.stdout.readline())
        ignored.write_text("second\n")
        hold.unlink()
        first.communicate(timeout=10)
        second.communicate(timeout=10)
        self.assertEqual((first.returncode, second.returncode), (0, 0))
        self.assertEqual(
            (self.root / ".git/calls").read_text().splitlines(),
            ["run", "run", "run", "run"],
        )
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertIn("checkout/config snapshot", receipt["plan"]["identity_scope"])


    def test_interrupt_stops_the_owned_executor_and_writes_failure(self):
        hold = self.root / ".git/hold"
        hold.touch()
        process = self.start()
        self.wait_started()
        child_pid = int((self.root / ".git/pid").read_text())
        process.terminate()
        process.communicate(timeout=15)
        self.assertEqual(process.returncode, 130)
        with self.assertRaises(ProcessLookupError):
            os.kill(child_pid, 0)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertEqual(receipt["exit_code"], 130)

    def fake_buck2(self, owners=None, dependents=()):
        """A Buck2 client answering `uquery owner(...)` and `cquery rdeps(...)`."""
        query = self.bin / "buck2"
        query.write_text(
            "#!/usr/bin/env python3\nimport json, sys\nfrom pathlib import Path\n"
            "with Path('.git/query-args').open('a') as log:\n"
            " log.write(' '.join(sys.argv[1:]) + '\\n')\n"
            "if 'uquery' in sys.argv:\n"
            f" owners = {owners!r}\n"
            " if owners is None: raise SystemExit(1)\n"
            " print(json.dumps(owners))\n"
            "else:\n"
            f" print('\\n'.join(label + ' (cfg:linux#0123)' for label in {list(dependents)!r}))\n")
        query.chmod(0o755)

    def test_dependents_read_a_package_manifest_as_its_package(self):
        (self.source.parents[1] / "Cargo.toml").write_text("[package]\n")
        self.fake_buck2(dependents=[
            "root//crates/example:first", "root//crates/example:second",
            "root//crates/dependent:dependent__test",
        ])
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["selection"], "dependents")
        self.assertEqual(planned["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//crates/dependent:dependent__test", "//crates/example:test_batch"],
        ])
        self.assertIn("set(//crates/example:)", (self.root / ".git/query-args").read_text())
        # Package iteration asks Buck2 nothing, so the manifest still widens.
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"][-1],
                         ["kiln", "test", "//:dev_tests"])

    def test_dependents_select_the_declared_readers_of_a_schema_file(self):
        schema = self.root / "schemas/host/demo/v1.schema.json"
        schema.parent.mkdir(parents=True)
        schema.write_text("{}\n")
        self.git("checkout", "--", "crates/example/src/lib.rs")
        self.git("add", ".")
        self.git("commit", "-qm", "schema")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        schema.write_text('{"title": "demo"}\n')
        relative = "schemas/host/demo/v1.schema.json"
        self.fake_buck2(owners={relative: ["root//:host_schemas"]},
                        dependents=["root//crates/dependent:dependent__test"])
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"], [
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//crates/dependent:dependent__test"],
        ])
        arguments = (self.root / ".git/query-args").read_text()
        self.assertIn(f"uquery --json owner('%s') {relative}", arguments)
        self.assertIn("set(//:host_schemas)", arguments)
        # No reader at all still checks the schemas and runs no test.
        self.fake_buck2(owners={relative: ["root//:host_schemas"]})
        self.assertEqual(json.loads(self.invoke("--dependents", "--dry-run").stdout)["commands"],
                         [["kiln", "build", "//:schema_checks"]])
        # A file no target declares cannot be selected exactly.
        self.fake_buck2(owners={relative: []})
        result = self.invoke("--dependents", "--dry-run")
        self.assertEqual(json.loads(result.stdout)["commands"][-1], ["kiln", "test", "//:dev_tests"])
        self.assertIn(f"no Buck2 target declares {relative}", result.stderr)
        self.fake_buck2(owners=None)
        self.assertEqual(json.loads(self.invoke("--dependents", "--dry-run").stdout)["selection"], "suite")

    def test_dependents_run_no_suite_for_a_script_no_target_declares(self):
        self.git("checkout", "--", "crates/example/src/lib.rs")
        script = self.root / "scripts/ci/helper.sh"
        script.write_text("true\n")
        workflow = self.root / ".github/workflows/ci.yml"
        workflow.write_text(workflow.read_text() + "jobs:\n  rust:\n    if: needs.plan.outputs.rust == 'true'\n"
                            "    steps:\n      - run: bash scripts/ci/helper.sh\n")
        (self.root / "justfile").write_text("")
        self.git("add", ".")
        self.git("commit", "-qm", "helper")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        script.write_text("true # edited\n")
        self.fake_buck2(owners={"scripts/ci/helper.sh": []})
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"], [
            ["bash", "scripts/ci/repository-gates.sh"],
            ["kiln", "build", "//:schema_checks"],
        ])
        # Declared as a test input, it selects that input's dependents.
        self.fake_buck2(owners={"scripts/ci/helper.sh": ["root//:workspace_test_scripts"]},
                        dependents=["root//crates/dependent:dependent__test"])
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"][-1], ["kiln", "test", "//crates/dependent:dependent__test"])
        # Package iteration keeps the whole suite for a script a Rust job runs.
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"][-1],
                         ["kiln", "test", "//:dev_tests"])

    def test_unaffected_repository_gates_are_skipped(self):
        gate = "python3 scripts/test_check_version_bumps.py"
        workflow = self.root / ".github/workflows/ci.yml"
        workflow.write_text("bash scripts/ci/run-gate-commands.sh --jobs 4 <<'GATES'\n"
                            f"python3 scripts/test_dev_test.py\n{gate}\nGATES\n")
        self.git("checkout", "--", "crates/example/src/lib.rs")
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"][0],
                         ["bash", "scripts/ci/repository-gates.sh", "--skip", gate])
        # A package source is an input of the version-bump gate.
        self.source.write_text("pub fn example() { let _ = 2; }\n")
        planned = json.loads(self.invoke("--dependents", "--dry-run").stdout)
        self.assertEqual(planned["commands"][0], ["bash", "scripts/ci/repository-gates.sh"])

    def both_halves(self, script):
        """A diff that plans one script proof and the Buck2 commands."""
        (self.root / "scripts/test_dev_test.py").write_text(script)
        planned = json.loads(self.invoke("--dry-run").stdout)
        self.assertEqual(planned["commands"], [
            ["python3", "scripts/test_dev_test.py"],
            ["kiln", "build", "//:schema_checks"],
            ["kiln", "test", "//crates/example:test_batch"],
        ])

    def test_repository_and_buck2_halves_run_side_by_side(self):
        # The script proof waits for the executor to start and then releases
        # it: run one after the other, the proof would time out.
        self.both_halves(
            "import time\nfrom pathlib import Path\n"
            "deadline = time.monotonic() + 10\n"
            "while not Path('.git/started').exists():\n"
            "    if time.monotonic() > deadline: raise SystemExit(9)\n"
            "    time.sleep(0.01)\n"
            "Path('.git/hold').unlink()\n")
        (self.root / ".git/hold").touch()
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("dev-test: PASS: 0 test targets (0 from cache), 3 commands", result.stdout)
        # Each command keeps its own log.
        logs = sorted(path.name for path in (self.root / ".git/lash-validation").glob("command-*.log"))
        self.assertEqual(logs, ["command-0.log", "command-1.log", "command-2.log"])

    def test_a_failed_repository_gate_still_reports_the_buck2_verdicts(self):
        self.both_halves("print('script proof output')\nraise SystemExit(7)\n")
        self.report({"//crates/example:first": ("FAIL", False)})
        self.env["TEST_EXIT"] = "32"
        result = self.invoke()
        # One verdict over both halves; the first failure in plan order exits.
        self.assertEqual(result.returncode, 7)
        self.assertIn("dev-test: `python3 scripts/test_dev_test.py` failed with exit 7", result.stdout)
        self.assertIn("script proof output", result.stdout)
        self.assertIn("dev-test: ERROR: `python3 scripts/test_dev_test.py` exit 7", result.stdout)
        self.assertIn("dev-test: FAIL: 1 of 1 test targets did not pass", result.stdout)
        self.assertNotIn("NOT RUN", result.stdout)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertEqual(receipt["exit_code"], 7)
        self.assertEqual(receipt["plan"]["commands"][0], ["python3", "scripts/test_dev_test.py"])
        # Streamed, the half that does not own the terminal prints as one block.
        verbose = self.invoke("--verbose")
        self.assertEqual(verbose.returncode, 7)
        self.assertIn("dev-test: output of `python3 scripts/test_dev_test.py` (exit 7):\n"
                      "script proof output\n", verbose.stdout)

    def test_interrupt_stops_both_halves(self):
        self.both_halves(
            "import os, time\nfrom pathlib import Path\n"
            "Path('.git/script-pid').write_text(str(os.getpid()))\ntime.sleep(60)\n")
        (self.root / ".git/hold").touch()
        process = self.start()
        self.wait_started()
        deadline = time.monotonic() + 10
        while not (self.root / ".git/script-pid").exists() or not (self.root / ".git/script-pid").read_text():
            self.assertLess(time.monotonic(), deadline, "script proof never started")
            time.sleep(0.01)
        pids = [int((self.root / name).read_text()) for name in (".git/pid", ".git/script-pid")]
        process.terminate()
        output, _ = process.communicate(timeout=15)
        self.assertEqual(process.returncode, 130)
        for pid in pids:
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
        self.assertIn("dev-test: NOT RUN: python3 scripts/test_dev_test.py; kiln build //:schema_checks; "
                      "kiln test //crates/example:test_batch", output)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertEqual(receipt["exit_code"], 130)

    def test_live_store_environment_is_refused(self):
        self.env["LASH_POSTGRES_DATABASE_URL"] = "postgres://fixture"
        self.assertEqual(self.invoke().returncode, 2)
        self.assertFalse((self.root / ".git/calls").exists())

    def canned_failure(self):
        """A fake `kiln test` failure: one target fails with a Rust panic."""
        # The canned artifacts live outside the fixture root: an untracked
        # file inside it would widen the plan's selection.
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        artifacts = Path(outside.name) / "testlogs/crates/example/first"
        artifacts.mkdir(parents=True)
        (artifacts / "test.log").write_text(
            "exec ${PAGER:-less} \"$0\" || exit 1\n"
            "Executed tests from //crates/example:first\n"
            "running 3 tests\n"
            "test alpha::passes ... ok\n"
            "test alpha::fails_hard ... FAILED\n"
            "test alpha::passes_too ... ok\n\n"
            "failures:\n\n"
            "---- alpha::fails_hard stdout ----\n"
            "thread 'alpha::fails_hard' panicked at crates/example/src/lib.rs:42:7:\n"
            "assertion `left == right` failed\n"
            "  left: 1\n"
            " right: 2\n"
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n"
            "failures:\n"
            "    alpha::fails_hard\n\n"
            "test result: FAILED. 2 passed; 1 failed; 0 ignored\n"
        )
        (artifacts / "test.xml").write_text(
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            '<testsuite name="//crates/example:first" tests="3" failures="1">\n'
            '  <testcase name="alpha::passes" classname="first" time="0.01"/>\n'
            '  <testcase name="alpha::fails_hard" classname="first" time="0.02">\n'
            '    <failure message="panicked">thread \'alpha::fails_hard\' panicked at '
            'crates/example/src/lib.rs:42:7:\nassertion `left == right` failed</failure>\n'
            '  </testcase>\n'
            '  <testcase name="alpha::passes_too" classname="first" time="0.01"/>\n'
            '</testsuite>\n'
        )
        canned = Path(outside.name) / "canned.txt"
        canned.write_text(
            "INFO: Invocation ID: fixture\n"
            "BUCK2-NOISE-MARKER-LINE\n"
            "==================== Test output for //crates/example:first:\n"
            + "noise padding lines that must never reach stdout\n" * 8
            + "-----------------------------------------------------------------------------\n"
            f"//crates/example:first                     FAILED in 1.2s\n"
            f"  {artifacts}/test.log\n\n"
            f"//crates/example:second                    PASSED in 0.3s\n"
            f"  {artifacts}/second/test.log\n\n"
            "Executed 2 out of 2 tests: 1 test fails.\n"
        )
        report = Path(outside.name) / "test-report.json"
        report.write_text(json.dumps({
            "schema": 1,
            "results": {
                "root//crates/example:first": {
                    "status": "FAIL",
                    "exit_code": 101,
                    "outputs": {
                        "junit_xml": str(artifacts / "test.xml"),
                        "log": str(artifacts / "test.log"),
                        "undeclared": None,
                    },
                },
                "root//crates/example:second": {
                    "status": "PASS",
                    "exit_code": 0,
                    "outputs": {},
                },
            },
        }))
        self.env["TEST_STDOUT_FILE"] = str(canned)
        self.env["TEST_REPORT_FILE"] = str(report)
        self.env["TEST_EXIT"] = "1"

    def test_failure_summary_names_the_target_test_panic_and_log(self):
        self.canned_failure()
        result = self.invoke()
        self.assertEqual(result.returncode, 1)
        out = result.stdout
        self.assertIn("//crates/example:first", out)
        self.assertIn("alpha::fails_hard", out)
        self.assertIn("crates/example/src/lib.rs:42:7", out)
        self.assertIn("test.log", out)
        self.assertIn("--verbose", out)
        # The point of the summary: the firehose stays out of the output.
        self.assertNotIn("BUCK2-NOISE-MARKER-LINE", out)
        self.assertNotIn("noise padding", out)
        self.assertNotIn("//crates/example:second", out)
        self.assertLess(len(out.splitlines()), 60)

    def test_verbose_streams_the_full_output(self):
        self.canned_failure()
        result = self.invoke("--verbose")
        self.assertEqual(result.returncode, 1)
        self.assertIn("BUCK2-NOISE-MARKER-LINE", result.stdout)
        self.assertNotIn("failing targets:", result.stdout)

    def test_quick_mode_forwards_test_env_and_shard_includes(self):
        package = self.root / "crates/lash-typescript"
        package.mkdir()
        (package / "BUCK").write_text("# fixture\n")
        outcomes = self.root / "crates/lash-typescript/tests/test262/outcomes"
        outcomes.mkdir(parents=True)
        (outcomes / "built-ins.tsv").write_text("# rows\n")
        vendored = self.root / "crates/lash-typescript/tests/test262/test/language/statements"
        vendored.mkdir(parents=True)
        (vendored / "for.js").write_text("await control.finish(true);\n")
        self.env["LASH_QUICK"] = "1"
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        test_commands = [c for c in commands if c[:2] == ["kiln", "test"]]
        self.assertTrue(test_commands)
        flags = [arg for c in test_commands for arg in c if arg.startswith("--test_env=")]
        self.assertIn("--test_env=LASH_QUICK=1", flags)
        self.assertIn(
            "--test_env=LASH_TEST262_QUICK_INCLUDE=built-ins,language", flags)
        # A selection-wide input keeps every shard whole.
        (self.root / "crates/lash-typescript/tests/test262/census.tsv").write_text("# census\n")
        flags = [
            arg
            for c in json.loads(self.invoke("--dry-run").stdout)["commands"]
            for arg in c if arg.startswith("--test_env=")
        ]
        self.assertIn(
            "--test_env=LASH_TEST262_QUICK_INCLUDE=*", flags)
        # Without the knob the commands carry no quick flags at all.
        del self.env["LASH_QUICK"]
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        self.assertFalse(
            any(arg.startswith("--test_env=") for c in commands for arg in c))


class RepositoryGatesRunnerTests(unittest.TestCase):
    """`repository-gates.sh --skip` leaves out exactly the named gate."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "scripts/ci").mkdir(parents=True)
        for name in ("repository-gates.sh", "run-gate-commands.sh", "repository_gate_commands.py"):
            shutil.copyfile(SOURCE.parent / "ci" / name, self.root / "scripts/ci" / name)
        workflow = self.root / ".github/workflows/ci.yml"
        workflow.parent.mkdir(parents=True)
        workflow.write_text("        run: |\n          bash scripts/ci/run-gate-commands.sh --jobs 4 <<'GATES'\n"
                            "          echo kept gate\n          exit 3\n          GATES\n")

    def gates(self, *args):
        return subprocess.run(["bash", str(self.root / "scripts/ci/repository-gates.sh"), *args],
                              cwd=self.root, capture_output=True, text=True, timeout=60)

    def test_a_skipped_gate_does_not_run_and_is_named(self):
        self.assertEqual(self.gates().returncode, 1)
        result = self.gates("--skip", "exit 3")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 gate commands passed", result.stdout)
        self.assertIn("skipped as unaffected by this change (CI still runs them):\n- exit 3\n",
                      result.stdout)

    def test_a_skip_that_names_no_gate_is_refused(self):
        result = self.gates("--skip", "exit 4")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--skip names no gate command: exit 4", result.stderr)
        self.assertEqual(self.gates("--skip").returncode, 2)
        self.assertEqual(self.gates("--unknown").returncode, 2)


class FormatterTests(unittest.TestCase):
    """The pure summary helpers of `dev-test.py`, exercised without a run."""

    module = None

    @classmethod
    def setUpClass(cls):
        cls.module = dev_test_module()

    def test_quick_includes_map_changed_paths_to_shards(self):
        includes = self.module.quick_test262_includes
        self.assertEqual(
            includes(["crates/lash-typescript/tests/test262/test/language/foo/a.js"]),
            {"language"})
        self.assertEqual(
            includes(["crates/lash-typescript/tests/test262/outcomes/built-ins.tsv"]),
            {"built-ins"})
        self.assertEqual(
            includes(["crates/lash-typescript/tests/test262/census.tsv",
                      "crates/lash-typescript/tests/test262/test/language/a.js",
                      "crates/other/src/lib.rs"]),
            {"*"})

    def test_failed_targets_reads_the_test_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = Path(tmp) / "report.json"
            report.write_text(json.dumps({"results": {
                "root//pkg:a": {"status": "FAIL", "outputs": {
                    "log": "/results/pkg/a/test.log",
                    "junit_xml": "/results/pkg/a/test.xml",
                }},
                "root//pkg:b": {"status": "PASS", "outputs": {}},
            }}))
            self.assertEqual(self.module.failed_targets(report), [
                ("//pkg:a", ["/results/pkg/a/test.xml", "/results/pkg/a/test.log"]),
            ])

    def test_panic_line_prefers_the_first_location(self):
        text = (
            "thread 'x' panicked at src/a.rs:7:9:\n"
            "assertion failed: left == right\n"
            "thread 'y' panicked at src/b.rs:9:1:\n"
            "other\n"
        )
        self.assertEqual(self.module.panic_line(text),
                         "src/a.rs:7:9: assertion failed: left == right")

    def test_failure_summary_reads_xml_and_caps_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            artifacts = Path(tmp) / "testlogs/pkg/a"
            artifacts.mkdir(parents=True)
            (artifacts / "test.log").write_text("running 1 test\n")
            (artifacts / "test.xml").write_text(
                '<testsuite><testcase name="a::boom">'
                '<failure>thread \'a::boom\' panicked at src/x.rs:3:1:\nboom</failure>'
                "</testcase></testsuite>")
            log = Path(tmp) / "command-0.log"
            log.write_text(
                "//pkg:a                       FAILED in 0.1s\n"
                f"  {artifacts}/test.log\n"
            )
            report = Path(tmp) / "report.json"
            report.write_text(json.dumps({"results": {
                "root//pkg:a": {"status": "FAIL", "outputs": {
                    "log": str(artifacts / "test.log"),
                    "junit_xml": str(artifacts / "test.xml"),
                }}
            }}))
            summary = self.module.failure_summary(
                ["kiln", "test", "//pkg:a"], 1, log, report)
            self.assertIn("//pkg:a", summary)
            self.assertIn("a::boom", summary)
            self.assertIn("src/x.rs:3:1: boom", summary)
            self.assertIn(str(artifacts / "test.log"), summary)
            self.assertLessEqual(len(summary.splitlines()), self.module.SUMMARY_LINES)

    def test_failure_summary_falls_back_to_output_tail(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "command-0.log"
            log.write_text("line of noise\n" * 100 + "real error at the end\n")
            summary = self.module.failure_summary(["tool"], 2, log, Path(tmp) / "missing.json")
            self.assertIn("real error at the end", summary)
            self.assertNotIn("line of noise\n" * 10, summary)
            self.assertLessEqual(len(summary.splitlines()), self.module.SUMMARY_LINES)


if __name__ == "__main__":
    unittest.main()
