#!/usr/bin/env python3
"""Contract tests for the generated Bazel test-cache surface."""

from __future__ import annotations

import ast
import collections
import json
import os
import pathlib
import re
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
TEST_KINDS = {"bin-unit-test", "test", "unit-test"}


def generated_list(name: str) -> list[str]:
    source = (ROOT / "tools/bazel/workspace_targets.bzl").read_text(encoding="utf-8")
    match = re.search(rf"^{name} = (\[.*?^\])$", source, flags=re.MULTILINE | re.DOTALL)
    if match is None:
        raise AssertionError(f"missing generated list {name}")
    return ast.literal_eval(match.group(1))


def test_targets() -> list[dict[str, object]]:
    inventory = json.loads(
        (ROOT / "tools/bazel/target-inventory.json").read_text(encoding="utf-8")
    )
    return [
        target
        for package in inventory["packages"]
        for target in package["targets"]
        if target["label"] is not None and target["kind"] in TEST_KINDS
    ]


class BazelTestContractTests(unittest.TestCase):
    def test_generated_inventory_is_current(self) -> None:
        subprocess.run(
            ["python3", "tools/bazel/generate_build_files.py", "--check"],
            cwd=ROOT,
            check=True,
        )

    def test_generated_suite_partitions_every_executable_test(self) -> None:
        targets = test_targets()
        all_labels = {target["label"] for target in targets}
        bazel_labels = set(generated_list("WORKSPACE_BAZEL_TEST_TARGETS"))
        cargo_labels = set(generated_list("WORKSPACE_CARGO_TEST_TARGETS"))

        self.assertEqual(109, len(all_labels))
        self.assertEqual(87, len(bazel_labels))
        self.assertEqual(22, len(cargo_labels))
        self.assertFalse(bazel_labels & cargo_labels)
        self.assertEqual(all_labels, bazel_labels | cargo_labels)
        self.assertEqual(all_labels, set(generated_list("WORKSPACE_TEST_TARGETS")))

        by_label = {target["label"]: target for target in targets}
        self.assertTrue(
            all("manual" not in by_label[label]["tags"] for label in bazel_labels)
        )
        self.assertTrue(
            all(
                "manual" in by_label[label]["tags"]
                and by_label[label]["cargo_only"]
                for label in cargo_labels
            )
        )

        exception_classes = collections.Counter(
            tag
            for label in cargo_labels
            for tag in by_label[label]["tags"]
            if tag != "manual"
        )
        self.assertEqual(
            {
                "cargo-frontend-assets": 4,
                "cargo-heavy-suite": 1,
                "cargo-nested-suite": 2,
                "cargo-path-assets": 1,
                "cargo-service-gate": 13,
                "cargo-trybuild": 1,
            },
            dict(exception_classes),
        )

    def test_workspace_suite_and_cli_default_to_the_generated_partition(self) -> None:
        root_build = (ROOT / "BUILD.bazel").read_text(encoding="utf-8")
        self.assertIn('name = "workspace_tests"', root_build)
        self.assertIn("tests = WORKSPACE_BAZEL_TEST_TARGETS", root_build)

        with tempfile.TemporaryDirectory() as temporary:
            args_log = pathlib.Path(temporary) / "args"
            fake_bazel = pathlib.Path(temporary) / "bazel"
            fake_bazel.write_text(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$BAZEL_ARGS_LOG\"\n",
                encoding="utf-8",
            )
            fake_bazel.chmod(0o755)
            environment = os.environ | {
                "BAZEL": str(fake_bazel),
                "BAZEL_ARGS_LOG": str(args_log),
            }

            subprocess.run(
                ["bash", "scripts/hermetic-build.sh", "--local", "test"],
                cwd=ROOT,
                env=environment,
                check=True,
            )
            self.assertEqual(
                ["test", "--config=local", "//:workspace_tests"],
                args_log.read_text(encoding="utf-8").splitlines(),
            )

            subprocess.run(
                [
                    "bash",
                    "scripts/hermetic-build.sh",
                    "--local",
                    "test",
                    "//crates/lash-sansio:lash-sansio__unit_test",
                ],
                cwd=ROOT,
                env=environment,
                check=True,
            )
            self.assertEqual(
                [
                    "test",
                    "--config=local",
                    "//crates/lash-sansio:lash-sansio__unit_test",
                ],
                args_log.read_text(encoding="utf-8").splitlines(),
            )

    def test_trusted_ci_runs_the_suite_with_explicit_test_cache_semantics(self) -> None:
        workflow = (ROOT / ".github/workflows/bazel.yml").read_text(encoding="utf-8")
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        self.assertIn("test --cache_test_results=yes", bazelrc)
        self.assertIn("github.actor != 'dependabot[bot]'", workflow)
        self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", workflow)
        self.assertIn("environment: build-cache", workflow)
        self.assertIn("--remote_cache=grpcs://51.15.75.60:8443", workflow)
        self.assertIn("--cache_test_results=yes --test_output=errors", workflow)
        self.assertIn("//:workspace_tests", workflow)
        self.assertNotIn("//:workspace_compile", workflow)

        for path_filter in (
            "'crates/**'",
            "'examples/**'",
            "'runbooks/**'",
            "'fixtures/**'",
            "tools/bazel/**",
            "scripts/hermetic-build.sh",
            "scripts/perf_guard_budgets.json",
            "scripts/slack-clone-live-model-ui.py",
        ):
            self.assertIn(f"- {path_filter}", workflow)


if __name__ == "__main__":
    unittest.main()
