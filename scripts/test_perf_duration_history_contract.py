#!/usr/bin/env python3
"""Behavior tests for the release-profile duration-trend history wiring.

The advisory drift signal (FIG-1532) is only worth anything if the history it
reads survives the run that wrote it. That makes the wiring -- a restored and
saved cache series, a history path inside it, and an upload that carries the
ledger out -- part of the contract rather than workflow decoration, and it is
the half that has already been lost once: #1370 pruned the quick perf smoke and
took the only caller of the trend with it, leaving the mechanism in the tree
with nothing feeding it.

Every assertion below is proven by a mutation that makes it fail, so a test
that stops discriminating is visible rather than quietly green.
"""

from __future__ import annotations

import copy
import pathlib
import re
import unittest

import yaml


ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "perf.yml"
PROFILE_RUNTIME = ROOT / "scripts" / "profile_runtime.py"

JOB = "perf-guard-full"
RESTORE_STEP = "Restore runtime perf duration history"
SAVE_STEP = "Save runtime perf duration history"
RUNTIME_STEP = "Run full runtime performance checks"
UPLOAD_STEP = "Upload performance guard artifacts"

# The series prefix carries the run shape. Records are keyed by profile because
# durations are only comparable within one benchmark geometry, so a key that
# did not say `full` could be restored into a quick run and back again.
SERIES_PREFIX = "perf-duration-history-full-"
# The report directory holds a ~70 MB runtime.json per run; the history must
# not share it or every run would push that through the cache service.
REPORT_DIR = ".benchmarks/perf-guard"


def step(job: dict[str, object], name: str) -> dict[str, object] | None:
    return next((item for item in job["steps"] if item.get("name") == name), None)


def step_index(job: dict[str, object], name: str) -> int:
    return next(
        (index for index, item in enumerate(job["steps"]) if item.get("name") == name),
        -1,
    )


def upload_paths(upload: dict[str, object]) -> set[str]:
    raw = str(upload.get("with", {}).get("path", ""))
    return {line.strip() for line in raw.splitlines() if line.strip()}


def workflow_contract_failures(workflow: dict[str, object]) -> list[str]:
    failures: list[str] = []
    job = workflow["jobs"][JOB]

    restore = step(job, RESTORE_STEP)
    save = step(job, SAVE_STEP)
    runtime = step(job, RUNTIME_STEP)
    upload = step(job, UPLOAD_STEP)
    if restore is None or save is None or runtime is None or upload is None:
        return ["the perf guard job lacks the restore, save, runtime or upload step"]

    restore_with = restore.get("with", {})
    save_with = save.get("with", {})
    history_dir = str(restore_with.get("path", ""))

    if history_dir != str(save_with.get("path", "")):
        failures.append("the restored and saved history directories differ")
    if not history_dir or history_dir.startswith(REPORT_DIR):
        failures.append(
            f"the history directory must exist and stay outside {REPORT_DIR}"
        )

    for label, with_block in (("restore", restore_with), ("save", save_with)):
        if not str(with_block.get("key", "")).startswith(SERIES_PREFIX):
            failures.append(f"the {label} key is not in the {SERIES_PREFIX} series")
    restore_fallbacks = {
        line.strip()
        for line in str(restore_with.get("restore-keys", "")).splitlines()
        if line.strip()
    }
    if restore_fallbacks != {SERIES_PREFIX}:
        failures.append(
            "the restore must fall back to the full series prefix and nothing else"
        )
    # A rerun reuses `run_id`, so a key without the attempt collides with the
    # entry its own first attempt saved.
    if "github.run_attempt" not in str(save_with.get("key", "")):
        failures.append("the save key does not separate run attempts")

    # Reading is for every dispatch -- a run on a branch is compared against
    # main's trailing median rather than against nothing. Writing is trunk's
    # alone, or a branch would extend the durable series.
    if restore.get("if") is not None:
        failures.append("the restore is gated, so a branch dispatch reads no baseline")
    if "refs/heads/main" not in str(save.get("if", "")):
        failures.append("the save is not restricted to main")

    # FIG-1385 stands: neither cache step may turn a cache-service outage into
    # a red guard.
    for label, item in (("restore", restore), ("save", save)):
        if item.get("continue-on-error") is not True:
            failures.append(f"the {label} step can fail the run")

    run = str(runtime.get("run", ""))
    if "--profile full" not in run:
        failures.append("the measured profile is not the full release profile")
    history_flag = re.search(r"--duration-history\s+(\S+)", run)
    if history_flag is None:
        failures.append("the runtime step records no duration history")
    elif not history_flag.group(1).startswith(f"{history_dir}/"):
        failures.append(
            "the recorded history path is not inside the cached history directory"
        )

    if step_index(job, SAVE_STEP) < step_index(job, RUNTIME_STEP):
        failures.append("the history is saved before the run that appends to it")

    if history_dir and f"{history_dir}/*" not in upload_paths(upload):
        failures.append("the uploaded artifact does not carry the history ledger")

    return failures


def enforcement_flag_failures(source: str) -> list[str]:
    """The trend must have no enforcing switch to reach for.

    FIG-1385 demoted wall clock to advisory. The drift signal is advisory *by
    construction* -- `report_drift` returns nothing -- and the CLI in front of
    it must not grow an option that re-litigates that, so the enforcement
    surface is pinned by name rather than by intent.
    """
    declared = set(re.findall(r'"(--enforce[a-z-]*)"', source))
    expected = {"--enforce-budgets", "--enforce-inventory"}
    if declared != expected:
        return [
            "profile_runtime.py enforcement flags changed: "
            f"{sorted(declared)} instead of {sorted(expected)}"
        ]
    return []


class PerfDurationHistoryWorkflowTest(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))

    def test_production_wiring_persists_and_publishes_the_full_series(self) -> None:
        self.assertEqual([], workflow_contract_failures(self.workflow))

    def test_the_runtime_cli_exposes_no_duration_enforcement_switch(self) -> None:
        self.assertEqual(
            [], enforcement_flag_failures(PROFILE_RUNTIME.read_text(encoding="utf-8"))
        )

    def test_a_duration_enforcement_switch_is_rejected(self) -> None:
        source = PROFILE_RUNTIME.read_text(encoding="utf-8").replace(
            '"--enforce-inventory"', '"--enforce-durations"'
        )
        self.assertNotEqual([], enforcement_flag_failures(source))

    def test_contract_rejects_every_way_the_series_stops_persisting(self) -> None:
        mutations: list[tuple[str, dict[str, object]]] = []

        def mutate(name: str):
            copied = copy.deepcopy(self.workflow)
            mutations.append((name, copied))
            return copied["jobs"][JOB]

        job = mutate("history written outside the cached directory")
        step(job, RUNTIME_STEP)["run"] = str(
            step(job, RUNTIME_STEP)["run"]
        ).replace(".benchmarks/perf-history/", ".benchmarks/perf-guard/")

        job = mutate("history recording dropped entirely")
        step(job, RUNTIME_STEP)["run"] = re.sub(
            r"\s*\\\n\s*--duration-history \S+",
            "",
            str(step(job, RUNTIME_STEP)["run"]),
        )

        job = mutate("no save, so the series never grows")
        job["steps"] = [item for item in job["steps"] if item.get("name") != SAVE_STEP]

        job = mutate("save moved before the run that appends")
        steps = job["steps"]
        steps.insert(0, steps.pop(step_index(job, SAVE_STEP)))

        job = mutate("quick and full series share one key")
        step(job, RESTORE_STEP)["with"]["key"] = "perf-duration-history-quick-1"

        job = mutate("restore falls back to any series")
        step(job, RESTORE_STEP)["with"]["restore-keys"] = "perf-duration-history-\n"

        job = mutate("save key drops the attempt, so a rerun collides")
        step(job, SAVE_STEP)["with"]["key"] = f"{SERIES_PREFIX}" + "${{ github.run_id }}"

        job = mutate("a branch dispatch extends the durable series")
        step(job, SAVE_STEP)["if"] = "!cancelled()"

        job = mutate("restore gated, so a branch compares against nothing")
        step(job, RESTORE_STEP)["if"] = "github.ref == 'refs/heads/main'"

        job = mutate("a cache outage fails the guard")
        step(job, SAVE_STEP)["continue-on-error"] = False

        job = mutate("the report directory is pushed through the cache")
        step(job, RESTORE_STEP)["with"]["path"] = f"{REPORT_DIR}/history"
        step(job, SAVE_STEP)["with"]["path"] = f"{REPORT_DIR}/history"

        job = mutate("the ledger never leaves the runner")
        step(job, UPLOAD_STEP)["with"]["path"] = f"{REPORT_DIR}/*\n"

        job = mutate("the trend is taken from the quick profile")
        step(job, RUNTIME_STEP)["run"] = str(
            step(job, RUNTIME_STEP)["run"]
        ).replace("--profile full", "--profile quick")

        for name, mutation in mutations:
            with self.subTest(name=name):
                self.assertNotEqual([], workflow_contract_failures(mutation))


if __name__ == "__main__":
    unittest.main()
