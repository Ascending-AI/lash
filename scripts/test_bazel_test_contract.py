#!/usr/bin/env python3
"""Contract tests for the generated Bazel test-cache surface."""

from __future__ import annotations

import ast
import collections
import json
import tomllib
import importlib.util
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

        # The tail leg runs only for merge groups and dispatches, preserving
        # the complete combined-tree partition without duplicating PR work.
        tail_job = jobs["bazel-tests-tail"]
        self.assertEqual("build-cache", tail_job["environment"])
        self.assertEqual(
            bazel_job["if"] + " && github.event_name != 'pull_request'",
            tail_job["if"],
        )
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
            jobs["bazel-tests"], "Test the workspace core suite with shared cache"
        )["run"]
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        self.assertIn("test --cache_test_results=yes", bazelrc)
        # Every test writes its own JUnit report (tools/bazel/test_xml.bzl).
        self.assertIn("test --run_under=//tools/bazel:test_xml_runner", bazelrc)
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
        never re-fetched at all, which is why every Bazel job restores it.
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
        self.assertEqual("${{ runner.temp }}/bazel-repository", restore["with"]["path"])
        self.assertTrue(restore["uses"].startswith("actions/cache/restore@"))
        self.assertEqual("${{ steps.keys.outputs.repository }}", restore["with"]["key"])
        keys = job_step(setup, "Resolve Bazel cache keys")["run"]
        self.assertIn(
            "hashFiles('MODULE.bazel.lock', 'MODULE.bazel', 'Cargo.lock')", keys
        )

    def test_bazel_caches_are_restored_by_ci_and_saved_only_on_main(self) -> None:
        """Every Bazel job restores; only cache-warm.yml, on main, saves.

        An Actions cache is readable only from the ref that wrote it and from
        the default branch. ci.yml never runs on `main`, so a save from one of
        its jobs lands under a PR or merge-queue ref nothing else reads: 883
        repository-cache misses against 99 hits in the 2026-09-22/23 window.
        The shared action is restore-only and exports its keys; cache-warm.yml
        saves exactly those keys from a `main` run.
        """
        setup = shared_cache_action()
        for step in setup["steps"]:
            uses = step.get("uses", "")
            with self.subTest(step=step.get("name")):
                self.assertFalse(uses.startswith("actions/cache@"), uses)
                self.assertFalse(uses.startswith("actions/cache/save@"), uses)
        output_base = job_step(setup, "Restore Bazel output base")
        self.assertEqual("${{ runner.temp }}/bazel-output", output_base["with"]["path"])
        self.assertTrue(output_base["uses"].startswith("actions/cache/restore@"))
        self.assertEqual("${{ steps.keys.outputs.output_base }}", output_base["with"]["key"])
        action = yaml.load(
            (ROOT / ".github/actions/bazel-shared-cache/action.yml").read_text(encoding="utf-8"),
            Loader=yaml.BaseLoader,
        )
        self.assertNotIn("inputs", action)
        self.assertEqual(
            {"repository-cache-key", "repository-cache-hit", "output-base-key", "output-base-hit"},
            set(action["outputs"]),
        )

        ci = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertNotIn("actions/cache/save@", ci)
        self.assertNotIn("save-repository-cache", ci)
        self.assertNotIn("save-output-base", ci)

        warm_workflow = yaml.load(
            (ROOT / ".github/workflows/cache-warm.yml").read_text(encoding="utf-8"),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual({"branches": ["main"]}, warm_workflow["on"]["push"])
        self.assertIn("schedule", warm_workflow["on"])
        warm = warm_workflow["jobs"]["warm-bazel"]
        setup_step = job_step(warm, "Configure Bazel shared cache")
        self.assertEqual("cache", setup_step["id"])
        for name, path, key, hit in (
            ("Save Bazel repository cache", "bazel-repository",
             "repository-cache-key", "repository-cache-hit"),
            ("Save Bazel output base", "bazel-output", "output-base-key", "output-base-hit"),
        ):
            with self.subTest(save=name):
                save = job_step(warm, name)
                self.assertTrue(save["uses"].startswith("actions/cache/save@"))
                self.assertEqual(f"${{{{ runner.temp }}}}/{path}", save["with"]["path"])
                self.assertEqual(f"${{{{ steps.cache.outputs.{key} }}}}", save["with"]["key"])
                self.assertEqual(f"steps.cache.outputs.{hit} != 'true'", save["if"])

    def test_client_flags_have_one_home_and_ci_keeps_execution_logs(self) -> None:
        """Remote-connection flags live in `.bazelrc`, which CI inherits.

        `build` lines reach CI's own flag list, so a keepalive or timeout set
        there needs no second copy in the shared action. Test runs download
        their logs, not their binaries, and every CI Bazel leg uploads a
        compact execution log for cache-miss forensics.
        """
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        for line in (
            "build --grpc_keepalive_time=30s",
            "build --remote_timeout=600",
            "startup --max_idle_secs=600",
            "startup --host_jvm_args=-XX:G1PeriodicGCInterval=60000",
            "build:shared --remote_download_outputs=minimal",
            "test:shared --remote_download_regex=.*/testlogs/.*/"
            "(test[.]log|test[.]xml|test[.]outputs/.*)$",
        ):
            self.assertIn(line + "\n", bazelrc)
        self.assertNotIn("test:shared --remote_download_outputs", bazelrc)
        flags = job_step(shared_cache_action(), "Export shared cache flags")["run"]
        self.assertNotIn("--remote_timeout", flags)
        self.assertNotIn("--grpc_keepalive_time", flags)

        jobs = workflow()["jobs"]
        for job_id, step_name in (
            ("bazel-tests", "Test the workspace core suite with shared cache"),
            ("bazel-tests-tail", "Test the workspace tail suite with shared cache"),
            ("lint", "Clippy (workspace, all targets, shared cache)"),
        ):
            with self.subTest(job=job_id):
                run = job_step(jobs[job_id], step_name)["run"]
                self.assertIn(
                    '--execution_log_compact_file="${RUNNER_TEMP}/bazel-exec-log.binpb.zst"',
                    run,
                )
                upload = job_step(jobs[job_id], "Upload Bazel execution log")
                self.assertEqual(
                    "${{ runner.temp }}/bazel-exec-log.binpb.zst", upload["with"]["path"]
                )

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
        a trusted event must refuse the Cargo job, including on workbench diffs.
        """
        self.assertEqual(
            "none()",
            (ROOT / "tools/bazel/cargo_owned_nextest_filter.txt")
            .read_text(encoding="utf-8")
            .strip(),
        )
        for workbench in (False, True):
            with self.subTest(workbench=workbench):
                completed, invocations, _ = self.run_workspace_test_step(
                    trusted=True, workbench=workbench
                )
                self.assertEqual(1, completed.returncode)
                self.assertIn("must use the Bazel partition", completed.stderr)
                self.assertEqual([], invocations)

    def test_workbench_browser_projection_has_a_pinned_bazel_node_input(self) -> None:
        """The full workbench unit binary executes under Bazel with Node in runfiles."""
        workbench = next(
            target
            for target in inventory_targets()
            if target["package"] == "agent-workbench"
            and target["kind"] == "bin-unit-test"
        )
        self.assertEqual([], workbench["tags"])
        self.assertNotIn("cargo_only", workbench)
        self.assertNotIn("bazel_skipped", workbench)
        build_file = (
            ROOT / "examples/agent-workbench/BUILD.bazel"
        ).read_text(encoding="utf-8")
        self.assertIn('"@workbench_node_linux_x64//:bin/node"', build_file)
        self.assertIn('"LASH_WORKBENCH_TEST_NODE"', build_file)
        self.assertNotIn("--skip=tests::recoverable_chat_tests::workbench_browser", build_file)
        source = (
            ROOT / "examples/agent-workbench/src/main_sections/tests/recoverable_chat.rs"
        ).read_text(encoding="utf-8")
        self.assertIn('var_os("LASH_WORKBENCH_TEST_NODE")', source)

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
        # which have no cache credentials and so no pool. The feature-lane
        # lint is merge-group breadth: a pull request lints only the
        # workspace and builds the schema checks. That every feature variant
        # compiles is the `feature-lanes` job's proof on every trusted event
        # (FIG-3572), so Lint names no single lane unit.
        self.assertIn(
            "targets=(//:workspace_clippy //:schema_checks)", clippy_bazel["run"]
        )
        self.assertIn(
            'if [[ "$GITHUB_EVENT_NAME" != pull_request ]]; then\n'
            "  targets+=(//:feature_lane_clippy)",
            clippy_bazel["run"],
        )

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
            jobs["bazel-tests"], "Test the workspace core suite with shared cache"
        )
        # One suite for every trusted event: cached results scope the run.
        self.assertIn("-- //:workspace_tests -//:workspace_tail_tests", bazel_test["run"])
        self.assertNotIn("GITHUB_EVENT_NAME", bazel_test["run"])
        self.assertEqual(
            ["Check out repository", "Configure shared build cache",
             "Test the workspace core suite with shared cache"],
            [step["name"] for step in jobs["bazel-tests"]["steps"][:3]],
        )
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
        for service in ("postgres", "s3"):
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
                    suites.append(run.split("scripts/ci/store-tests.sh", 1)[1].split()[0])
                # Cargo toolchain setup exists only for the untrusted path.
                if step.get("uses", "").startswith("./.github/actions/rust-toolchain"):
                    self.assertEqual(
                        "needs.plan.outputs.bazel_trusted != 'true'", step["if"]
                    )
        self.assertEqual(
            [
                "pg-store",
                "pg-pool-wait",
                "pg-sim-backend-faults",
                "pg-cross-backend",
                "pg-catalog-compatibility",
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
        # The feature lanes need the pool as much as the Bazel partition does.
        for job in ci_plan.BAZEL_TEST_JOBS | {ci_plan.FEATURE_LANES_JOB}:
            needs[job]["result"] = "skipped"
        self.assertEqual(
            [], ci_plan.evaluate_conclusion(needs, "pull_request", bazel_is_trusted=False)
        )
        problems = ci_plan.evaluate_conclusion(
            needs, "pull_request", bazel_is_trusted=True
        )
        self.assertTrue(any(ci_plan.BAZEL_TEST_JOB in problem for problem in problems))



class FocusedClippyVerdicts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("run_bazel_clippy", ROOT / "scripts/run-bazel-clippy.py")
        cls.driver = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.driver)

    def verdict(self, labels, marked, *, empty=False, incomplete=False):
        events = []
        for label, configuration in labels:
            completed = {"label": label, "configuration": {"id": configuration}}
            events.append({"id": {"targetConfigured": {"label": label}}, "children": [{"targetCompleted": completed}]})
        for label, configuration in marked:
            events.append({"id": {"targetCompleted": {"label": label, "configuration": {"id": configuration}}}, "completed": {"success": True, "outputGroup": [{"name": "clippy_checks", "fileSets": [{"id": "parent"}], "incomplete": incomplete}]}})
        events.extend([
            {"id": {"namedSet": {"id": "parent"}}, "namedSetOfFiles": {"fileSets": [{"id": "child"}]}},
            {"id": {"namedSet": {"id": "child"}}, "namedSetOfFiles": {"files": [] if empty else [{"name": "crate.lash-clippy.ok"}]}},
        ])
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "events.json"
            path.write_text("\n".join(json.dumps(event) for event in events))
            self.driver.validate_events(path)

    def test_every_requested_configuration_needs_a_completed_marker(self):
        first = ("//crate:lib", "default")
        second = ("//crate:lib", "feature")
        self.verdict([first, second], [first, second])
        for requested, marked, kwargs in [
            ([], [], {}),
            ([first, second], [first], {}),
            ([first, ("//:Cargo.toml", "file")], [first], {}),
            ([first], [first], {"empty": True}),
            ([first], [first], {"incomplete": True}),
        ]:
            with self.subTest(requested=requested, kwargs=kwargs), self.assertRaisesRegex(ValueError, "no Clippy verdict"):
                self.verdict(requested, marked, **kwargs)

    def test_options_and_event_destination_are_preserved_and_failures_propagate(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            fake = directory / "bazel"
            captured = directory / "arguments.json"
            fake.write_text("#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\nPath(" + repr(str(captured)) + ").write_text(json.dumps(sys.argv[1:]))\nsys.exit(7)\n")
            fake.chmod(0o755)
            event_path = directory / "requested.json"
            flags = ["--config=shared", "--keep_going", "--output_groups=+custom", "--build_event_json_file", str(event_path), "--", "//crate:lib"]
            self.assertEqual(7, self.driver.run(str(fake), flags))
            arguments = json.loads(captured.read_text())
            self.assertEqual(["build", *flags[:5]], arguments[:6])
            self.assertIn(str(event_path), arguments)
            self.assertIn("--output_groups=+clippy_checks", arguments)
            self.assertIn("--aspects=" + self.driver.ASPECT, arguments)
            self.assertEqual(["--", "//crate:lib"], arguments[-2:])
            self.assertFalse(any(arg.startswith("--build_event_json_file=") for arg in arguments))

class CargoTargetSelectionTests(unittest.TestCase):
    def emit(self, arguments, required_features=()):
        from unittest.mock import Mock, patch

        sys.path.insert(0, str(ROOT / "tools/bazel"))
        import generate_build_files as generator

        library = {"name": "example", "kind": ["lib"], "test": True}
        targets = [library, *(
            {"name": name, "kind": ["test"], "test": True}
            for name in ("process_model", "effect_model", "other")
        ), {"name": "app", "kind": ["bin"], "test": True}]
        targets[1]["required-features"] = list(required_features)
        graph = generator.FeatureLaneGraph.__new__(generator.FeatureLaneGraph)
        graph.by_name = {"example": {"targets": targets}}
        graph.library_of = Mock(return_value=library)
        graph.emit_target = Mock(side_effect=lambda package, resolution, target, kind, runnable, args:
                                 f"{target['name']}:{kind}")
        graph.units = []
        command = generator.feature_variants.parse_command(
            ["cargo", "test", "-p", "example", "--no-default-features", *arguments]
        )
        tests = []
        with patch.object(generator, "cargo_test_policy", return_value=(False, "")):
            compiled = graph.emit_root_targets(command, {"example": []}, tests)
        return command, compiled, tests, graph.emit_target.call_args_list

    def test_named_integrations_compile_binaries_without_unit_harnesses(self):
        command, compiled, tests, calls = self.emit([
            "--test", "process_model", "--test", "effect_model", "--locked"
        ])
        self.assertEqual(["process_model:test", "effect_model:test", "app:bin"], compiled)
        self.assertEqual(["process_model:test", "effect_model:test"], tests)
        self.assertEqual(("process_model", "effect_model"), command.tests)
        self.assertFalse(command.default_features)
        self.assertTrue(command.with_dev)
        for call in calls:
            self.assertEqual({"example": []}, call.args[1])
            self.assertEqual([], call.args[-1])

    def test_unavailable_named_targets_fail_instead_of_running_no_tests(self):
        with self.assertRaisesRegex(ValueError, "unknown --test target.*misspelled"):
            self.emit(["--test", "misspelled"])
        with self.assertRaisesRegex(ValueError, "process_model requires missing features.*testing"):
            self.emit(["--test", "process_model"], required_features=("testing",))
        with self.assertRaisesRegex(ValueError, "literal --test names"):
            self.emit(["--test", "*_model"])

    def test_default_and_explicit_unit_selections_remain_runnable(self):
        for flags in ([], ["--tests"], ["--all-targets"]):
            with self.subTest(flags=flags):
                _, compiled, tests, _ = self.emit(flags)
                self.assertIn("example:unit-test", tests)
                self.assertIn("app:bin-unit-test", tests)
                self.assertIn("other:test", tests)
                self.assertIn("app:bin", compiled)
        _, compiled, tests, _ = self.emit(["--lib"])
        self.assertEqual(["example:unit-test"], compiled)
        self.assertEqual(compiled, tests)

    def test_named_integration_can_be_combined_with_explicit_library_tests(self):
        _, compiled, tests, _ = self.emit(["--lib", "--test", "process_model"])
        self.assertEqual(["example:unit-test", "process_model:test", "app:bin"], compiled)
        self.assertEqual(["example:unit-test", "process_model:test"], tests)

    def test_case_filter_and_harness_arguments_do_not_change_selection(self):
        command, compiled, tests, calls = self.emit([
            "--test=process_model", "effect_model", "--", "--exact", "--ignored"
        ])
        self.assertEqual(["process_model:test"], tests)
        self.assertNotIn("effect_model:test", compiled)
        self.assertEqual(("effect_model", "--exact", "--ignored"), command.test_args)
        for call in calls:
            self.assertEqual(list(command.test_args), call.args[-1])


class CargoResolutionTests(unittest.TestCase):
    def test_repeated_tree_markers_preserve_features_and_still_reject_drift(self):
        import contextlib
        import io
        from types import SimpleNamespace
        from unittest.mock import patch

        sys.path.insert(0, str(ROOT / "tools/bazel"))
        import generate_build_files as generator

        plan = {"lane": [{"commands": [["cargo", "check", "-p", "example"]]}]}
        for features, output, expected in (
            (["enabled"], "example v1.0.0|enabled\nexample v1.0.0|enabled (*)\n", 0),
            ([], "example v1.0.0|\nexample v1.0.0| (*)\n", 0),
            (["enabled"], "example v1.0.0|changed\nexample v1.0.0|changed (*)\n", 1),
        ):
            with self.subTest(features=features, expected=expected), \
                    patch.object(generator.feature_variants.Workspace, "from_metadata", return_value=SimpleNamespace(packages={"example": None})), \
                    patch.object(generator, "feature_coverage_plan", return_value=plan), \
                    patch.object(generator.feature_variants, "resolve_request", return_value=SimpleNamespace(sorted_features=lambda: {"example": features})), \
                    patch.object(generator.subprocess, "run", side_effect=lambda argv, **kwargs: SimpleNamespace(
                        stdout=output if "--color=never" in argv else output.replace("(*)", "\x1b[33m\x1b[2m(*)\x1b[39m\x1b[22m")
                    )), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(generator.verify_resolution({}), expected)


class OptLevelMirrorTests(unittest.TestCase):
    def test_cargo_profile_opt_levels_reach_bazel(self):
        sys.path.insert(0, str(ROOT / "tools/bazel"))
        import generate_build_files as generator

        profile = tomllib.loads((ROOT / "Cargo.toml").read_text())["profile"]["dev"]["package"]
        rc = (ROOT / "tools/bazel/opt_levels.bazelrc").read_text()
        module = (ROOT / "tools/bazel/opt_levels.MODULE.bazel").read_text()
        self.assertIn('include("//tools/bazel:opt_levels.MODULE.bazel")', (ROOT / "MODULE.bazel").read_text())
        self.assertIn("import %workspace%/tools/bazel/opt_levels.bazelrc", (ROOT / ".bazelrc").read_text())
        for name, settings in profile.items():
            level = settings["opt-level"]
            if name in generator.PROC_MACRO_HOST_CRATES:
                self.assertNotIn(f'crate = "{name}"', module)
            elif name.startswith("lash-"):
                self.assertIn(f"/{name}:@-Copt-level={level}", rc)
            else:
                self.assertIn(f'crate = "{name}",\n    rustc_flags = ["-Copt-level={level}"]', module)


class PackagePolicyTests(unittest.TestCase):
    def setUp(self):
        sys.path.insert(0, str(ROOT / "tools/bazel"))
        import generate_build_files as generator

        self.generator = generator
        self.metadata = {
            "workspace_members": ["sim"],
            "packages": [{
                "id": "sim",
                "name": "lash-sim",
                "targets": [
                    {"name": "cross_backend_a", "kind": ["test"]},
                    {"name": "helper", "kind": ["bin"]},
                ],
            }],
        }

    def check(self, rule):
        from unittest.mock import patch

        with patch.object(self.generator, "PACKAGE_POLICY", {"rule": [rule]}):
            self.generator.validate_package_policy(self.metadata)

    def test_rules_that_name_nothing_are_refused(self):
        for rule in (
            {"packages": ["lash-missing"]},
            {"packages": ["lash-sim"], "kinds": ["binary"]},
            {"packages": ["lash-sim"], "targets": ["cross_frontend*"]},
            {"packages": ["lash-sim"], "bin_env": {"HELPER": "absent"}},
            {"packages": ["lash-sim"], "colour": "red"},
        ):
            with self.subTest(rule=rule), self.assertRaises(ValueError):
                self.check(rule)
        self.check({"packages": ["lash-sim"], "kinds": ["test"], "targets": ["cross_backend*"],
                    "bin_env": {"HELPER": "helper"}})

    def test_rules_fold_in_file_order(self):
        from unittest.mock import patch

        rules = [
            {"packages": ["lash-sim"], "kinds": ["test"], "compile_data": ["//:a"], "tags": ["manual"]},
            {"packages": ["lash-sim"], "targets": ["cross_*"], "compile_data": ["//:b"], "serial": True},
            {"packages": ["lash-sim"], "kinds": ["unit-test"], "compile_data": ["//:c"]},
        ]
        with patch.object(self.generator, "PACKAGE_POLICY", {"rule": rules}):
            policy = self.generator.target_policy("lash-sim", "test", "cross_backend_a")
        self.assertEqual(policy.compile_data, ["//:a", "//:b"])
        self.assertEqual(policy.env, {"RUST_TEST_THREADS": "1"})
        self.assertEqual(policy.tags, ["manual"])


if __name__ == "__main__":
    unittest.main()
