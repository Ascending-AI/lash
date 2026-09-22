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


def generated_batches() -> dict[str, list[str]]:
    """`WORKSPACE_TEST_BATCHES`: batch label -> member test labels."""
    source = (ROOT / "tools/bazel/workspace_targets.bzl").read_text(encoding="utf-8")
    match = re.search(
        r"^WORKSPACE_TEST_BATCHES = (\{.*?^\})$",
        source,
        flags=re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError("missing generated dict WORKSPACE_TEST_BATCHES")
    return ast.literal_eval(match.group(1))


def inventory() -> dict[str, object]:
    return json.loads(
        (ROOT / "tools/bazel/target-inventory.json").read_text(encoding="utf-8")
    )


def inventory_targets() -> list[dict[str, object]]:
    return [
        target | {"package": package["package"]}
        for package in inventory()["packages"]
        for target in package["targets"]
    ]


def test_targets() -> list[dict[str, object]]:
    return [
        target
        for target in inventory_targets()
        if target["label"] is not None and target["kind"] in TEST_KINDS
    ]


def labels_tagged(targets: list[dict[str, object]], *tags: str) -> set[str]:
    """Labels carrying any of `tags` -- the inventory's own partition facts."""
    return {
        target["label"] for target in targets if set(tags) & set(target["tags"])
    }


def workflow() -> dict[str, object]:
    return yaml.load(
        (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8"),
        Loader=yaml.BaseLoader,
    )


def job_step(job: dict[str, object], name: str) -> dict[str, str]:
    return next(step for step in job["steps"] if step.get("name") == name)


def shared_cache_action() -> dict[str, object]:
    parsed = yaml.load(
        (ROOT / ".github/actions/bazel-shared-cache/action.yml").read_text(
            encoding="utf-8"
        ),
        Loader=yaml.BaseLoader,
    )
    return parsed["runs"]


def generated_nextest_terms() -> set[tuple[str, str, str | None]]:
    source = (ROOT / "tools/bazel/cargo_owned_nextest_filter.txt").read_text(
        encoding="utf-8"
    )
    term_pattern = re.compile(
        r"\(package\(([^)]+)\) & kind\(([^)]+)\)"
        r"(?: & binary\(([^)]+)\))?\)"
    )
    if source.strip() == "none()":
        return set()
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
    def test_source_ownership_keeps_shared_helpers_without_sibling_suites(self) -> None:
        policy = json.loads((ROOT / "tools/bazel/source-ownership.json").read_text())
        package = ROOT / "crates/lash-core"
        sources = {
            name: {path.relative_to(package).as_posix() for pattern in patterns for path in package.glob(pattern)}
            for name, patterns in policy["crates/lash-core"]["tests"].items()
        }
        shared = "tests/runtime_support/effect_controller_doubles.rs"
        self.assertIn(shared, sources["runtime_effect"] & sources["runtime_scenarios"])
        self.assertIn("tests/runtime/tests/effect/turn_cancel_modes.rs", sources["runtime_effect"])
        scenarios = "tests/runtime/tests/runtime_scenarios/cases.rs"
        self.assertIn(scenarios, sources["runtime_scenarios"])
        self.assertNotIn(scenarios, sources["runtime_effect"])
        self.assertNotIn("tests/runtime/tests/effect/turn_cancel_modes.rs", sources["runtime_scenarios"])

    def test_core_execution_unit_sources_exclude_relocated_wire_suite(self) -> None:
        policy = json.loads((ROOT / "tools/bazel/source-ownership.json").read_text())
        package = ROOT / "crates/lash-core-execution"
        policy = policy["crates/lash-core-execution"]
        unit = {path.relative_to(package).as_posix() for pattern in policy["unit_test_sources"] for path in package.glob(pattern)}
        integration = {path.relative_to(package).as_posix() for pattern in policy["tests"]["process_model"] for path in package.glob(pattern)}
        self.assertIn("src/runtime/effect/tool_child_driver/tests.rs", unit)
        self.assertIn("tests/runtime/process/lease_serde_tests.rs", integration)
        self.assertFalse(unit & integration)

    def test_source_ownership_rejects_stale_and_escaping_patterns(self) -> None:
        sys.path.insert(0, str(ROOT / "tools/bazel"))
        import generate_build_files as generator
        from unittest.mock import patch

        package = "crates/lash-core"
        metadata = {
            "workspace_members": ["core"],
            "packages": [{
                "id": "core",
                "manifest_path": str(ROOT / package / "Cargo.toml"),
                "targets": [{"name": "runtime_effect", "kind": ["test"], "src_path": str(ROOT / package / "tests/runtime_effect.rs")}],
            }],
        }
        for patterns in [
            ["tests/runtime_effect.rs", "tests/removed/**/*.rs"],
            ["tests/runtime_effect.rs", "../lash-core-execution/src/lib.rs"],
            ["tests/runtime/tests/effect.rs"],
        ]:
            with self.subTest(patterns=patterns), patch.object(generator, "SOURCE_OWNERSHIP", {package: {"tests": {"runtime_effect": patterns}}}):
                with self.assertRaises(ValueError):
                    generator.validate_source_ownership(metadata)

    def test_generated_inventory_is_current(self) -> None:
        subprocess.run(
            ["python3", "tools/bazel/generate_build_files.py", "--check"],
            cwd=ROOT,
            check=True,
        )

    def test_inventory_carries_every_package_dependency_set(self) -> None:
        """The dependency fact that replaces a nested `cargo metadata` call.

        `crates/lash-core/tests/integration_boundary.rs` proves dependency
        direction from this list, so a package entry that lost it (or a
        `lash-internal-core` entry whose dependencies went empty) would turn
        that architecture gate into a vacuous pass.
        """
        packages = inventory()["packages"]
        for package in packages:
            with self.subTest(package=package["package"]):
                dependencies = package["dependencies"]
                self.assertIsInstance(dependencies, list)
                self.assertEqual(sorted(set(dependencies)), dependencies)
        core = next(
            package for package in packages if package["package"] == "lash-internal-core"
        )
        self.assertIn("serde", core["dependencies"])

    def test_inventory_counts_describe_its_own_target_list(self) -> None:
        """The inventory's summary counts are projections of its target list.

        They are the only counts this contract reads, and they are read back
        against the list they summarise, never against a literal: a target
        added with a regenerated inventory moves both halves together, and a
        hand edit to one half is what this refuses.
        """
        payload = inventory()
        labelled = [
            target for target in inventory_targets() if target["label"] is not None
        ]
        unlabelled = [
            target for target in inventory_targets() if target["label"] is None
        ]
        self.assertEqual(len(payload["packages"]), payload["cargo_package_count"])
        self.assertEqual(len(labelled), payload["generated_label_count"])
        self.assertEqual(len(unlabelled), payload["cargo_only_target_count"])
        # `cargo_target_count` is Cargo's own target list; every labelled
        # entry here stands for at least one Cargo target, but a library
        # yields both a lib and a unit-test label, so the inventory can only
        # be at least that large.
        self.assertGreaterEqual(len(labelled), payload["cargo_target_count"])

    def test_generated_suite_partitions_every_executable_test(self) -> None:
        """Every executable test label lands in exactly one generated partition.

        FIG-3477: the partitions are derived from the tags the inventory
        records for each label, never from a count. A target added without
        regenerating the inventory is refused by
        `test_generated_inventory_is_current`; a target added with a
        regenerated inventory moves every list here in step and needs no edit
        to this file.
        """
        targets = test_targets()
        all_labels = {target["label"] for target in targets}
        bazel_labels = set(generated_list("WORKSPACE_BAZEL_TEST_TARGETS"))
        cargo_labels = set(generated_list("WORKSPACE_CARGO_TEST_TARGETS"))
        deferred_labels = set(generated_list("WORKSPACE_DEFERRED_TEST_TARGETS"))
        dev_labels = set(generated_list("WORKSPACE_DEV_TEST_TARGETS"))

        # The tags are the partition facts; the lists are their projections.
        manual = labels_tagged(targets, "manual")
        pr_deferred = labels_tagged(targets, "pr-deferred")
        dev_deferred = labels_tagged(targets, "dev-deferred")
        self.assertEqual(manual, cargo_labels)
        self.assertEqual(pr_deferred, deferred_labels)
        self.assertEqual(all_labels - manual - pr_deferred, bazel_labels)
        self.assertEqual(bazel_labels - dev_deferred, dev_labels)
        # Every partition is populated, so a generator that stopped tagging
        # would fail here instead of passing on three empty sets.
        for name, labels in (
            ("bazel", bazel_labels),
            ("dev", dev_labels),
            ("cargo", cargo_labels),
            ("pr-deferred", deferred_labels),
            ("dev-deferred", bazel_labels - dev_labels),
        ):
            with self.subTest(partition=name):
                self.assertTrue(labels)
        self.assertFalse(bazel_labels & cargo_labels)
        self.assertFalse(bazel_labels & deferred_labels)
        self.assertFalse(cargo_labels & deferred_labels)
        self.assertEqual(all_labels, bazel_labels | cargo_labels | deferred_labels)
        self.assertEqual(all_labels, set(generated_list("WORKSPACE_TEST_TARGETS")))

        # FIG-3365: batched members ride inside their package's `:test_batch`
        # action. This reconciliation is what makes a silently dropped member
        # a CI failure: every coverage-listed member either appears in the
        # suite itself or inside exactly one batch, and every batch sits in
        # the suite.
        batches = generated_batches()
        batched = {member for members in batches.values() for member in members}
        suite_labels = set(generated_list("WORKSPACE_TEST_SUITE_LABELS"))
        self.assertFalse(batched - bazel_labels)
        self.assertEqual(bazel_labels - batched, suite_labels - set(batches))
        self.assertLessEqual(set(batches), suite_labels)
        for members in batches.values():
            self.assertGreaterEqual(len(members), 2)
        dev_suite = set(generated_list("WORKSPACE_DEV_SUITE_LABELS"))
        self.assertEqual((dev_labels - batched) | set(batches), dev_suite)

        # The parallel tail leg must stay a strict subset of the suite: a
        # label in the tail but not the suite would silently run nowhere on
        # the main leg's `//:workspace_tests -//:workspace_tail_tests`.
        # The tail is two measured shapes -- every `//examples/` leaf and every
        # `dev-deferred` label -- each in the form the suite carries it (its
        # batch when batched, itself otherwise). Asserting the set from those
        # two facts, not from a list of labels, is what lets a new example
        # crate land without an edit here while a dev-deferred label the tail
        # forgot still fails.
        tail_suite = set(generated_list("WORKSPACE_TAIL_SUITE_LABELS"))
        self.assertLessEqual(tail_suite, suite_labels)
        batch_of = {
            member: batch for batch, members in batches.items() for member in members
        }
        self.assertEqual(
            {label for label in suite_labels if label.startswith("//examples/")}
            | {batch_of.get(label, label) for label in dev_deferred},
            tail_suite,
        )
        self.assertTrue(dev_deferred)

        by_label = {target["label"]: target for target in targets}
        for label in sorted(cargo_labels):
            with self.subTest(cargo_owned=label):
                self.assertTrue(by_label[label]["cargo_only"])

        # Every Cargo-owned label records exactly one exception class, drawn
        # from the closed vocabulary the generator's classifier emits, and
        # every class in that vocabulary is still in use -- so a class that
        # emptied out (its last label moved to the partition) is a conscious
        # edit here, not a silent one.
        excluded = {"cargo-service-gate", "cargo-trybuild", "cargo-frontend-assets"}
        exception_classes = collections.Counter()
        for label in sorted(cargo_labels):
            classes = excluded & set(by_label[label]["tags"])
            with self.subTest(cargo_owned=label):
                self.assertEqual(1, len(classes))
            exception_classes.update(classes)
        self.assertEqual(excluded, set(exception_classes))

        expected_nextest = set()
        for target in targets:
            if target["label"] not in cargo_labels:
                continue
            if excluded.intersection(target["tags"]):
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
        # Every deterministic Cargo-owned binary has moved to the Bazel
        # partition: what remains is service-gated, trybuild, or
        # frontend-asset-bound, all of which the filter excludes by design.
        self.assertEqual(set(), expected_nextest)
        self.assertEqual(expected_nextest, generated_nextest_terms())

    def test_workspace_suite_and_cli_default_to_the_generated_partition(self) -> None:
        root_build = (ROOT / "BUILD.bazel").read_text(encoding="utf-8")
        self.assertIn('name = "workspace_tests"', root_build)
        self.assertIn("tests = WORKSPACE_TEST_SUITE_LABELS", root_build)
        self.assertIn('name = "dev_tests"', root_build)
        self.assertIn("tests = WORKSPACE_DEV_SUITE_LABELS", root_build)

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
                ["test", "--config=local", "//:dev_tests"],
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
            {"pull_request", "workflow_dispatch", "merge_group"},
            set(triggers),
        )
        self.assertFalse((ROOT / ".github/workflows/bazel.yml").exists())

        trust_expression = jobs["plan"]["outputs"]["bazel_trusted"]
        self.assertEqual(
            "${{ github.event_name == 'merge_group' "
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
        self.assertEqual(
            "needs.plan.outputs.bazel_trusted == 'true' && needs.plan.outputs.rust == 'true'",
            bazel_job["if"],
        )
        self.assertIn("bazel-tests", jobs["ci-conclusion"]["needs"])

        # The tail leg runs `//:workspace_tail_tests` on a parallel runner
        # under the identical trust gate and pool environment, so the
        # partition's wall clock is max(legs), not sum.
        tail_job = jobs["bazel-tests-tail"]
        self.assertEqual("build-cache", tail_job["environment"])
        self.assertEqual(bazel_job["if"], tail_job["if"])
        tail_run = job_step(
            tail_job, "Test the workspace tail suite with shared cache"
        )["run"]
        self.assertIn("//:workspace_tail_tests", tail_run)
        self.assertIn("${BAZEL_SHARED_CACHE_FLAGS}", tail_run)
        self.assertIn("--remote_download_outputs=minimal", tail_run)
        self.assertIn("--cache_test_results=yes", tail_run)
        self.assertIn("bazel-tests-tail", jobs["ci-conclusion"]["needs"])
        self.assertEqual(
            "${{ needs.plan.outputs.bazel_trusted }}",
            job_step(jobs["ci-conclusion"], "Validate CI conclusion")["env"][
                "BAZEL_TRUSTED"
            ],
        )

    def test_ci_enrolls_the_contract_and_configures_the_shared_pool(self) -> None:
        jobs = workflow()["jobs"]
        repository_tests = job_step(jobs["repo-gates"], "Test repository scripts")[
            "run"
        ].splitlines()
        self.assertEqual(
            1,
            repository_tests.count("python3 scripts/test_bazel_test_contract.py"),
        )

        setup = shared_cache_action()
        flags = job_step(setup, "Export shared cache flags")["run"]
        bazel_command = job_step(
            jobs["bazel-tests"], "Test deterministic workspace suite with shared cache"
        )["run"]
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        self.assertIn("test --cache_test_results=yes", bazelrc)
        self.assertIn("--remote_local_fallback=false", flags)
        self.assertIn("--cache_test_results=yes --test_output=errors", bazel_command)
        self.assertIn("${BAZEL_SHARED_CACHE_FLAGS}", bazel_command)
        self.assertNotIn("github_runner_runtime", flags)
        self.assertNotIn("spawn_strategy=local", flags)

    def test_no_deployment_fact_is_committed(self) -> None:
        """Where the pool lives is a deployment fact, not a repository fact.

        The endpoint, the instance, the runtime fingerprint, the client
        certificate paths and this host's cache directories move when the pool
        is redeployed or the executor is repinned, and they differ between a
        development host and a CI runner. `.bazelrc` imports kiln's generated
        `.kiln.bazelrc` for the local copy, CI reads `build-cache` environment
        secrets for its own, and neither copy is committed.
        """
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        self.assertIn("try-import %workspace%/.kiln.bazelrc", bazelrc)
        self.assertIn(
            "/.kiln.bazelrc", (ROOT / ".gitignore").read_text(encoding="utf-8")
        )
        # What the pool is asked FOR stays in the repository. The values are
        # the small-action defaults; anything heavier carries its own
        # `exec_properties` from `tools/bazel/action-sizes.json`.
        self.assertIn(
            "build --remote_default_exec_properties=cpu_count=1", bazelrc
        )
        self.assertIn(
            "build --remote_default_exec_properties=memory_kb=2097152", bazelrc
        )
        self.assertIn("build:shared --remote_local_fallback=false", bazelrc)
        action = (ROOT / ".github/actions/bazel-shared-cache/action.yml").read_text()
        for property_name in ("cpu_count", "memory_kb"):
            self.assertNotIn(f"--remote_default_exec_properties={property_name}=", action)


        sources = [(pathlib.Path(".bazelrc"), bazelrc)]
        for path in sorted((ROOT / ".github").rglob("*")):
            if path.is_file():
                sources.append(
                    (
                        path.relative_to(ROOT),
                        path.read_text(encoding="utf-8", errors="surrogateescape"),
                    )
                )
        sources.append(
            (
                pathlib.Path("scripts/ci_plan.py"),
                (ROOT / "scripts/ci_plan.py").read_text(encoding="utf-8"),
            )
        )
        patterns = (
            # Loopback is a property of the runner a service job stands up,
            # not of where the pool lives.
            (r"\b(?!127\.|0\.0\.0\.0)\d{1,3}(?:\.\d{1,3}){3}\b", "an IP address"),
            (r"kiln-runtime-sha256-", "an executor runtime fingerprint"),
            (r"--remote_instance_name=(?!\$)", "a literal REAPI instance name"),
            (r"--remote_(?:executor|cache)=grpc", "a literal pool endpoint"),
            (r"--tls_[a-z_]*=(?![\"$])", "a literal certificate path"),
            (r"/home/[a-z]+/", "a home-directory path"),
        )
        for path, text in sources:
            for pattern, description in patterns:
                self.assertIsNone(
                    re.search(pattern, text),
                    f"{path} carries {description}; it is a deployment fact and "
                    "belongs in .kiln.bazelrc or a build-cache secret",
                )

    def test_transient_release_asset_fetches_are_retried_and_cached(self) -> None:
        """A 5xx from someone else's CDN must not abort analysis.

        Every Bazel job fetches the module graph and its release assets before
        it compiles anything, so one HTTP 500 from the BCR or from a GitHub
        release used to end the run with nothing built. Measured on Bazel
        9.1.0 against a server answering 500, the stock defaults give eight
        attempts over 17s; `--experimental_repository_downloader_retries` adds
        nothing for this error class, so `--http_connector_attempts` carries
        the window and the per-retry cap keeps its doubling backoff from
        pricing the extra attempts in hours. Retrying is only half of it: the
        repository cache is content-addressed, so an asset already seen is
        never re-fetched at all, which is why every Bazel job restores it and
        exactly one saves it.
        """
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        self.assertIn("common --http_connector_attempts=20", bazelrc)
        self.assertIn("common --http_connector_retry_max_timeout=15s", bazelrc)
        self.assertIn(
            "common --experimental_repository_downloader_retries=5", bazelrc
        )

        setup = shared_cache_action()
        flags = job_step(setup, "Export shared cache flags")["run"]
        self.assertIn("--repository_cache=$RUNNER_TEMP/bazel-repository", flags)
        restore = job_step(setup, "Restore Bazel repository cache")
        save = job_step(setup, "Restore and save Bazel repository cache")
        self.assertEqual("${{ runner.temp }}/bazel-repository", restore["with"]["path"])
        self.assertEqual("${{ runner.temp }}/bazel-repository", save["with"]["path"])
        self.assertTrue(restore["uses"].startswith("actions/cache/restore@"))
        self.assertTrue(save["uses"].startswith("actions/cache@"))
        # Exactly one job uploads the warmed cache; the rest restore only.
        self.assertEqual("inputs.save-repository-cache != 'true'", restore["if"])
        self.assertEqual("inputs.save-repository-cache == 'true'", save["if"])
        saving_jobs = [
            job
            for job in workflow()["jobs"].values()
            for step in job.get("steps", [])
            if step.get("uses") == "./.github/actions/bazel-shared-cache"
            and step.get("with", {}).get("save-repository-cache") == "true"
        ]
        self.assertEqual(1, len(saving_jobs))

    def test_the_output_base_is_restored_everywhere_and_saved_once(self) -> None:
        """Every Bazel job restores the previous run's output base; one saves.

        The output base carries the local action cache and the materialised
        runfiles trees that dominate the merge group's local-side bookkeeping.
        Persisting it under RUNNER_TEMP is useless -- RUNNER_TEMP dies with the
        runner -- so the shared action restores it into that path and exactly
        one job (Lint, which runs on every trusted event) uploads it again.
        """
        setup = shared_cache_action()
        restore = job_step(setup, "Restore Bazel output base")
        save = job_step(setup, "Restore and save Bazel output base")
        self.assertEqual("${{ runner.temp }}/bazel-output", restore["with"]["path"])
        self.assertEqual("${{ runner.temp }}/bazel-output", save["with"]["path"])
        self.assertTrue(restore["uses"].startswith("actions/cache/restore@"))
        self.assertTrue(save["uses"].startswith("actions/cache@"))
        self.assertEqual("inputs.save-output-base != 'true'", restore["if"])
        self.assertEqual("inputs.save-output-base == 'true'", save["if"])
        saving_jobs = [
            job
            for job in workflow()["jobs"].values()
            for step in job.get("steps", [])
            if step.get("uses") == "./.github/actions/bazel-shared-cache"
            and step.get("with", {}).get("save-output-base") == "true"
        ]
        self.assertEqual(1, len(saving_jobs))

    def test_the_shared_cache_action_fails_closed_on_a_bad_secret(self) -> None:
        """A misconfigured environment must name what is wrong, not build wrong.

        A Bazel invocation with no executor falls back to a two-core runner
        compile and one advertising the wrong runtime matches nothing in the
        pool, so an empty value can neither be defaulted nor ignored. The
        values are masked, so a malformed endpoint surfaces from Bazel only as
        `Invalid DNS name: ***`; the shape is checked here instead. A secret is
        stored as text and routinely arrives with a trailing newline, which is
        normalized rather than rejected.
        """
        verify = job_step(shared_cache_action(), "Verify shared pool configuration")[
            "run"
        ]
        names = [
            "CACHE_ENDPOINT",
            "CACHE_INSTANCE",
            "KILN_EXECUTOR_RUNTIME",
            "CACHE_CA",
            "CACHE_CERT",
            "CACHE_KEY",
        ]
        supplied = {name: f"value-of-{name}" for name in names}
        supplied["CACHE_ENDPOINT"] = "grpcs://cache.example:8443"

        def run_verify(environment: dict[str, str]) -> subprocess.CompletedProcess[str]:
            with tempfile.TemporaryDirectory() as temporary:
                output = pathlib.Path(temporary) / "output"
                result = subprocess.run(
                    ["bash", "-c", verify],
                    cwd=ROOT,
                    env=os.environ | environment | {"GITHUB_OUTPUT": str(output)},
                    capture_output=True,
                    text=True,
                )
                result.stdout = (
                    output.read_text(encoding="utf-8") if output.exists() else ""
                )
                return result

        complete = run_verify(supplied)
        self.assertEqual(0, complete.returncode, complete.stderr)
        self.assertIn("endpoint=grpcs://cache.example:8443", complete.stdout)

        trailing = run_verify(
            supplied
            | {
                "CACHE_ENDPOINT": "grpcs://cache.example:8443\n",
                "KILN_EXECUTOR_RUNTIME": "kiln-runtime-sha256-abc\n",
            }
        )
        self.assertEqual(0, trailing.returncode, trailing.stderr)
        self.assertIn("endpoint=grpcs://cache.example:8443\n", trailing.stdout)
        self.assertIn("runtime=kiln-runtime-sha256-abc\n", trailing.stdout)

        for name in names:
            with self.subTest(missing=name):
                result = run_verify(supplied | {name: ""})
                self.assertEqual(1, result.returncode)
                self.assertIn(name, result.stderr)

        for malformed in ("cache.example:8443", "grpcs://cache.example", "https://x:1"):
            with self.subTest(endpoint=malformed):
                result = run_verify(supplied | {"CACHE_ENDPOINT": malformed})
                self.assertEqual(1, result.returncode)
                self.assertIn("CACHE_ENDPOINT", result.stderr)

        # Masked before any flag carrying them is written.
        for value in ("endpoint", "instance", "runtime"):
            self.assertIn(f'echo "::add-mask::${{{value}}}"', verify)
        flags = job_step(shared_cache_action(), "Export shared cache flags")["run"]
        self.assertIn("--remote_executor=${POOL_ENDPOINT}", flags)
        self.assertIn("--remote_cache=${POOL_ENDPOINT}", flags)
        self.assertIn("--remote_instance_name=${POOL_INSTANCE}", flags)
        self.assertIn(
            "--remote_default_exec_properties=kiln_executor_runtime=${POOL_RUNTIME}",
            flags,
        )

    def run_workspace_test_step(self, trusted: bool, workbench: bool):
        """Execute the workspace job's test step against a recording `cargo`."""

        jobs = workflow()["jobs"]
        workspace_step = job_step(jobs["workspace-tests"], "Test workspace")
        self.assertEqual(
            "${{ needs.plan.outputs.bazel_trusted }}",
            workspace_step["env"]["BAZEL_TRUSTED"],
        )
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = pathlib.Path(temporary)
            args_log = temporary_path / "cargo-args"
            args_log.write_text("", encoding="utf-8")
            fake_cargo = temporary_path / "cargo"
            fake_cargo.write_text(
                "#!/usr/bin/env bash\nprintf '%q ' \"$@\" >> \"$CARGO_ARGS_LOG\"\nprintf '\\n' >> \"$CARGO_ARGS_LOG\"\n",
                encoding="utf-8",
            )
            fake_cargo.chmod(0o755)
            python_log = temporary_path / "python-args"
            python_log.write_text("", encoding="utf-8")
            fake_python = temporary_path / "python3"
            fake_python.write_text(
                "#!/usr/bin/env bash\nprintf '%q ' \"$@\" >> \"$PYTHON_ARGS_LOG\"\nprintf '\\n' >> \"$PYTHON_ARGS_LOG\"\n",
                encoding="utf-8",
            )
            fake_python.chmod(0o755)
            environment = os.environ | {
                "BAZEL_TRUSTED": str(trusted).lower(),
                "CARGO_ARGS_LOG": str(args_log),
                "PYTHON_ARGS_LOG": str(python_log),
                "LASH_CI_FEATURES": "",
                "PATH": f"{temporary}{os.pathsep}{os.environ['PATH']}",
            }
            script_with_plan = workspace_step["run"].replace(
                "${{ needs.plan.outputs.workbench }}", str(workbench).lower()
            )
            completed = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", script_with_plan],
                cwd=ROOT,
                env=environment,
                capture_output=True,
                text=True,
            )
            invocations = [
                shlex.split(line)
                for line in args_log.read_text(encoding="utf-8").splitlines()
            ]
            python_invocations = [
                shlex.split(line)
                for line in python_log.read_text(encoding="utf-8").splitlines()
            ]
            return completed, invocations, python_invocations

    def test_a_trusted_event_runs_no_cargo_rust_workspace_partition(self) -> None:
        """The Bazel partition owns every deterministic Rust binary.

        `tools/bazel/cargo_owned_nextest_filter.txt` is therefore `none()`, and
        a trusted event with no workbench diff must refuse to run rather than
        launder an empty selection into a green Cargo job.
        """
        self.assertEqual(
            "none()",
            (ROOT / "tools/bazel/cargo_owned_nextest_filter.txt")
            .read_text(encoding="utf-8")
            .strip(),
        )
        completed, invocations, _ = self.run_workspace_test_step(
            trusted=True, workbench=False
        )
        self.assertEqual(1, completed.returncode)
        self.assertIn("without workbench", completed.stderr)
        self.assertEqual([], invocations)

    def test_a_trusted_event_runs_exactly_the_workbench_partition(self) -> None:
        expected_filter = (
            ROOT / "tools/bazel/workbench_nextest_filter.txt"
        ).read_text(encoding="utf-8").strip()
        completed, invocations, _ = self.run_workspace_test_step(
            trusted=True, workbench=True
        )
        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertEqual(2, len(invocations))
        nextest = invocations[1]
        self.assertEqual(["nextest", "run"], nextest[:2])
        self.assertEqual(expected_filter, nextest[nextest.index("-E") + 1])

    def test_a_trusted_event_builds_only_the_workbench_package(self) -> None:
        """The selection is one binary, so the build must be one package.

        A `cargo build --workspace` here compiled and linked every workspace
        crate on a 2-core runner before running a single Node-gated test, which
        made this job the CI tail (836 s against the Bazel partition's 145 s).
        The trusted path builds `agent-workbench`'s test targets and nothing
        else; the store conformance helper example belongs to the untrusted
        full-workspace path, which still spawns it.
        """
        _, invocations, _ = self.run_workspace_test_step(trusted=True, workbench=True)
        build, nextest = invocations
        self.assertEqual("build", build[0])
        self.assertNotIn("--workspace", build)
        self.assertNotIn("--example", build)
        self.assertEqual(["--package", "agent-workbench"], build[-3:-1])
        self.assertIn("--tests", build)
        self.assertNotIn("--workspace", nextest)
        self.assertIn("--package", nextest)
        self.assertEqual(
            "agent-workbench", nextest[nextest.index("--package") + 1]
        )

    def test_the_workbench_split_covers_every_workbench_case_exactly_once(
        self,
    ) -> None:
        """Bazel skips by name exactly what the Cargo filter selects by name.

        The workbench unit binary is partition-owned apart from the cases that
        shell out to `node --test`. Those two halves are generated from one
        `bazel_skipped` record; this refuses a hand edit that skips a case in
        Bazel without selecting it in Cargo, or selects one Cargo never runs.
        """
        workbench = next(
            target
            for target in inventory_targets()
            if target["package"] == "agent-workbench"
            and target["kind"] == "bin-unit-test"
        )
        self.assertEqual([], workbench["tags"])
        self.assertNotIn("cargo_only", workbench)
        skipped = workbench["bazel_skipped"]
        self.assertEqual(sorted(skipped), skipped)
        self.assertTrue(skipped)
        build_file = (
            ROOT / "examples/agent-workbench/BUILD.bazel"
        ).read_text(encoding="utf-8")
        for test in skipped:
            self.assertIn(f'"--skip={test}"', build_file)
        expected_filter = " + ".join(
            sorted(
                "((package(agent-workbench) & kind(bin) &"
                f" binary(agent-workbench)) & test(={test}))"
                for test in skipped
            )
        )
        self.assertEqual(
            expected_filter,
            (ROOT / "tools/bazel/workbench_nextest_filter.txt")
            .read_text(encoding="utf-8")
            .strip(),
        )
        source = (
            ROOT / "examples/agent-workbench/src/main_sections/tests/recoverable_chat.rs"
        ).read_text(encoding="utf-8")
        self.assertIn('Command::new("node")', source)

    def test_an_untrusted_event_keeps_the_full_cargo_workspace_run(self) -> None:
        completed, invocations, python_invocations = self.run_workspace_test_step(
            trusted=False, workbench=False
        )
        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertEqual(2, len(invocations))
        nextest = invocations[1]
        self.assertEqual(["nextest", "run"], nextest[:2])
        expression = nextest[nextest.index("-E") + 1]
        self.assertIn("not (", expression)
        self.assertIn("lash-internal-postgres-store", expression)
        # FIG-3429 item 8: the untrusted leg must census law execution
        # receipts over every crate whose conformance suites it ran.
        self.assertEqual(1, len(python_invocations))
        self.assertEqual(
            "scripts/check_law_execution_receipts.py", python_invocations[0][0]
        )
        self.assertIn("--crate", python_invocations[0])
        self.assertIn("crates/lash-sqlite-store", python_invocations[0])

    def test_doctests_are_removed_from_bazel_and_from_cargo(self) -> None:
        """Doctests were removed by ruling (2026-09-13), Bazel and Cargo alike.

        A doc-test label reappearing in the inventory, a `rust_doc_test`
        wrapper returning to the rule file, or a workspace library whose
        manifest stops saying `doctest = false` would each silently restore a
        gate the repository no longer runs, so all three are refused here.
        """
        self.assertEqual(
            [],
            [
                target
                for target in inventory_targets()
                if target["kind"] == "doc-test"
            ],
        )
        targets_bzl = (ROOT / "tools/bazel/workspace_targets.bzl").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("WORKSPACE_DOCTEST_TARGETS", targets_bzl)

        root_build = (ROOT / "BUILD.bazel").read_text(encoding="utf-8")
        self.assertNotIn("workspace_doctests", root_build)

        rules = (ROOT / "tools/bazel/lash_rust.bzl").read_text(encoding="utf-8")
        self.assertNotIn("rust_doc_test", rules)

        generator = (ROOT / "tools/bazel/generate_build_files.py").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("doc_test", generator)

        # Cargo is the other half: `cargo test` must never compile a doc
        # snippet again, which is a per-manifest `[lib] doctest = false`.
        missing = [
            str(manifest.relative_to(ROOT))
            for manifest in sorted(ROOT.glob("crates/*/Cargo.toml"))
            + sorted(ROOT.glob("examples/*/Cargo.toml"))
            + sorted(ROOT.glob("runbooks/*/Cargo.toml"))
            if (manifest.parent / "src/lib.rs").exists()
            and "doctest = false"
            not in manifest.read_text(encoding="utf-8")
        ]
        self.assertEqual([], missing)

    def test_clippy_partition_is_the_all_targets_shape(self) -> None:
        targets = [
            target for target in inventory_targets() if target["label"] is not None
        ]
        build_scripts = {
            target["label"] for target in targets if target["kind"] == "custom-build"
        }
        clippy = set(generated_list("WORKSPACE_CLIPPY_TARGETS"))

        # `cargo clippy --workspace --all-targets` lints every target of the
        # resolved default graph. The Bazel partition is the same set minus the
        # labels that only RUN a build script, each of which carries its own
        # recorded exemption; the `build.rs` compile behind each of them is in
        # the partition as its `:build_script_` target.
        self.assertEqual(
            set(generated_list("WORKSPACE_COMPILE_TARGETS")) - build_scripts,
            clippy,
        )
        # The partition's size is the inventory's own label count less the
        # exemptions it records -- a fact read from the inventory, not pinned.
        self.assertEqual(
            inventory()["generated_label_count"] - len(build_scripts), len(clippy)
        )
        self.assertTrue(build_scripts)
        # The half that actually compiles `build.rs` is linted, not exempted.
        self.assertIn("//crates/lash-protocol-rlm:build_script_", clippy)
        self.assertTrue(
            all(
                target.get("clippy_exempt")
                for target in targets
                if target["label"] in build_scripts
            )
        )

        root_build = (ROOT / "BUILD.bazel").read_text(encoding="utf-8")
        self.assertIn(
            'lash_rust_clippy(\n    name = "workspace_clippy",\n'
            "    testonly = True,\n    deps = WORKSPACE_CLIPPY_TARGETS,\n)",
            root_build,
        )

    def test_every_lintable_label_contributes_a_clippy_marker(self) -> None:
        # `lash_rust_clippy` fails analysis when a dep contributes no clippy
        # marker, so membership in WORKSPACE_CLIPPY_TARGETS is the same fact as
        # "this label was linted". The partition may therefore only omit a
        # labelled target that records why, and the exemption list is asserted
        # here rather than left implicit in the generator's filter.
        targets = [
            target for target in inventory_targets() if target["label"] is not None
        ]
        clippy = set(generated_list("WORKSPACE_CLIPPY_TARGETS"))
        exempt = {
            target["label"]: target["clippy_exempt"]
            for target in targets
            if target.get("clippy_exempt")
        }

        self.assertEqual(
            {target["label"] for target in targets},
            clippy | set(exempt),
        )
        self.assertFalse(clippy & set(exempt))
        self.assertEqual(
            {
                "//crates/lash-protocol-rlm:build_script": (
                    "the run action has no CrateInfo; its build.rs compile is "
                    "linted as :build_script_"
                )
            },
            exempt,
        )

        clippy_bzl = (ROOT / "tools/bazel/clippy.bzl").read_text(encoding="utf-8")
        self.assertIn('fail("clippy produced no marker for: {}"', clippy_bzl)

    def test_lint_and_doc_jobs_branch_on_the_shared_trust_decision(self) -> None:
        jobs = workflow()["jobs"]
        trusted = "needs.plan.outputs.bazel_trusted == 'true'"
        untrusted = "needs.plan.outputs.bazel_trusted != 'true'"

        for job_id in ("lint", "check"):
            with self.subTest(job=job_id):
                self.assertEqual("plan", jobs[job_id]["needs"])
        self.assertEqual("build-cache", jobs["lint"]["environment"])

        clippy_bazel = job_step(
            jobs["lint"], "Clippy (workspace, all targets, shared cache)"
        )
        self.assertEqual(trusted, clippy_bazel["if"])
        self.assertIn("//:workspace_clippy", clippy_bazel["run"])

        clippy_cargo = job_step(jobs["lint"], "Clippy (workspace, all targets)")
        self.assertEqual(untrusted, clippy_cargo["if"])
        self.assertIn(
            "cargo clippy --workspace --all-targets --locked "
            "${LASH_CI_FEATURES} -- -D warnings",
            clippy_cargo["run"],
        )

        # The `e2e` feature is outside the resolved default graph, but it is no
        # longer outside Bazel: the feature-lane generator emits a variant of
        # every unit that resolution compiles, and `//:feature_lane_clippy`
        # lints exactly those. The Cargo command stays for untrusted events,
        # which have no cache credentials and so no pool.
        self.assertIn("//:feature_lane_clippy", clippy_bazel["run"])

        e2e = job_step(jobs["lint"], "Clippy (slack-clone e2e feature)")
        self.assertEqual(untrusted, e2e["if"])
        self.assertIn(
            "cargo clippy -p slack-clone --all-targets --features e2e "
            "--locked --no-deps -- -D warnings",
            e2e["run"],
        )

        # The workspace compile proof rides the Lint job's
        # `//:workspace_clippy` (asserted above): the clippy action
        # type-checks the same all-targets shape `cargo check --workspace
        # --all-targets` used to, so the partition does not build
        # `//:workspace_compile` a second time on its own critical path.
        # Remote execution needs the action result, not hundreds of MiB of
        # top-level binaries.
        bazel_test = job_step(
            jobs["bazel-tests"], "Test deterministic workspace suite with shared cache"
        )
        self.assertIn("//:workspace_tests", bazel_test["run"])
        self.assertIn("-//:workspace_tail_tests", bazel_test["run"])
        self.assertNotIn("//:workspace_compile", bazel_test["run"])
        self.assertNotIn("workspace_doctests", bazel_test["run"])
        self.assertIn("--remote_download_outputs=minimal", bazel_test["run"])

        self.assertNotIn("--doc", yaml.safe_dump(jobs["check"]))

        setup = job_step(jobs["lint"], "Configure Bazel shared cache")
        self.assertEqual(
            "./.github/actions/bazel-shared-cache", setup["uses"]
        )
    def test_service_jobs_never_reuse_a_cached_service_test_result(self) -> None:
        """A cached green for a live-service test is a false green.

        Both halves matter: the labels a service job executes must be absent
        from the cacheable `//:workspace_tests` aggregate, and every Bazel
        invocation in scripts/ci/store-tests.sh must refuse cached results and
        keep them off the shared cache. The `--modify_execution_info` filter is
        scoped to `TestRunner` so the compile actions stay shared.
        """
        bazel_labels = set(generated_list("WORKSPACE_BAZEL_TEST_TARGETS"))
        service_labels = set()
        for service in ("postgres", "minio"):
            labels = (
                ROOT / f"tools/bazel/{service}_test_labels.txt"
            ).read_text(encoding="utf-8").split()
            self.assertTrue(labels)
            service_labels.update(labels)
        self.assertFalse(service_labels & bazel_labels)

        by_label = {target["label"]: target for target in test_targets()}
        for label in service_labels:
            self.assertIn("cargo-service-gate", by_label[label]["tags"])

        script = (ROOT / "scripts/ci/store-tests.sh").read_text(encoding="utf-8")
        self.assertIn("--nocache_test_results", script)
        self.assertIn(
            "--modify_execution_info=TestRunner=+no-cache,"
            "TestRunner=+no-remote-cache,TestRunner=+no-remote-exec",
            script,
        )
        self.assertNotIn("--cache_test_results=yes", script)
        # One database, one bucket: the binaries must not overlap.
        self.assertIn("--local_test_jobs=1", script)

    def test_store_jobs_run_the_same_suites_on_both_trust_paths(self) -> None:
        jobs = workflow()["jobs"]
        suites = []
        for job_id in ("postgres-store", "s3-store"):
            job = jobs[job_id]
            self.assertEqual("build-cache", job["environment"])
            self.assertEqual(
                "${{ needs.plan.outputs.bazel_trusted }}", job["env"]["BAZEL_TRUSTED"]
            )
            for step in job["steps"]:
                run = step.get("run", "")
                if "scripts/ci/store-tests.sh" in run:
                    suites.append(run.split()[-1])
                # Cargo toolchain setup exists only for the untrusted path.
                if step.get("uses", "").startswith("./.github/actions/rust-toolchain"):
                    self.assertEqual(
                        "needs.plan.outputs.bazel_trusted != 'true'", step["if"]
                    )
        self.assertEqual(
            [
                "pg-catalog-compatibility",
                "pg-store",
                "pg-pool-wait",
                "pg-agent-scenario",
                "pg-sim-backend-faults",
                "pg-cross-backend",
                "s3-store",
                "s3-attachment-differential",
            ],
            suites,
        )

        # Every suite the workflow names must dispatch on both trust decisions,
        # and no suite may exist that the workflow never runs.
        script = (ROOT / "scripts/ci/store-tests.sh").read_text(encoding="utf-8")
        shaped = set(re.findall(r"^  ([a-z0-9-]+)\)$", script, flags=re.MULTILINE))
        table = script.split("declare -A uniform_store_suites=(\n", 1)[1]
        table = table.split("\n)\n", 1)[0]
        uniform = set(re.findall(r"^\s*\[([^\]]+)\]=", table, flags=re.MULTILINE))
        self.assertEqual(set(suites), shaped | uniform)
        self.assertEqual(set(), shaped & uniform)

        # A suite whose shape varies keeps writing both halves itself.
        for suite in shaped:
            body = script.split(f"\n  {suite})\n", 1)[1].split("\n    ;;", 1)[0]
            self.assertIn('if [ "${trusted}" = true ]; then', body)
            self.assertIn("cargo ", body)

        # A uniform suite states its selection once; the dispatcher renders it
        # into whichever dialect the trust decision calls for.
        dispatcher = script.split("run_uniform_store_suite() {", 1)[1].split("\n}", 1)[0]
        self.assertIn('if [ "${trusted}" = true ]; then', dispatcher)
        self.assertIn("render_bazel_suite", dispatcher)
        self.assertIn("render_cargo_suite", dispatcher)
        for suite in uniform:
            self.assertIn(f"[{suite}]=", table)

    def test_ci_policy_accepts_bazel_skip_only_for_untrusted_events(self) -> None:
        needs = {
            job: {"result": "success", "outputs": {}}
            for job in ci_plan.UNGATED_JOBS
            | set(ci_plan.GATED_JOBS)
            | ci_plan.BAZEL_TEST_JOBS
        }
        needs["plan"]["outputs"] = {
            "docs_only": "false",
            "fail_open": "false",
            **{family: "true" for family in ci_plan.FAMILIES},
        }
        for job in ci_plan.DISPATCH_ONLY_JOBS:
            needs[job]["result"] = "skipped"
        for job in ci_plan.BAZEL_TEST_JOBS:
            needs[job]["result"] = "skipped"
        self.assertEqual(
            [], ci_plan.evaluate_conclusion(needs, "pull_request", bazel_is_trusted=False)
        )
        problems = ci_plan.evaluate_conclusion(
            needs, "pull_request", bazel_is_trusted=True
        )
        self.assertTrue(any(ci_plan.BAZEL_TEST_JOB in problem for problem in problems))


if __name__ == "__main__":
    unittest.main()
