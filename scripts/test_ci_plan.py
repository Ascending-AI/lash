#!/usr/bin/env python3
"""Unit tests for ci_plan.py."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

import re

import yaml

import ci_plan


ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = ROOT / ".github/workflows/ci.yml"
# `CI conclusion` is the only required check on `main`, and `evaluate_conclusion`
# reasons over a static job map that has to agree with `ci-conclusion`'s
# `needs:`. A job that is dropped from both -- while its definition stays in
# `ci.yml` and keeps running -- is consistent to the aggregator and invisible to
# it, so `test_every_ci_job_is_registered_or_allowlisted` below is the only
# thing that catches that partial removal. An entry here silences that guard for
# one job permanently, so it carries the reason the job legitimately sits
# outside the aggregator, and `AggregatorAllowlistTests` pins the membership
# against a hand-written literal: appending to it is a deliberate, reviewed edit
# rather than a one-word append nothing else sees (FIG-2824).
AGGREGATOR_ALLOWLIST = {
    # Structural: a job cannot appear in its own `needs:`, so the aggregator can
    # never register itself. Its own failure is the required check failing.
    "ci-conclusion": "the aggregator itself; a job cannot list itself in needs",
}


def unregistered_ci_jobs(
    workflow_source: str, allowlist: dict[str, str] | None = None
) -> set[str]:
    workflow = yaml.safe_load(workflow_source)
    jobs = workflow["jobs"]
    aggregator_needs = jobs["ci-conclusion"]["needs"]
    if allowlist is None:
        allowlist = AGGREGATOR_ALLOWLIST
    return set(jobs) - set(aggregator_needs) - set(allowlist)


class ConfidenceConclusionTests(unittest.TestCase):
    def needs(self, event="workflow_dispatch", selector="full"):
        return {job: {"result": "success" if event in {"schedule", "workflow_dispatch"}
                      and ((selector != "full") if policy == "selector" else selector == "full")
                      else "skipped"} for job, policy in ci_plan.CONFIDENCE_JOB_POLICY.items()}

    def test_confidence_job_policy_matches_workflow_and_producer_edges(self):
        jobs = yaml.safe_load(CI_WORKFLOW.with_name("confidence.yml").read_text())["jobs"]
        self.assertEqual(set(jobs) - {"confidence-conclusion"}, set(ci_plan.CONFIDENCE_JOB_POLICY))
        self.assertEqual(set(ci_plan.CONFIDENCE_JOB_POLICY), set(jobs["confidence-conclusion"]["needs"]))
        self.assertEqual("full-producer", ci_plan.CONFIDENCE_JOB_POLICY["confidence-build"])
        for job, policy in ci_plan.CONFIDENCE_JOB_POLICY.items():
            if policy == "full-consumer":
                self.assertEqual("confidence-build", jobs[job]["needs"])

    def test_confidence_event_and_selector_policy(self):
        for event in ("schedule", "workflow_dispatch", "merge_group"):
            for selector in (("full", "full+area:sim", "fast") if event != "schedule" else ("full",)):
                with self.subTest(event=event, selector=selector):
                    self.assertEqual([], ci_plan.evaluate_confidence_conclusion(self.needs(event, selector), event, selector))

    def test_every_confidence_stage_fails_closed(self):
        for job in ci_plan.CONFIDENCE_JOB_POLICY:
            selector = "fast" if job == "confidence" else "full"
            for result in ("failure", "cancelled", "skipped", None, "missing"):
                with self.subTest(job=job, result=result):
                    needs = self.needs(selector=selector)
                    if result == "missing":
                        del needs[job]
                    else:
                        needs[job]["result"] = result
                    self.assertTrue(any(job in p for p in ci_plan.evaluate_confidence_conclusion(needs, "workflow_dispatch", selector)))

    def test_confidence_rejects_unknown_jobs_policies_and_events(self):
        from unittest.mock import patch
        needs = self.needs()
        needs["unmapped"] = {"result": "success"}
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(needs, "workflow_dispatch", "full"))
        with patch.dict(ci_plan.CONFIDENCE_JOB_POLICY, {"confidence-build": "unknown"}):
            self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "workflow_dispatch", "full"))
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "unknown", "full"))
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "schedule", "fast"))
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "workflow_dispatch", ""))


class ClassifyTests(unittest.TestCase):
    def test_docs_only_skips_every_expensive_family(self) -> None:
        plan = ci_plan.classify(
            [("M", "README.md"), ("A", "docs/runbooks/ci.md"), ("M", "runbooks/operator/README.md")]
        )
        self.assertEqual("true", plan["docs_only"])
        self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_file_deletion_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify([("D", "docs/adr/0079-x.md")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual("docs deletion", plan["reason"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_addition_and_modification_preserve_docs_only_skip(self) -> None:
        plan = ci_plan.classify([("A", "docs/new.md"), ("M", "AGENTS.md")])
        self.assertEqual("true", plan["docs_only"])
        self.assertEqual("docs-only diff", plan["reason"])
        self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_markdown_file_stays_docs_only(self) -> None:
        plan = ci_plan.classify([("M", "docs/adr/0079-x.md")])
        self.assertEqual("true", plan["docs_only"])
        self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_deletion_mixed_with_docs_modification_runs_everything(self) -> None:
        plan = ci_plan.classify([("D", "docs/old.md"), ("M", "README.md")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual("docs deletion", plan["reason"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_unknown_status_fails_open(self) -> None:
        plan = ci_plan.classify([("X", "docs/unknown.md")])
        self.assertEqual("true", plan["fail_open"])
        self.assertIn("unknown change statuses", plan["reason"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_rust_change_in_the_workbench_closure_runs_the_cargo_partition(self) -> None:
        """A crate the workbench binary links selects the job that tests it.

        FIG-3049: a session change in lash-core landed with the Cargo
        partition skipped, and the workbench approvals tests it broke only
        surfaced on later, unrelated pull requests.
        """
        plan = ci_plan.classify([("M", "crates/lash-core/src/session/mod.rs")])
        self.assertEqual("true", plan["rust"])
        self.assertEqual("false", plan["stores"])
        self.assertEqual("true", plan["workbench"])
        self.assertEqual("false", plan["regress"])
        self.assertEqual("false", plan["schema"])
        self.assertEqual("false", plan["facade"])
        self.assertEqual("false", plan["tooling"])

    def test_rust_change_outside_the_workbench_closure_skips_the_cargo_partition(self) -> None:
        for path in (
            "crates/lash-s3-store/src/lib.rs",
            "crates/lash-perf/src/lib.rs",
            "examples/toolbench/src/main.rs",
        ):
            with self.subTest(path=path):
                plan = ci_plan.classify([("M", path)])
                self.assertEqual("true", plan["rust"])
                self.assertEqual("false", plan["workbench"])

    def test_the_workbench_closure_is_the_first_party_dependency_graph(self) -> None:
        closure = ci_plan.workbench_dependency_dirs()
        self.assertIn(ci_plan.WORKBENCH_MANIFEST_DIR, closure)
        self.assertIn("crates/lash-core", closure)
        self.assertNotIn("crates/lash-s3-store", closure)

    def test_the_cargo_partition_follows_the_workbench_closure_only(self) -> None:
        """No event forces the partition on: the closure is the sole selector."""
        for changes, expected in (
            ([("M", "docs/adr/0079-x.md")], "false"),
            ([("M", "crates/lash-s3-store/src/lib.rs")], "false"),
            ([("M", "crates/lash-core/src/session/mod.rs")], "true"),
        ):
            with self.subTest(changes=changes):
                for event in ("pull_request", "merge_group"):
                    self.assertEqual(
                        expected, ci_plan.classify(changes, event)["workbench"]
                    )

    def test_a_docs_only_pull_request_still_skips_every_expensive_family(self) -> None:
        for event in ("pull_request", "merge_group"):
            with self.subTest(event=event):
                plan = ci_plan.classify([("M", "docs/adr/0079-x.md")], event)
                self.assertEqual("true", plan["docs_only"])
                self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_an_underivable_closure_fails_open(self) -> None:
        plan = ci_plan.classify(
            [("M", "crates/lash-s3-store/src/lib.rs")], "", frozenset()
        )
        self.assertEqual("false", plan["workbench"])
        with mock.patch.object(
            ci_plan, "workbench_dependency_dirs", side_effect=OSError("no manifest")
        ):
            plan = ci_plan.classify([("M", "crates/lash-s3-store/src/lib.rs")])
        self.assertEqual("true", plan["fail_open"])
        self.assertIn("workbench dependency closure is underivable", plan["reason"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_workbench_only_skips_breadth_but_keeps_the_bazel_partition(self) -> None:
        """The pool owns the workbench unit suite, so `rust` cannot be false.

        `rust` gates `Test Bazel partition`, which runs every agent-workbench
        unit case with a pinned Node interpreter for the browser-projection
        test. The store, functional-E2E and
        worker-E2E breadth families stay off: the workbench is an example
        host, not a store or a worker.
        """
        plan = ci_plan.classify([("M", "examples/agent-workbench/src/main.rs")])
        self.assertEqual("workbench-only diff", plan["reason"])
        self.assertEqual("true", plan["rust"])
        self.assertEqual("false", plan["stores"])
        self.assertEqual("false", plan["functional_e2e"])
        self.assertEqual("false", plan["workers_e2e"])
        self.assertEqual("true", plan["workbench"])
        self.assertEqual("false", plan["facade"])
        self.assertEqual("false", plan["tooling"])

    def test_facade_selects_the_seal_lane(self) -> None:
        for path, expected in (
            ("crates/lash/src/lib.rs", "true"),
            ("Cargo.toml", "true"),
            ("Cargo.lock", "true"),
            ("crates/lash-core/src/lib.rs", "false"),
        ):
            with self.subTest(path=path):
                self.assertEqual(
                    expected, ci_plan.classify([("M", path)])["facade"]
                )

    def test_stores_is_path_derived(self) -> None:
        for path, expected in (
            ("crates/lash-postgres-store/src/lib.rs", "true"),
            ("crates/lash-s3-store/src/lib.rs", "true"),
            ("crates/lash-core-store/src/lib.rs", "true"),
            ("crates/lash-conformance/src/lib.rs", "true"),
            ("crates/lash-sim/src/lib.rs", "true"),
            ("crates/lash-sqlite-store/migrations/0001_init/up.sql", "true"),
            ("crates/lash-core/src/lib.rs", "false"),
            ("examples/agent-workbench/src/main.rs", "false"),
        ):
            with self.subTest(path=path):
                self.assertEqual(
                    expected, ci_plan.classify([("M", path)])["stores"]
                )

    def test_tooling_selects_repo_gates(self) -> None:
        # `scripts/` is a global invalidator, so a script diff turns on every
        # family including tooling; the narrower check is a non-global tooling
        # path that leaves facade and stores off.
        self.assertEqual("true", ci_plan.classify([("M", "scripts/x.py")])["tooling"])
        for path, expected in (
            ("tools/bazel/clippy.bzl", "true"),
            ("BUILD.bazel", "true"),
            ("tools/kiln/main.py", "true"),
            ("crates/lash-core/src/lib.rs", "false"),
        ):
            with self.subTest(path=path):
                self.assertEqual(
                    expected, ci_plan.classify([("M", path)])["tooling"]
                )

    def test_readme_prefixed_rust_file_runs_core_families(self) -> None:
        plan = ci_plan.classify([("A", "crates/x/src/readme_gen.rs")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual("true", plan["rust"])
        self.assertEqual("false", plan["workbench"])

    def test_each_global_invalidator_runs_everything(self) -> None:
        paths = [
            "Cargo.lock",
            "Cargo.toml",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            ".config/nextest.toml",
            ".github/workflows/ci.yml",
            "scripts/ci_plan.py",
            ".github/actions/rust-toolchain/action.yml",
            "justfile",
            "deny.toml",
        ]
        for path in paths:
            with self.subTest(path=path):
                plan = ci_plan.classify([("M", path)])
                self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_unknown_path_fails_open(self) -> None:
        plan = ci_plan.classify([("M", "mystery.data")])
        self.assertEqual("true", plan["fail_open"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})


class PathClassifierTests(unittest.TestCase):
    """`classify_path` is the one table; these pin the classes CI narrows on."""

    def kind(self, path: str) -> "ci_plan.PathKind":
        return ci_plan.classify_path(path).kind

    def test_a_crate_manifest_is_crate_local_not_a_global_invalidator(self) -> None:
        path_class = ci_plan.classify_path("crates/lash-core/Cargo.toml")
        self.assertEqual(ci_plan.PathKind.PACKAGE, path_class.kind)
        self.assertEqual("crates/lash-core", path_class.package)
        self.assertTrue(path_class.manifest)
        plan = ci_plan.classify([("M", "crates/lash-core/Cargo.toml")])
        self.assertEqual("production-relevant diff", plan["reason"])
        self.assertEqual("true", plan["rust"])
        # The repository gates read every manifest (feature-lane resolution,
        # dependency boundary); nothing else widens.
        self.assertEqual("true", plan["tooling"])
        for family in ("stores", "schema", "facade", "regress"):
            self.assertEqual("false", plan[family], family)
        store = ci_plan.classify([("M", "crates/lash-postgres-store/Cargo.toml")])
        self.assertEqual("true", store["stores"])
        self.assertEqual("true", store["schema"])

    def test_the_root_manifests_stay_global(self) -> None:
        for path in ("Cargo.toml", "Cargo.lock"):
            with self.subTest(path=path):
                self.assertEqual(ci_plan.PathKind.SHARED, self.kind(path))
        # A nested workspace's lock file is its own package data, not the root's.
        self.assertEqual(ci_plan.PathKind.DATA, self.kind("fuzz/Cargo.lock"))

    def test_store_fixtures_select_stores_without_failing_open(self) -> None:
        for path in (
            "fixtures/checkpoint-component-v1-refusal/postgres/fixture.sql",
            "fixtures/durable-read/v1/sqlite/durable-core.db",
        ):
            with self.subTest(path=path):
                self.assertEqual(ci_plan.PathKind.DATA, self.kind(path))
                plan = ci_plan.classify([("M", path)])
                self.assertEqual("false", plan["fail_open"])
                self.assertEqual("true", plan["rust"])
                self.assertEqual("true", plan["stores"])
                self.assertEqual("false", plan["facade"])
                self.assertEqual("false", plan["tooling"])

    def test_markdown_outside_a_package_is_docs(self) -> None:
        for path in (
            "AGENTS.md",
            "README.md",
            "fixtures/checkpoint-component-v1-refusal/README.md",
            "schemas/host/README.md",
            "runbooks/agent-service-branching/README.md",
            "docs/agents/pr-style.md",
        ):
            with self.subTest(path=path):
                self.assertEqual(ci_plan.PathKind.DOCS, self.kind(path))
                self.assertEqual("true", ci_plan.classify([("M", path)])["docs_only"])

    def test_markdown_inside_a_bazel_package_is_package_input(self) -> None:
        # `include_str!` and test runfiles read package READMEs.
        for path in ("crates/lash/README.md", "runbooks/rlm-smoke/README.md"):
            with self.subTest(path=path):
                self.assertEqual(ci_plan.PathKind.PACKAGE, self.kind(path))
                self.assertEqual("true", ci_plan.classify([("M", path)])["rust"])

    def test_prose_a_rust_test_reads_runs_rust(self) -> None:
        for path in sorted(ci_plan.RUST_RUNTIME_DOC_INPUTS):
            with self.subTest(path=path):
                self.assertEqual(ci_plan.PathKind.DOC_INPUT, self.kind(path))
                plan = ci_plan.classify([("M", path)])
                self.assertEqual("false", plan["docs_only"])
                self.assertEqual("true", plan["rust"])

    def test_bazel_configuration_is_tooling(self) -> None:
        for path in (".bazelrc", ".bazelversion", "MODULE.bazel", "MODULE.bazel.lock",
                     "BUILD.bazel", "tools/bazel/clippy.bzl", ".gitattributes"):
            with self.subTest(path=path):
                self.assertEqual(ci_plan.PathKind.TOOLING, self.kind(path))
                plan = ci_plan.classify([("M", path)])
                self.assertEqual("false", plan["fail_open"])
                self.assertEqual("true", plan["rust"])
                self.assertEqual("true", plan["tooling"])
                self.assertEqual("false", plan["stores"])

    def test_every_tracked_path_has_a_known_class(self) -> None:
        listed = subprocess.run(
            ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, text=True, check=True
        ).stdout.split("\0")
        unknown = sorted(
            path for path in listed
            if path and ci_plan.classify_path(path).kind is ci_plan.PathKind.UNKNOWN
        )
        self.assertEqual([], unknown)


class RustRuntimeDocInputTests(unittest.TestCase):
    """Keep "docs-only means no Rust" true as the tree evolves.

    The claim rests on prose not failing the Rust suite. That is a claim about
    the *tree*, so it is checked against the tree: every reference to `docs/**`
    or `CONTEXT.md` in a tracked Rust source must name a path the classifier
    already treats as a Rust input. A new test that reads an ADR at run time
    fails here until `RUST_RUNTIME_DOC_INPUTS` catches up.
    """

    # Leading `./` and `../` are stripped first so a crate-relative
    # `include_str!("../../../docs/x.md")` is caught too.
    LITERAL = re.compile(r'"((?:\.{1,2}/)*(?:docs/[^"\s]+|CONTEXT\.md))"')

    @classmethod
    def setUpClass(cls) -> None:
        listed = subprocess.run(
            ["git", "ls-files", "-z", "*.rs"], cwd=ROOT, capture_output=True,
            text=True, check=True,
        )
        cls.sources = [ROOT / entry for entry in listed.stdout.split("\0") if entry]

    def referenced_paths(self) -> dict[str, list[str]]:
        found: dict[str, list[str]] = {}
        for source in self.sources:
            body = source.read_text(encoding="utf-8", errors="replace")
            for match in self.LITERAL.finditer(body):
                path = re.sub(r"^(?:\.{1,2}/)+", "", match.group(1))
                found.setdefault(path, []).append(source.relative_to(ROOT).as_posix())
        return found

    def test_the_sweep_finds_the_sources_it_is_meant_to_read(self) -> None:
        # A silent zero-hit sweep would be a guard that cannot fail.
        self.assertGreater(len(self.sources), 100)

    def test_every_doc_path_named_in_rust_is_a_rust_input(self) -> None:
        offenders = {
            path: sorted(set(sources))
            for path, sources in self.referenced_paths().items()
            if ci_plan.classify_path(path).kind is ci_plan.PathKind.DOCS
        }
        self.assertEqual(
            {}, offenders,
            "these prose paths are read by Rust sources but classify as docs; "
            f"add them to ci_plan.RUST_RUNTIME_DOC_INPUTS: {offenders}",
        )

    def test_the_pinned_list_stays_current(self) -> None:
        referenced = self.referenced_paths()
        stale = sorted(path for path in ci_plan.RUST_RUNTIME_DOC_INPUTS if path not in referenced)
        self.assertEqual([], stale, "no Rust source reads these any more; drop them")


PUSH_GATE = ROOT / "scripts/push-gate.sh"


def push_gate_function(name: str) -> str:
    """One shell function out of push-gate.sh, so the test drives the real one."""
    script = PUSH_GATE.read_text(encoding="utf-8")
    match = re.search(rf"^{re.escape(name)}\(\) \{{\n.*?^\}}\n", script, re.MULTILINE | re.DOTALL)
    assert match, f"missing shell function {name}"
    return match.group(0)


class GateScopeTests(unittest.TestCase):
    """The push gate's projection. Every ambiguity must run everything."""

    RUST = ci_plan.GateFamily.RUST_COMPILE
    SCRIPTS = ci_plan.GateFamily.SCRIPTS
    WORKFLOWS = ci_plan.GateFamily.WORKFLOWS

    def scope(self, *paths: str) -> "ci_plan.GateScope":
        return ci_plan.gate_scope(list(paths))

    def test_docs_skip_every_family_and_name_the_paths(self) -> None:
        scope = self.scope("docs/guide.md", "README.md", "CONTRIBUTING.md")
        self.assertEqual("docs-only", scope.classification)
        self.assertEqual(frozenset(), scope.families)
        self.assertIn("docs/guide.md", scope.reason)

    def test_a_crate_change_runs_compile_only(self) -> None:
        for path in ("crates/lash-core/src/runtime/turn_loop.rs", "crates/lash/README.md",
                     "examples/slack-clone/ui/index.html"):
            with self.subTest(path=path):
                scope = self.scope(path)
                self.assertEqual("rust-only", scope.classification)
                self.assertEqual(frozenset({self.RUST}), scope.families)

    def test_a_crate_manifest_adds_the_script_gates(self) -> None:
        scope = self.scope("crates/lash-core/Cargo.toml")
        self.assertEqual(frozenset({self.RUST, self.SCRIPTS}), scope.families)

    def test_a_pinned_doc_input_runs_the_rust_battery(self) -> None:
        scope = self.scope("docs/adr/0008-confidence-gate.md")
        self.assertEqual("rust-input-docs", scope.classification)
        self.assertEqual(frozenset({self.RUST}), scope.families)

    def test_docs_plus_crates_is_mixed(self) -> None:
        scope = self.scope("docs/guide.md", "crates/lash/src/lib.rs")
        self.assertEqual("mixed", scope.classification)
        self.assertEqual(frozenset({self.RUST}), scope.families)

    def test_shared_inputs_unknown_paths_and_empty_sets_run_everything(self) -> None:
        for paths, classification in (
            (("Cargo.lock",), "shared-inputs"),
            (("docs/guide.md", "Cargo.lock"), "shared-inputs"),
            (("rust-toolchain.toml",), "shared-inputs"),
            ((".github/workflows/ci.yml",), "shared-inputs"),
            (("scripts/ci_plan.py",), "shared-inputs"),
            (("mystery.data",), "unknown-paths"),
            ((), "empty-diff"),
            (("", "  "), "empty-diff"),
        ):
            with self.subTest(paths=paths):
                scope = self.scope(*paths)
                self.assertEqual(classification, scope.classification)
                self.assertEqual(ci_plan.ALL_GATE_FAMILIES, scope.families)

    def test_tooling_runs_compile_and_scripts(self) -> None:
        scope = self.scope("tools/bazel/generate_build_files.py")
        self.assertEqual(frozenset({self.RUST, self.SCRIPTS}), scope.families)

    def test_text_output_lists_every_family_in_ascii(self) -> None:
        lines = ci_plan.render_gate_text(self.scope("docs/guide.md")).splitlines()
        self.assertEqual(len(ci_plan.GATE_FAMILIES) + 1, len(lines))
        self.assertIn("rust-compile: skip", lines)
        self.assertTrue(lines[-1].startswith("classification: docs-only -- "))
        "\n".join(lines).encode("ascii")

    def test_env_output_is_evaluable_and_publishes_the_closed_set(self) -> None:
        env = ci_plan.render_gate_env(self.scope("docs/guide.md"))
        self.assertIn("GATE_RUN_RUST_COMPILE=0", env)
        self.assertIn("GATE_SCOPE_FAMILIES='RUST_COMPILE SCRIPTS WORKFLOWS'", env)
        printed = subprocess.run(
            ["bash", "-c", 'eval "$1"; printf "%s|%s" "$GATE_RUN_SCRIPTS" "$GATE_SCOPE_CLASSIFICATION"',
             "bash", env], capture_output=True, text=True, check=True,
        )
        self.assertEqual("0|docs-only", printed.stdout)

    def test_the_family_set_is_closed_and_ordered(self) -> None:
        self.assertEqual(["rust-compile", "scripts", "workflows"],
                         [str(family) for family in ci_plan.GATE_FAMILIES])
        self.assertEqual(["GATE_RUN_RUST_COMPILE", "GATE_RUN_SCRIPTS", "GATE_RUN_WORKFLOWS"],
                         [family.env_variable for family in ci_plan.GATE_FAMILIES])

    def cli(self, *args: str, stdin: str = "") -> subprocess.CompletedProcess:
        return subprocess.run(
            ["python3", str(ROOT / "scripts/ci_plan.py"), "gate-scope", *args],
            input=stdin, capture_output=True, text=True, check=False,
        )

    def test_cli_reads_paths_from_stdin_without_git(self) -> None:
        result = self.cli("--paths-from", "-", "--format", "env", stdin="Cargo.lock\n")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("GATE_RUN_RUST_COMPILE=1", result.stdout)
        self.assertIn("GATE_SCOPE_CLASSIFICATION=shared-inputs", result.stdout)

    def test_cli_failures_exit_nonzero_and_say_run_everything(self) -> None:
        for args in (("--paths-from", "/nonexistent/fig-1811"),
                     ("--base", "refs/heads/no-such-ref-fig-1811")):
            with self.subTest(args=args):
                result = self.cli(*args)
                self.assertNotEqual(0, result.returncode)
                self.assertIn("run everything", result.stderr)

    def test_cli_against_a_real_repository(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            git = lambda *args: subprocess.run(  # noqa: E731
                ["git", *args], cwd=root, capture_output=True, text=True, check=True)
            git("init", "-q", "-b", "main")
            git("config", "user.email", "gate@example.com")
            git("config", "user.name", "Gate")
            (root / "docs").mkdir()
            (root / "docs/guide.md").write_text("base\n")
            git("add", "-A")
            git("commit", "-qm", "base")
            git("checkout", "-qb", "topic")
            (root / "docs/guide.md").write_text("changed\n")
            git("commit", "-qam", "docs")
            out = self.cli("--repo", str(root), "--base", "main").stdout
            self.assertIn("classification: docs-only", out)
            # A dirty, staged shared input widens the scope unless excluded.
            (root / "Cargo.lock").write_text("dirty\n")
            git("add", "Cargo.lock")
            self.assertIn("rust-compile: run", self.cli("--repo", str(root), "--base", "main").stdout)
            self.assertIn("rust-compile: skip",
                          self.cli("--repo", str(root), "--base", "main", "--no-worktree").stdout)
            # A rename reports both of its paths.
            git("reset", "-q", "Cargo.lock")
            (root / "Cargo.lock").unlink()
            (root / "crates").mkdir()
            (root / "crates/a.rs").write_text("fn main() {}\n")
            git("add", "-A")
            git("commit", "-qm", "add")
            git("mv", "crates/a.rs", "crates/b.rs")
            out = self.cli("--repo", str(root), "--base", "main").stdout
            self.assertIn("crates/a.rs", out)
            self.assertIn("crates/b.rs", out)

    def test_the_push_gate_sets_exactly_the_known_families(self) -> None:
        script = PUSH_GATE.read_text(encoding="utf-8")
        fallback = push_gate_function("gate_scope_apply")
        assigned = set(re.findall(r"^  (GATE_RUN_[A-Z_]+)=", fallback, re.MULTILINE))
        self.assertEqual({family.env_variable for family in ci_plan.GateFamily}, assigned)
        self.assertIn("python3 scripts/ci_plan.py gate-scope", fallback)
        used = set(re.findall(r"^scoped ([A-Z_]+) ", script, re.MULTILINE))
        self.assertTrue(used)
        self.assertLessEqual(used, {family.name for family in ci_plan.GateFamily})

    def gate_family_runs(self, known: str, family: str) -> subprocess.CompletedProcess:
        harness = "\n".join((
            "set -euo pipefail",
            f"GATE_SCOPE_FAMILIES={known!r}",
            "GATE_RUN_RUST_COMPILE=1",
            "GATE_RUN_SCRIPTS=0",
            "GATE_RUN_WORKFLOWS=1",
            push_gate_function("gate_family_runs"),
            push_gate_function("scoped"),
            'ran() { printf "RAN\\n"; }',
            f'scoped {family} "label" ran',
        ))
        return subprocess.run(["bash", "-c", harness], capture_output=True, text=True, check=False)

    def test_the_push_gate_refuses_unknown_families_once_the_set_is_known(self) -> None:
        known = "RUST_COMPILE SCRIPTS WORKFLOWS"
        refused = self.gate_family_runs(known, "NOT_A_FAMILY")
        self.assertEqual(2, refused.returncode, refused.stderr)
        self.assertIn("unknown gate family 'NOT_A_FAMILY'", refused.stderr)
        # Fail-open survives: an unset set means every family runs.
        self.assertIn("RAN", self.gate_family_runs("", "NOT_A_FAMILY").stdout)
        self.assertIn("RAN", self.gate_family_runs(known, "RUST_COMPILE").stdout)
        skipped = self.gate_family_runs(known, "SCRIPTS")
        self.assertNotIn("RAN", skipped.stdout)
        self.assertIn("skipped: label", skipped.stdout)


class DevTestScopeTests(unittest.TestCase):
    """dev-test's projection; its CLI behaviour is `test_dev_test.py`'s."""

    def scope(self, *paths: str, scripts: frozenset[str] = frozenset()):
        return ci_plan.dev_test_scope(list(paths), ROOT, scripts)

    def test_a_package_source_selects_its_package(self) -> None:
        scope = self.scope("crates/lash-core/src/lib.rs", "docs/guide.md")
        self.assertEqual(("//crates/lash-core",), scope.packages)
        self.assertFalse(scope.broad or scope.repository or scope.facade)

    def test_the_facade_crate_names_the_seal(self) -> None:
        self.assertTrue(self.scope("crates/lash/src/lib.rs").facade)
        root = self.scope("Cargo.toml")
        self.assertTrue(root.broad and root.facade)

    def test_a_package_manifest_widens_without_repository_gates(self) -> None:
        scope = self.scope("crates/lash-core/Cargo.toml")
        self.assertTrue(scope.broad)
        self.assertFalse(scope.repository)

    def test_shared_inputs_and_tooling_widen_with_repository_gates(self) -> None:
        for path in ("scripts/unknown.py", ".bazelrc", ".github/workflows/ci.yml"):
            with self.subTest(path=path):
                scope = self.scope(path)
                self.assertTrue(scope.broad and scope.repository)

    def test_the_test_xml_runner_runs_its_self_test(self) -> None:
        tests = frozenset({"scripts/test_test_xml.py"})
        for path in ("tools/bazel/junit_xml.py", "tools/bazel/test_xml_runner.sh"):
            with self.subTest(path=path):
                scope = self.scope(path, scripts=tests)
                self.assertEqual(("scripts/test_test_xml.py",), scope.script_tests)
                self.assertFalse(scope.broad or scope.repository)

    def test_a_script_test_edit_runs_only_its_proof(self) -> None:
        tests = frozenset({"scripts/test_ci_plan.py", "scripts/test_dev_test.py"})
        scope = self.scope("scripts/ci_plan.py", "scripts/test_dev_test.py", scripts=tests)
        self.assertEqual(("scripts/test_ci_plan.py", "scripts/test_dev_test.py"), scope.script_tests)
        self.assertFalse(scope.broad or scope.repository)


def successful_needs() -> dict[str, dict[str, object]]:
    plan_outputs = {family: "true" for family in ci_plan.FAMILIES}
    plan_outputs.update({"docs_only": "false", "fail_open": "false"})
    needs = {
        job: {"result": "success", "outputs": {}}
        for job in ci_plan.UNGATED_JOBS
        | set(ci_plan.GATED_JOBS)
        | ci_plan.BAZEL_TEST_JOBS
    }
    needs["plan"]["outputs"] = plan_outputs
    needs["workspace-tests"]["result"] = "skipped"
    return needs


def apply_event_deferrals(needs: dict, event: str) -> dict:
    """Give every event-deferred job the result `event` expects of it."""

    for job in ci_plan.DISPATCH_ONLY_JOBS:
        needs[job]["result"] = "skipped" if event in ci_plan.DEFERRED_EVENTS else "success"
    if event == "pull_request":
        needs["bazel-tests-tail"]["result"] = "skipped"
    return needs


class ConclusionTests(unittest.TestCase):
    def test_trusted_pr_requires_preflight_and_defers_full_tail_to_queue(self) -> None:
        needs = apply_event_deferrals(successful_needs(), "pull_request")
        self.assertEqual([], ci_plan.evaluate_conclusion(needs, "pull_request"))
        needs["bazel-tests-tail"]["result"] = "success"
        self.assertTrue(any(
            "bazel-tests-tail" in problem
            for problem in ci_plan.evaluate_conclusion(needs, "pull_request")
        ))
        needs["bazel-tests-tail"]["result"] = "skipped"
        needs["bazel-tests"]["result"] = "skipped"
        self.assertTrue(any(
            "bazel-tests" in problem
            for problem in ci_plan.evaluate_conclusion(needs, "pull_request")
        ))

    def test_bazel_job_succeeds_for_trusted_and_skips_only_when_untrusted(self) -> None:
        trusted = successful_needs()
        self.assertEqual([], ci_plan.evaluate_conclusion(trusted, bazel_is_trusted=True))

        untrusted = successful_needs()
        for job in ci_plan.BAZEL_TEST_JOBS:
            untrusted[job]["result"] = "skipped"
        untrusted["workspace-tests"]["result"] = "success"
        self.assertEqual(
            [], ci_plan.evaluate_conclusion(untrusted, bazel_is_trusted=False)
        )

        for trusted_event, result in ((True, "skipped"), (False, "success")):
            with self.subTest(trusted=trusted_event, result=result):
                needs = successful_needs()
                needs[ci_plan.BAZEL_TEST_JOB]["result"] = result
                problems = ci_plan.evaluate_conclusion(
                    needs, bazel_is_trusted=trusted_event
                )
                self.assertTrue(
                    any(ci_plan.BAZEL_TEST_JOB in problem for problem in problems)
                )

    def test_hygiene_jobs_are_required_for_every_event_and_docs_changes(self) -> None:
        workflow = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        for job in ("hygiene",):
            self.assertIn(job, ci_plan.UNGATED_JOBS)
            self.assertIn(job, workflow["ci-conclusion"]["needs"])
            self.assertNotIn("if", workflow[job])
            for event in ("pull_request", "merge_group", "workflow_dispatch"):
                for result in ("failure", "cancelled", "skipped"):
                    with self.subTest(job=job, event=event, result=result):
                        needs = successful_needs()
                        needs["plan"]["outputs"].update({"docs_only": "true", **{f: "false" for f in ci_plan.FAMILIES}})
                        for gated in ci_plan.GATED_JOBS:
                            needs[gated]["result"] = "skipped"
                        for bazel_job in ci_plan.BAZEL_TEST_JOBS:
                            needs[bazel_job]["result"] = "skipped"
                        needs[job]["result"] = result
                        problems = ci_plan.evaluate_conclusion(needs, event_name=event)
                        self.assertTrue(any(job in problem for problem in problems))

    def test_all_success_succeeds(self) -> None:
        self.assertEqual([], ci_plan.evaluate_conclusion(successful_needs()))

    def test_failure_and_cancellation_fail(self) -> None:
        for result in ("failure", "cancelled"):
            with self.subTest(result=result):
                needs = successful_needs()
                needs["workspace-tests"]["result"] = result
                self.assertTrue(ci_plan.evaluate_conclusion(needs))

    def test_planned_skip_succeeds(self) -> None:
        needs = successful_needs()
        needs["plan"]["outputs"].update({"docs_only": "true", **{family: "false" for family in ci_plan.FAMILIES}})
        for job in ci_plan.GATED_JOBS:
            needs[job]["result"] = "skipped"
        for job in ci_plan.BAZEL_TEST_JOBS:
            needs[job]["result"] = "skipped"
        self.assertEqual([], ci_plan.evaluate_conclusion(needs))

    def test_wrongly_skipped_job_fails(self) -> None:
        needs = successful_needs()
        needs["workspace-tests"]["result"] = "skipped"
        for job in ci_plan.BAZEL_TEST_JOBS:
            needs[job]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs, bazel_is_trusted=False)
        self.assertTrue(any("workspace-tests" in problem for problem in problems))

    def test_a_trusted_rust_event_expects_no_cargo_workspace_run(self) -> None:
        """The Bazel partition owns every deterministic Rust binary.

        On a trusted event Bazel owns the browser-projection case too, so a
        Cargo run is a policy violation.
        """
        needs = successful_needs()
        needs["plan"]["outputs"]["workbench"] = "false"
        needs["workspace-tests"]["result"] = "skipped"
        self.assertEqual([], ci_plan.evaluate_conclusion(needs))

        needs["workspace-tests"]["result"] = "success"
        self.assertTrue(
            any("workspace-tests" in problem for problem in ci_plan.evaluate_conclusion(needs))
        )

    def test_an_untrusted_rust_event_still_requires_the_cargo_workspace_run(self) -> None:
        needs = successful_needs()
        needs["plan"]["outputs"]["workbench"] = "false"
        needs["workspace-tests"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs, bazel_is_trusted=False)
        self.assertTrue(any("workspace-tests" in problem for problem in problems))

    def test_inconsistent_classifier_output_fails(self) -> None:
        needs = successful_needs()
        needs["plan"]["outputs"].update(
            {
                "docs_only": "false",
                "fail_open": "false",
                "rust": "false",
                "workbench": "false",
            }
        )
        needs["workspace-tests"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs)
        self.assertTrue(
            any("rust and plan.workbench are both false" in problem for problem in problems)
        )

    def test_skipped_ungated_job_fails(self) -> None:
        needs = successful_needs()
        needs["hygiene"]["result"] = "skipped"
        self.assertTrue(ci_plan.evaluate_conclusion(needs))

    def test_missing_needed_job_fails(self) -> None:
        needs = successful_needs()
        del needs["hygiene"]
        self.assertTrue(ci_plan.evaluate_conclusion(needs))


class ProducerConclusionTests(unittest.TestCase):
    def event_needs(self, event, enabled=True):
        needs = successful_needs()
        apply_event_deferrals(needs, event)
        if not enabled:
            for job in ci_plan.WORKERS_E2E_JOBS:
                needs[job]["result"] = "skipped"
        self.assertEqual([], self.evaluate(needs, event, enabled))
        return needs

    def evaluate(self, needs, event, enabled=True):
        return ci_plan.evaluate_conclusion(needs, event, enabled)

    def assert_producer_rejected(self, event, result):
        for producer in ("worker-artifacts",):
            with self.subTest(producer=producer):
                needs = self.event_needs(event)
                needs[producer]["result"] = result
                self.assertTrue(any(producer in p for p in self.evaluate(needs, event)))

    def test_dispatch_main_producer_failure_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "failure")

    def test_dispatch_main_producer_cancelled_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "cancelled")

    def test_dispatch_main_producer_skipped_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "skipped")

    def test_dispatch_producer_failure_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "failure")

    def test_dispatch_producer_cancelled_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "cancelled")

    def test_dispatch_producer_skipped_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "skipped")

    def test_labeled_pr_producer_failure_rejected(self):
        self.assert_producer_rejected("pull_request", "failure")

    def test_labeled_pr_producer_cancelled_rejected(self):
        self.assert_producer_rejected("pull_request", "cancelled")

    def test_labeled_pr_producer_skipped_rejected(self):
        self.assert_producer_rejected("pull_request", "skipped")

    def test_skipped_consumer_cascade_rejected(self):
        for event in ("workflow_dispatch", "pull_request"):
            consumers = [
                "restate-postgres-workers",
                "restate-postgres-workers-summary",
            ]
            for consumer in consumers:
                with self.subTest(event=event, consumer=consumer):
                    needs = self.event_needs(event)
                    needs[consumer]["result"] = "skipped"
                    self.assertTrue(any(consumer in p for p in self.evaluate(needs, event)))

    def test_process_operations_consumer_failure_cancelled_and_skipped_rejected(self):
        for event in ("workflow_dispatch",):
            for result in ("failure", "cancelled", "skipped"):
                with self.subTest(event=event, result=result):
                    needs = self.event_needs(event)
                    needs["functional-e2e-process-operations"]["result"] = result
                    self.assertTrue(any("functional-e2e-process-operations" in p
                                        for p in self.evaluate(needs, event)))

    def test_worker_producer_failure_cascading_to_process_operations_rejected(self):
        for event in ("workflow_dispatch",):
            needs = self.event_needs(event)
            needs["worker-artifacts"]["result"] = "failure"
            needs["functional-e2e-process-operations"]["result"] = "skipped"
            problems = self.evaluate(needs, event)
            for job in ("worker-artifacts", "functional-e2e-process-operations"):
                self.assertTrue(any(job in p for p in problems))

    def test_unlabeled_pr_worker_producer_skipped_accepted(self):
        needs = self.event_needs("pull_request", False)
        self.assertEqual("skipped", needs["worker-artifacts"]["result"])
        self.assertEqual([], self.evaluate(needs, "pull_request", False))

    def test_merge_group_worker_producer_and_segments_skipped_accepted(self):
        needs = self.event_needs("merge_group", False)
        for job in ("worker-artifacts", "restate-postgres-workers", "restate-postgres-workers-summary"):
            self.assertEqual("skipped", needs[job]["result"])
        self.assertEqual([], self.evaluate(needs, "merge_group", False))


POSTGRES_TEST_STEPS = {
    "Test PostgreSQL catalog compatibility",
    "Test Postgres store (conformance and attempt atomicity)",
    "Test runtime pool-wait binding",
    "Test runtime Postgres agent scenarios",
    "Test cross-backend store differential",
}


def selected_postgres_test_steps(event: str, role: str) -> set[str]:
    """Evaluate the small fixed condition vocabulary used by the PG matrix."""

    pr_class = event in {"pull_request", "merge_group"}
    selectors = {
        "matrix.role == 'compatibility'": role == "compatibility",
        "matrix.role == 'primary'": role == "primary",
        (
            "matrix.role == 'primary' && github.event_name != 'pull_request'"
            " && github.event_name != 'merge_group'"
        ): role == "primary" and not pr_class,
    }
    job = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"][
        "postgres-store"
    ]
    selected = set()
    for step in job["steps"]:
        if step.get("name") not in POSTGRES_TEST_STEPS:
            continue
        condition = step.get("if")
        if condition not in selectors:
            raise AssertionError(f"unmodelled PostgreSQL test condition: {condition!r}")
        if selectors[condition]:
            selected.add(step["name"])
    return selected


class PostgresMatrixTests(unittest.TestCase):
    COMPATIBILITY = {"Test PostgreSQL catalog compatibility"}
    PRIMARY_PR = {"Test runtime Postgres agent scenarios"}
    PRIMARY_TRUNK = {
        "Test Postgres store (conformance and attempt atomicity)",
        "Test runtime pool-wait binding",
        "Test runtime Postgres agent scenarios",
        "Test cross-backend store differential",
    }

    def test_matrix_comes_from_the_plan_and_brackets_the_supported_range(self) -> None:
        jobs = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        self.assertEqual(
            "${{ fromJSON(needs.plan.outputs.postgres_matrix) }}",
            jobs["postgres-store"]["strategy"]["matrix"]["include"],
        )
        self.assertEqual(
            "${{ steps.postgres-matrix.outputs.postgres_matrix }}",
            jobs["plan"]["outputs"]["postgres_matrix"],
        )
        step = next(
            candidate
            for candidate in jobs["plan"]["steps"]
            if candidate.get("id") == "postgres-matrix"
        )
        self.assertIn("scripts/ci_plan.py postgres-matrix", step["run"])

    def test_compatibility_lanes_are_deferred_off_the_pull_request_path(self) -> None:
        """PG16 runs on rust PRs/queues; PG14/PG18 run on schema diffs or dispatch."""
        self.assertEqual(
            [{"postgres": "16", "role": "primary"}],
            ci_plan.postgres_matrix("pull_request"),
        )
        self.assertEqual(
            [{"postgres": "16", "role": "primary"}],
            ci_plan.postgres_matrix("merge_group"),
        )
        self.assertEqual(
            [
                {"postgres": "14", "role": "compatibility"},
                {"postgres": "16", "role": "primary"},
                {"postgres": "18", "role": "compatibility"},
            ],
            ci_plan.postgres_matrix("workflow_dispatch"),
        )
        self.assertEqual(
            [
                {"postgres": "14", "role": "compatibility"},
                {"postgres": "16", "role": "primary"},
                {"postgres": "18", "role": "compatibility"},
            ],
            ci_plan.postgres_matrix("pull_request", schema=True),
        )

    def test_event_and_role_selection_runs_the_right_real_tests(self) -> None:
        for event in ("pull_request", "merge_group", "workflow_dispatch"):
            roles = {leg["role"] for leg in ci_plan.postgres_matrix(event)}
            with self.subTest(event=event, role="compatibility"):
                if event == "workflow_dispatch":
                    self.assertIn("compatibility", roles)
                    self.assertEqual(
                        self.COMPATIBILITY,
                        selected_postgres_test_steps(event, "compatibility"),
                    )
                else:
                    self.assertNotIn("compatibility", roles)
            with self.subTest(event=event, role="primary"):
                self.assertIn("primary", roles)
                expected = (
                    self.PRIMARY_PR
                    if event in ci_plan.DEFERRED_EVENTS
                    else self.PRIMARY_TRUNK
                )
                self.assertEqual(expected, selected_postgres_test_steps(event, "primary"))

    def test_commands_pin_live_catalog_version_and_runtime_identity_oracle(self) -> None:
        """The named oracles must survive the Bazel/Cargo dispatch, on both paths.

        The steps delegate to scripts/ci/store-tests.sh, so the pin follows the
        test names into that script's branch for the suite each step selects,
        and each name must appear on the Bazel side and the Cargo side.
        """
        job = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"][
            "postgres-store"
        ]
        steps = {step["name"]: step for step in job["steps"]}
        script = (ROOT / "scripts/ci/store-tests.sh").read_text(encoding="utf-8")

        def suite_body(step_name: str) -> tuple[str, str]:
            run = steps[step_name]["run"]
            self.assertIn("scripts/ci/store-tests.sh", run)
            suite = run.split()[-1]
            if f"\n  {suite})\n" in script:
                # A shaped suite writes both halves itself.
                body = script.split(f"\n  {suite})\n", 1)[1].split("\n    ;;", 1)[0]
                bazel, cargo = body.split("\n    else\n", 1)
                return bazel, cargo
            # A uniform suite states its selection once and renders it into
            # both dialects, so the row *is* both halves: a name present here
            # reaches the Bazel and the Cargo command by construction.
            row = re.search(
                rf'^\s*\[{re.escape(suite)}\]="([^"]*)"$', script, re.MULTILINE
            )
            self.assertIsNotNone(row, f"no store suite {suite}")
            return row.group(1), row.group(1)

        for oracle, step_name in (
            (
                "committed_shape_artifact_matches_the_ddl_artifact",
                "Test PostgreSQL catalog compatibility",
            ),
            (
                "a_mismatched_version_stamp_is_reported_without_a_column_diff",
                "Test PostgreSQL catalog compatibility",
            ),
            (
                "public_provider_parent_end_row_is_recovered_after_a_crash_before_the_ledger_write_on_postgres",
                "Test runtime Postgres agent scenarios",
            ),
            (
                "automatic_queued_retry_reuses_recorded_completion_before_new_arrivals",
                "Test runtime Postgres agent scenarios",
            ),
        ):
            with self.subTest(oracle=oracle):
                bazel, cargo = suite_body(step_name)
                self.assertIn(oracle, bazel)
                self.assertIn(oracle, cargo)

    def test_postgres_conclusion_fails_closed_for_every_supported_event(self) -> None:
        for event in ("pull_request", "merge_group", "workflow_dispatch"):
            for result in ("skipped", "failure", "cancelled"):
                with self.subTest(event=event, result=result):
                    needs = successful_needs()
                    apply_event_deferrals(needs, event)
                    needs["postgres-store"]["result"] = result
                    problems = ci_plan.evaluate_conclusion(needs, event_name=event)
                    self.assertTrue(
                        any("postgres-store" in problem for problem in problems)
                    )
            with self.subTest(event=event, result="missing"):
                needs = successful_needs()
                apply_event_deferrals(needs, event)
                del needs["postgres-store"]
                problems = ci_plan.evaluate_conclusion(needs, event_name=event)
                self.assertTrue(any("postgres-store" in problem for problem in problems))


class FuzzSmokeTests(unittest.TestCase):
    def test_fuzz_smoke_is_gated_rust_and_dispatch_only(self) -> None:
        self.assertEqual("rust", ci_plan.GATED_JOBS.get("fuzz-smoke"))
        self.assertIn("fuzz-smoke", ci_plan.DISPATCH_ONLY_JOBS)

    def test_fuzz_smoke_job_is_bounded_and_off_the_pr_critical_path(self) -> None:
        workflow = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        job = workflow["fuzz-smoke"]
        self.assertIn("timeout-minutes", job, "fuzz smoke must carry an explicit timeout")
        self.assertIn("github.event_name == 'workflow_dispatch'", job["if"])
        self.assertIn("fuzz-smoke", workflow["ci-conclusion"]["needs"])

    def test_fuzz_smoke_must_be_skipped_on_deferred_events(self) -> None:
        for event in ("pull_request", "merge_group"):
            needs = successful_needs()
            apply_event_deferrals(needs, event)
            needs["fuzz-smoke"]["result"] = "success"
            problems = ci_plan.evaluate_conclusion(needs, event_name=event)
            self.assertTrue(any("fuzz-smoke" in problem for problem in problems))
            needs["fuzz-smoke"]["result"] = "skipped"
            self.assertEqual([], ci_plan.evaluate_conclusion(needs, event_name=event))

    def test_fuzz_smoke_targets_match_manifest(self) -> None:
        import check_fuzz_corpus

        job = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]["fuzz-smoke"]
        run_scripts = "\n".join(
            step["run"] for step in job["steps"] if "run" in step
        )
        targets = check_fuzz_corpus.fuzz_targets(
            check_fuzz_corpus.FUZZ_MANIFEST.read_text(encoding="utf-8")
        )
        # The loop must source its targets from the manifest, not a literal
        # list: no declared target name may appear in any run step.
        self.assertIn("check_fuzz_corpus.py --list-targets", run_scripts)
        for target in targets:
            self.assertNotIn(target, run_scripts)
        uploads = [
            step
            for step in job["steps"]
            if "upload-artifact" in str(step.get("uses", ""))
        ]
        self.assertTrue(
            any(
                step.get("if") == "failure()"
                and "fuzz/artifacts" in step["with"]["path"]
                for step in uploads
            ),
            "fuzz-smoke must upload fuzz/artifacts/ when a target crashes",
        )

    def test_fuzz_paths_are_known_to_the_classifier(self) -> None:
        plan = ci_plan.classify(
            [
                ("M", "fuzz/fuzz_targets/remote_wire_dto.rs"),
                ("A", "fuzz/corpus/remote_wire_dto/seed-turn-input.json"),
            ]
        )
        self.assertEqual("false", plan["fail_open"])
        self.assertEqual("true", plan["rust"])
        self.assertEqual("false", plan["workbench"])

    def test_committed_seed_corpus_is_present_and_non_empty(self) -> None:
        import check_fuzz_corpus

        targets = check_fuzz_corpus.fuzz_targets(
            check_fuzz_corpus.FUZZ_MANIFEST.read_text(encoding="utf-8")
        )
        self.assertGreaterEqual(len(targets), 3)
        self.assertEqual(
            [], check_fuzz_corpus.corpus_problems(targets, check_fuzz_corpus.CORPUS_ROOT)
        )

    def test_missing_or_empty_seed_is_detected(self) -> None:
        import tempfile

        import check_fuzz_corpus

        with tempfile.TemporaryDirectory() as tmp:
            corpus_root = Path(tmp)
            problems = check_fuzz_corpus.corpus_problems(["absent"], corpus_root)
            self.assertTrue(any("missing corpus directory" in problem for problem in problems))
            (corpus_root / "hollow").mkdir()
            (corpus_root / "hollow" / "seed-empty").write_bytes(b"")
            problems = check_fuzz_corpus.corpus_problems(["hollow"], corpus_root)
            self.assertTrue(any("empty seed files" in problem for problem in problems))


class WorkbenchFeatureClosureTests(unittest.TestCase):
    """An optional dependency is in the closure only when a feature enables it.

    `lash-runtime` gates one optional first-party dependency per host-wired
    extension (ADR 0079). The workbench asks for `rlm` and nothing else, so a
    crate behind `s3` or `google` is not in the binary the Cargo partition
    builds, and a change to it cannot change what that job executes. These
    cases are built as a synthetic workspace because in the real manifests
    every `rlm`-gated crate is also a direct workbench dependency, so the
    repository alone cannot isolate the rule.
    """

    def closure(self, facade_manifest: str, workbench_features: str) -> set[str]:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            members = ("facade", "on", "off", "plain")
            (root / "Cargo.toml").write_text(
                "[workspace.dependencies]\n"
                + "".join(
                    f'{name} = {{ path = "crates/{name}" }}\n' for name in members
                ),
                encoding="utf-8",
            )
            for name in members:
                directory = root / "crates" / name
                directory.mkdir(parents=True)
                (directory / "Cargo.toml").write_text(
                    facade_manifest if name == "facade" else f'[package]\nname = "{name}"\n',
                    encoding="utf-8",
                )
            workbench = root / ci_plan.WORKBENCH_MANIFEST_DIR
            workbench.mkdir(parents=True)
            (workbench / "Cargo.toml").write_text(
                '[package]\nname = "agent-workbench"\n\n'
                "[dependencies]\n"
                + "facade = { workspace = true"
                + (f", {workbench_features}" if workbench_features else "")
                + " }\n",
                encoding="utf-8",
            )
            return set(ci_plan.workbench_dependency_dirs(str(root)))

    FACADE = """[package]
name = "facade"

[dependencies]
on = { workspace = true, optional = true }
off = { workspace = true, optional = true }
plain = { workspace = true }

[features]
default = []
enabled = ["dep:on"]
disabled = ["dep:off"]
"""

    def test_only_the_enabled_optional_dependency_is_in_the_closure(self) -> None:
        closure = self.closure(self.FACADE, 'features = ["enabled"]')
        self.assertIn("crates/facade", closure)
        self.assertIn("crates/on", closure)
        self.assertIn("crates/plain", closure, "a non-optional dependency is unconditional")
        self.assertNotIn("crates/off", closure)

    def test_a_feature_reached_through_another_feature_still_enables_it(self) -> None:
        manifest = self.FACADE.replace(
            'default = []', 'default = ["enabled"]'
        )
        self.assertIn("crates/on", self.closure(manifest, ""))
        self.assertNotIn(
            "crates/on",
            self.closure(manifest, "default-features = false"),
            "`default-features = false` withholds the default feature",
        )

    def test_an_optional_dependency_keeps_its_implicit_feature(self) -> None:
        """With no `dep:off` anywhere, `off` is itself the feature that enables it."""

        manifest = self.FACADE.replace('disabled = ["dep:off"]', 'disabled = []')
        self.assertIn("crates/off", self.closure(manifest, 'features = ["off"]'))
        self.assertNotIn("crates/off", self.closure(manifest, 'features = ["disabled"]'))

    def test_a_weak_forward_does_not_enable_the_dependency(self) -> None:
        """`x?/feat` configures `x` if something else turns it on; it never does."""

        manifest = self.FACADE.replace('disabled = ["dep:off"]', 'disabled = ["off?/any"]')
        self.assertNotIn("crates/off", self.closure(manifest, 'features = ["disabled"]'))
        self.assertIn(
            "crates/on",
            self.closure(
                manifest.replace('enabled = ["dep:on"]', 'enabled = ["on/any"]'),
                'features = ["enabled"]',
            ),
            "a strong `x/feat` forward does enable it",
        )


class WorkbenchClosureContractTests(unittest.TestCase):
    """The plan's dependency closure must agree with Cargo's own resolution.

    scripts/ci_plan.py reads the workspace manifests directly (the plan job has
    no Rust toolchain), so this is the gate that keeps the pure-Python walk and
    Cargo from drifting: a new dependency kind, a renamed package or a path
    dependency Cargo resolves differently fails here.
    """

    def cargo_workbench_closure(self) -> set[str]:
        """The closure read off Cargo's own resolution.

        This walks `resolve`, not the raw manifest dependency lists, because
        an optional dependency is only compiled when a feature enables it and
        the resolve graph is where Cargo records that decision.
        """

        metadata = json.loads(
            subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--locked"],
                cwd=ROOT, text=True, capture_output=True, check=True,
            ).stdout
        )
        workspace_root = Path(metadata["workspace_root"])

        def relative(path: str) -> str:
            return Path(path).relative_to(workspace_root).as_posix()

        directory = {
            package["id"]: relative(str(Path(package["manifest_path"]).parent))
            for package in metadata["packages"]
            if package.get("source") is None
        }
        first_party = set(directory)
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        root = next(
            identifier
            for identifier, path in directory.items()
            if path == ci_plan.WORKBENCH_MANIFEST_DIR
        )

        closure: set[str] = set()
        pending = [(root, True)]
        while pending:
            identifier, include_dev = pending.pop()
            if directory[identifier] in closure:
                continue
            closure.add(directory[identifier])
            for dependency in nodes[identifier]["deps"]:
                if dependency["pkg"] not in first_party:
                    continue
                kinds = {kind.get("kind") for kind in dependency.get("dep_kinds", [])}
                if kinds == {"dev"} and not include_dev:
                    continue
                pending.append((dependency["pkg"], False))
        return closure

    def test_the_plan_closure_matches_cargo_metadata(self) -> None:
        if shutil.which("cargo") is None:
            if os.environ.get("CI") == "true":
                self.fail("CI must run this contract with a Rust toolchain on PATH")
            self.skipTest("cargo is not on PATH")
        self.assertEqual(
            self.cargo_workbench_closure(), set(ci_plan.workbench_dependency_dirs())
        )


class WorkflowRegistrationTests(unittest.TestCase):
    def test_every_ci_job_is_registered_or_allowlisted(self) -> None:
        self.assertEqual(set(), unregistered_ci_jobs(CI_WORKFLOW.read_text(encoding="utf-8")))

    def test_only_process_operations_waits_for_worker_artifacts(self):
        jobs = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        other = jobs["functional-e2e"]
        consumer = jobs["functional-e2e-process-operations"]
        self.assertEqual("plan", other["needs"])
        self.assertEqual(["plan", "worker-artifacts"], consumer["needs"])
        self.assertEqual(other["if"], consumer["if"])
        self.assertEqual(["process-operations"],
                         [leg["name"] for leg in consumer["strategy"]["matrix"]["include"]])
        self.assertEqual({"agent-service", "agent-workbench", "effect-group-conformance",
                          "workflow-graph-roundtrip", "version-bump-recreation",
                          "session-lease-triage", "slack-clone-full-host"},
                         {leg["name"] for leg in other["strategy"]["matrix"]["include"]})
        self.assertFalse(any("worker binaries" in step.get("name", "") for step in other["steps"]))
        self.assertTrue(any(step.get("name") == "Download worker binaries" for step in consumer["steps"]))

    def test_the_cargo_partition_is_gated_on_the_plan_and_runs_on_trunk(self) -> None:
        workflow = yaml.safe_load(CI_WORKFLOW.read_text())
        job = workflow["jobs"]["workspace-tests"]
        self.assertNotIn("github.event_name", job["if"])
        self.assertIn("needs.plan.outputs.bazel_trusted != 'true'", job["if"])
        self.assertIn("needs.plan.outputs.rust == 'true'", job["if"])
        classify = next(
            step for step in workflow["jobs"]["plan"]["steps"]
            if step.get("id") == "classify"
        )
        self.assertIn('--event "${GITHUB_EVENT_NAME}"', classify["run"])

    def test_partial_removal_is_invisible_to_the_runtime_aggregator(self) -> None:
        """The shape `unregistered_ci_jobs` is the sole guard against.

        `lint` is dropped from `ci-conclusion`'s `needs:` and from the
        aggregator's static job map -- the two halves a partial removal
        touches -- while the job itself stays defined in `ci.yml` and keeps
        running. `evaluate_conclusion` then sees a consistent map and reports
        nothing even though `lint` failed; only the workflow-derived guard
        notices that the job is still there. FIG-2824.
        """
        needs = successful_needs()
        needs["lint"]["result"] = "failure"
        self.assertTrue(any("lint" in problem for problem in ci_plan.evaluate_conclusion(needs)))

        del needs["lint"]
        with mock.patch.object(ci_plan, "UNGATED_JOBS", ci_plan.UNGATED_JOBS - {"lint"}):
            self.assertEqual([], ci_plan.evaluate_conclusion(needs))

        workflow = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))
        workflow["jobs"]["ci-conclusion"]["needs"] = [
            job for job in workflow["jobs"]["ci-conclusion"]["needs"] if job != "lint"
        ]
        dropped = yaml.safe_dump(workflow)
        self.assertEqual({"lint"}, unregistered_ci_jobs(dropped))
        # And an appended exemption silences it, with nothing behind it. That is
        # why the membership below is pinned rather than merely commented.
        self.assertEqual(
            set(),
            unregistered_ci_jobs(dropped, {**AGGREGATOR_ALLOWLIST, "lint": "silenced"}),
        )

    def test_rogue_job_is_caught(self) -> None:
        workflow_copy = CI_WORKFLOW.read_text(encoding="utf-8").rstrip()
        workflow_copy += "\n\n  rogue-job:\n    runs-on: ubuntu-latest\n"
        self.assertEqual({"rogue-job"}, unregistered_ci_jobs(workflow_copy))


class AggregatorAllowlistTests(unittest.TestCase):
    """FIG-2824. The exemption list is the escape hatch on the guard that covers
    the one partial-removal shape `CI conclusion` cannot see, so it is pinned
    exactly and every entry is justified.

    The expected membership is written out by hand rather than read from
    ``AGGREGATOR_ALLOWLIST``: a test that derives its expectation from the set
    under test still passes after someone appends to that set.
    """

    def test_the_allowlist_is_exactly_the_aggregator_job(self) -> None:
        self.assertEqual({"ci-conclusion"}, set(AGGREGATOR_ALLOWLIST))
        for job, reason in AGGREGATOR_ALLOWLIST.items():
            with self.subTest(job=job):
                self.assertTrue(reason.strip(), f"{job} needs a stated reason")

    def test_no_entry_exempts_a_job_the_aggregator_already_needs(self) -> None:
        """An exemption for a registered job is dead weight that hides the very
        shape it is listed for: drop that job from `needs:` later and the guard
        stays quiet. `plan` sat here in exactly that state until FIG-2824."""
        jobs = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        registered = set(jobs["ci-conclusion"]["needs"])
        self.assertIn("plan", registered)
        for job in AGGREGATOR_ALLOWLIST:
            with self.subTest(job=job):
                self.assertIn(job, jobs, f"{job} is allowlisted but not defined in ci.yml")
                self.assertNotIn(
                    job,
                    registered,
                    f"{job} is in ci-conclusion's needs; delete its exemption",
                )

    def test_the_aggregator_cannot_register_itself(self) -> None:
        """The single entry's stated reason, asserted rather than trusted."""
        jobs = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        self.assertNotIn("ci-conclusion", jobs["ci-conclusion"]["needs"])


class DispatchOnlyJobTests(unittest.TestCase):
    """Deferred compile lanes live on workflow_dispatch alone.

    The queue now runs the same minimal board as a pull request, so
    `lashlang-git-consumer`, `feature-lanes` and `unicode-tests` joined the
    other deferred jobs. The job IDs and conditions are spelled out by hand
    here rather than derived from ``ci_plan.DISPATCH_ONLY_JOBS``: a test that
    reads its expectation out of the set under test still passes after someone
    empties the set.
    """

    JOBS = ("lashlang-git-consumer", "feature-lanes", "unicode-tests")

    def board(self, event: str, docs_only: bool = False) -> dict:
        needs = successful_needs()
        if docs_only:
            needs["plan"]["outputs"].update(
                {"docs_only": "true", **{family: "false" for family in ci_plan.FAMILIES}}
            )
            for job in ci_plan.GATED_JOBS:
                needs[job]["result"] = "skipped"
            if event in ci_plan.DEFERRED_EVENTS:
                needs["postgres-store"]["result"] = "skipped"
            needs[ci_plan.BAZEL_TEST_JOB]["result"] = "skipped"
        apply_event_deferrals(needs, event)
        return needs

    def test_the_lanes_are_registered_as_dispatch_only(self) -> None:
        aggregator_needs = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]["ci-conclusion"]["needs"]
        for job in self.JOBS:
            with self.subTest(job=job):
                self.assertIn(job, ci_plan.DISPATCH_ONLY_JOBS)
                self.assertIn(job, aggregator_needs)

    def test_workflow_conditions_keep_them_off_pull_requests_and_merge_groups(self) -> None:
        jobs = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        for job in self.JOBS:
            with self.subTest(job=job):
                self.assertIn("github.event_name == 'workflow_dispatch'", jobs[job]["if"])

    def test_merge_group_accepts_them_skipped(self) -> None:
        self.assertEqual([], ci_plan.evaluate_conclusion(self.board("merge_group"), "merge_group"))
        for job in self.JOBS:
            self.assertEqual("skipped", self.board("merge_group")[job]["result"])
            for result in ("success", "failure", "cancelled"):
                with self.subTest(job=job, result=result):
                    needs = self.board("merge_group")
                    needs[job]["result"] = result
                    problems = ci_plan.evaluate_conclusion(needs, "merge_group")
                    self.assertIn(
                        f"dispatch-only job {job} ended with {result!r} on a"
                        " merge_group event, expected skipped",
                        problems,
                    )

    def test_workflow_dispatch_requires_them(self) -> None:
        self.assertEqual(
            [], ci_plan.evaluate_conclusion(self.board("workflow_dispatch"), "workflow_dispatch")
        )
        for job in self.JOBS:
            wrongly_skipped = self.board("workflow_dispatch")
            wrongly_skipped[job]["result"] = "skipped"
            self.assertTrue(
                any(job in problem for problem in
                    ci_plan.evaluate_conclusion(wrongly_skipped, "workflow_dispatch"))
            )

    def test_the_other_deferred_jobs_are_untouched(self) -> None:
        still_deferred = (
            "heavy-tests",
            "stack-budget",
            "s3-store",
            "functional-e2e",
            "functional-e2e-process-operations",
            "fuzz-smoke",
        )
        for job in still_deferred:
            with self.subTest(job=job):
                self.assertIn(job, ci_plan.DISPATCH_ONLY_JOBS)


class FacadeAndToolingGatingTests(unittest.TestCase):
    """Seal and repo-gates ride their own path-derived families."""

    def board(self, event: str = "pull_request", **families: str) -> dict:
        needs = successful_needs()
        needs["plan"]["outputs"].update(
            {"docs_only": "false", "fail_open": "false"}
        )
        needs["plan"]["outputs"].update(
            {family: "false" for family in ci_plan.FAMILIES}
        )
        needs["plan"]["outputs"]["rust"] = "true"
        needs["plan"]["outputs"].update(families)
        needs["postgres-store"]["result"] = "skipped"
        needs["workspace-tests"]["result"] = "skipped"
        apply_event_deferrals(needs, event)
        if event == "workflow_dispatch":
            needs["unicode-tests"]["result"] = (
                "success"
                if needs["plan"]["outputs"].get("regress") == "true"
                else "skipped"
            )
        return needs

    def test_core_only_diff_accepts_seal_postgres_and_repo_gates_skipped(self) -> None:
        needs = self.board()
        for job in ("check", "postgres-store", "repo-gates"):
            needs[job]["result"] = "skipped"
        self.assertEqual(
            [], ci_plan.evaluate_conclusion(needs, "pull_request")
        )
        # Running them anyway is also accepted.
        for job in ("check", "postgres-store", "repo-gates"):
            trial = self.board()
            problems = ci_plan.evaluate_conclusion(trial, "pull_request")
            self.assertFalse(
                any(job in problem for problem in problems),
                f"{job}: {problems}",
            )

    def test_facade_diff_requires_the_seal_lane(self) -> None:
        needs = self.board(facade="true")
        needs["check"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs, "pull_request")
        self.assertTrue(any("check" in problem for problem in problems))
        needs["check"]["result"] = "success"
        self.assertEqual([], ci_plan.evaluate_conclusion(needs, "pull_request"))

    def test_dispatch_always_requires_the_seal_lane(self) -> None:
        needs = self.board("workflow_dispatch")
        needs["check"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs, "workflow_dispatch")
        self.assertTrue(any("check" in problem for problem in problems))

    def test_tooling_diff_requires_repo_gates(self) -> None:
        needs = self.board(tooling="true")
        needs["repo-gates"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs, "pull_request")
        self.assertTrue(any("repo-gates" in problem for problem in problems))
        needs["repo-gates"]["result"] = "success"
        self.assertEqual([], ci_plan.evaluate_conclusion(needs, "pull_request"))

    def test_stores_diff_requires_postgres_store(self) -> None:
        needs = self.board(stores="true")
        # With stores on, a skipped postgres-store is rejected.
        self.assertEqual("skipped", needs["postgres-store"]["result"])
        problems = ci_plan.evaluate_conclusion(needs, "pull_request")
        self.assertTrue(any("postgres-store" in problem for problem in problems))
        needs["postgres-store"]["result"] = "success"
        self.assertEqual([], ci_plan.evaluate_conclusion(needs, "pull_request"))


if __name__ == "__main__":
    unittest.main()
