#!/usr/bin/env python3
"""Contract tests for the generated Bazel test-cache surface."""

from __future__ import annotations

import ast
import collections
import json
import os
import pathlib
import re
import shlex
import subprocess
import sys
import tempfile
import unittest

import yaml


sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import ci_plan  # noqa: E402


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
        target | {"package": package["package"]}
        for package in inventory["packages"]
        for target in package["targets"]
        if target["label"] is not None and target["kind"] in TEST_KINDS
    ]


def workflow() -> dict[str, object]:
    return yaml.load(
        (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8"),
        Loader=yaml.BaseLoader,
    )


def job_step(job: dict[str, object], name: str) -> dict[str, str]:
    return next(step for step in job["steps"] if step.get("name") == name)


def generated_nextest_terms() -> set[tuple[str, str, str | None]]:
    source = (ROOT / "tools/bazel/cargo_owned_nextest_filter.txt").read_text(
        encoding="utf-8"
    )
    term_pattern = re.compile(
        r"\(package\(([^)]+)\) & kind\(([^)]+)\)"
        r"(?: & binary\(([^)]+)\))?\)"
    )
    matches = term_pattern.findall(source.strip())
    self_check = " + ".join(
        f"(package({package}) & kind({kind})"
        + (f" & binary({binary})" if binary else "")
        + ")"
        for package, kind, binary in matches
    )
    if self_check != source.strip():
        raise AssertionError("generated nextest filter contains an unparsed term")
    return {(package, kind, binary or None) for package, kind, binary in matches}


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

        expected_nextest = set()
        for target in targets:
            if target["label"] not in cargo_labels:
                continue
            kind = target["kind"]
            expected_nextest.add(
                (
                    target["package"],
                    {
                        "unit-test": "lib",
                        "bin-unit-test": "bin",
                        "test": "test",
                    }[kind],
                    None if kind == "unit-test" else target["cargo"],
                )
            )
        self.assertEqual(22, len(expected_nextest))
        self.assertEqual(expected_nextest, generated_nextest_terms())

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

    def test_ci_has_one_authoritative_bazel_job_and_no_standalone_workflow(self) -> None:
        parsed = workflow()
        triggers = parsed["on"]
        jobs = parsed["jobs"]
        self.assertEqual(
            {"pull_request", "push", "workflow_dispatch", "merge_group"},
            set(triggers),
        )
        self.assertFalse((ROOT / ".github/workflows/bazel.yml").exists())

        trust_expression = jobs["plan"]["outputs"]["bazel_trusted"]
        self.assertEqual(
            "${{ github.event_name == 'merge_group' "
            "|| github.event_name == 'push' "
            "|| github.event_name == 'workflow_dispatch' "
            "|| (github.event_name == 'pull_request' "
            "&& github.actor != 'dependabot[bot]' "
            "&& github.event.pull_request.head.repo.full_name == github.repository) }}",
            trust_expression,
        )

        bazel_steps = [
            (job_id, step)
            for job_id, job in jobs.items()
            for step in job.get("steps", [])
            if "//:workspace_tests" in step.get("run", "")
        ]
        self.assertEqual(1, len(bazel_steps))
        self.assertEqual("bazel-tests", bazel_steps[0][0])
        bazel_job = jobs["bazel-tests"]
        self.assertEqual("build-cache", bazel_job["environment"])
        self.assertEqual("needs.plan.outputs.bazel_trusted == 'true'", bazel_job["if"])
        self.assertIn("bazel-tests", jobs["ci-conclusion"]["needs"])
        self.assertEqual(
            "${{ needs.plan.outputs.bazel_trusted }}",
            job_step(jobs["ci-conclusion"], "Validate CI conclusion")["env"][
                "BAZEL_TRUSTED"
            ],
        )

    def test_ci_enrolls_the_contract_and_uses_a_distinct_runner_identity(self) -> None:
        jobs = workflow()["jobs"]
        repository_tests = job_step(jobs["repo-gates"], "Test repository scripts")[
            "run"
        ].splitlines()
        self.assertEqual(
            1,
            repository_tests.count("python3 scripts/test_bazel_test_contract.py"),
        )

        runtime = job_step(jobs["bazel-tests"], "Resolve GitHub runner cache identity")
        self.assertIn("scripts/ci_plan.py bazel-runtime", runtime["run"])
        with tempfile.TemporaryDirectory() as temporary:
            github_output = pathlib.Path(temporary) / "output"
            environment = os.environ | {
                "GITHUB_OUTPUT": str(github_output),
                "ImageOS": "ubuntu24",
                "ImageVersion": "20260907.1",
                "RUNNER_ARCH": "X64",
                "RUNNER_OS": "Linux",
            }
            subprocess.run(
                ["bash", "-euo", "pipefail", "-c", runtime["run"]],
                cwd=ROOT,
                env=environment,
                check=True,
            )
            self.assertEqual(
                "bazel_runtime="
                + ci_plan.github_runner_cache_identity(
                    "Linux", "X64", "ubuntu24", "20260907.1"
                ),
                github_output.read_text(encoding="utf-8").strip(),
            )
        bazel_command = job_step(
            jobs["bazel-tests"], "Test deterministic workspace suite with shared cache"
        )["run"]
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        self.assertIn("test --cache_test_results=yes", bazelrc)
        self.assertIn("--remote_cache=grpcs://51.15.75.60:8443", bazel_command)
        self.assertIn("--cache_test_results=yes --test_output=errors", bazel_command)
        self.assertIn(
            "--remote_default_exec_properties=github_runner_runtime=${{ "
            "steps.bazel-runtime.outputs.bazel_runtime }}",
            bazel_command,
        )
        self.assertNotIn("orb_executor_runtime", bazel_command)

    def test_workspace_nextest_step_filters_only_trusted_events(self) -> None:
        jobs = workflow()["jobs"]
        workspace_step = job_step(jobs["workspace-tests"], "Test workspace")
        self.assertEqual(
            "${{ needs.plan.outputs.bazel_trusted }}",
            workspace_step["env"]["BAZEL_TRUSTED"],
        )
        script = workspace_step["run"]
        expected_filter = (
            ROOT / "tools/bazel/cargo_owned_nextest_filter.txt"
        ).read_text(encoding="utf-8").strip()

        for trusted in (True, False):
            with self.subTest(trusted=trusted), tempfile.TemporaryDirectory() as temporary:
                temporary_path = pathlib.Path(temporary)
                args_log = temporary_path / "cargo-args"
                fake_cargo = temporary_path / "cargo"
                fake_cargo.write_text(
                    "#!/usr/bin/env bash\nprintf '%q ' \"$@\" >> \"$CARGO_ARGS_LOG\"\nprintf '\\n' >> \"$CARGO_ARGS_LOG\"\n",
                    encoding="utf-8",
                )
                fake_cargo.chmod(0o755)
                environment = os.environ | {
                    "BAZEL_TRUSTED": str(trusted).lower(),
                    "CARGO_ARGS_LOG": str(args_log),
                    "LASH_CI_FEATURES": "",
                    "PATH": f"{temporary}{os.pathsep}{os.environ['PATH']}",
                }
                subprocess.run(
                    ["bash", "-euo", "pipefail", "-c", script],
                    cwd=ROOT,
                    env=environment,
                    check=True,
                )
                invocations = [
                    shlex.split(line)
                    for line in args_log.read_text(encoding="utf-8").splitlines()
                ]
                self.assertEqual(2, len(invocations))
                nextest = invocations[1]
                self.assertEqual(["nextest", "run"], nextest[:2])
                if trusted:
                    filter_index = nextest.index("-E")
                    self.assertEqual(expected_filter, nextest[filter_index + 1])
                else:
                    self.assertNotIn("-E", nextest)

    def test_ci_policy_accepts_bazel_skip_only_for_untrusted_events(self) -> None:
        needs = {
            job: {"result": "success", "outputs": {}}
            for job in ci_plan.UNGATED_JOBS
            | set(ci_plan.GATED_JOBS)
            | {ci_plan.BAZEL_TEST_JOB}
        }
        needs["plan"]["outputs"] = {
            "docs_only": "false",
            "fail_open": "false",
            **{family: "true" for family in ci_plan.FAMILIES},
        }
        for job in ci_plan.TRUNK_ONLY_JOBS | ci_plan.QUEUE_REQUIRED_COMPILE_JOBS:
            needs[job]["result"] = "skipped"
        needs[ci_plan.BAZEL_TEST_JOB]["result"] = "skipped"
        self.assertEqual(
            [], ci_plan.evaluate_conclusion(needs, "pull_request", bazel_is_trusted=False)
        )
        problems = ci_plan.evaluate_conclusion(
            needs, "pull_request", bazel_is_trusted=True
        )
        self.assertTrue(any(ci_plan.BAZEL_TEST_JOB in problem for problem in problems))


if __name__ == "__main__":
    unittest.main()
