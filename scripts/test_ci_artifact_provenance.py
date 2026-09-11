#!/usr/bin/env python3
"""Behavior tests for exact same-run CI artifact provenance."""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import pathlib
import sys
import tarfile
import tempfile
import unittest

import yaml


ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location(
    "artifact_provenance", ROOT / "scripts" / "ci" / "artifact_provenance.py"
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


def named_step(job: dict[str, object], name: str) -> dict[str, object]:
    return next(step for step in job["steps"] if step.get("name") == name)


def workflow_contract_failures(workflow: dict[str, object]) -> list[str]:
    failures: list[str] = []
    if workflow.get("permissions") != {"contents": "read"}:
        failures.append("workflow permissions must remain exactly contents: read")

    jobs = workflow["jobs"]
    producer = jobs["worker-artifacts"]
    expected_producer_outputs = {
        "artifact_id": "${{ steps.upload-worker-binaries.outputs.artifact-id }}",
        "artifact_name": "${{ steps.prepare-worker-binaries.outputs.artifact_name }}",
        "producer_attempt": "${{ steps.prepare-worker-binaries.outputs.producer_attempt }}",
    }
    if producer.get("outputs") != expected_producer_outputs:
        failures.append("worker producer outputs do not bind upload ID and producer attempt")
    upload = named_step(producer, "Upload worker binaries")
    upload_paths = set(str(upload["with"].get("path", "")).splitlines())
    if upload.get("id") != "upload-worker-binaries" or upload_paths != {
        "${{ runner.temp }}/worker-binaries.tar",
        "${{ runner.temp }}/worker-binaries.provenance.json",
    }:
        failures.append("worker upload does not publish the payload and provenance together")

    selected_worker_id = "${{ needs.worker-artifacts.outputs.artifact_id }}"
    for job_id in ("functional-e2e-process-operations", "restate-postgres-workers"):
        job = jobs[job_id]
        steps = job["steps"]
        try:
            guard_index = next(
                index
                for index, step in enumerate(steps)
                if step.get("name") == "Require exact worker artifact selection"
            )
            download_index = next(
                index
                for index, step in enumerate(steps)
                if step.get("name") == "Download worker binaries"
            )
        except StopIteration:
            failures.append(f"{job_id} lacks worker selection guard or download")
            continue
        download = steps[download_index]
        if guard_index >= download_index:
            failures.append(f"{job_id} does not refuse blank selection before download")
        if download.get("with") != {
            "artifact-ids": selected_worker_id,
            "path": "${{ runner.temp }}/worker-download",
        }:
            failures.append(f"{job_id} does not download only the exact producer artifact ID")
        consume = named_step(job, "Verify and extract worker binaries").get("run", "")
        for argument in (
            "--require-payload worker-binaries.tar",
            '--producer-attempt "${{ needs.worker-artifacts.outputs.producer_attempt }}"',
            '--selected-artifact-id "${{ needs.worker-artifacts.outputs.artifact_id }}"',
            '--current-attempt "${GITHUB_RUN_ATTEMPT}"',
        ):
            if argument not in consume:
                failures.append(f"{job_id} worker verification omits {argument}")

    workers = jobs["restate-postgres-workers"]
    expected_segment_outputs = {
        f"segment_{segment}_{field}": (
            f"${{{{ steps.upload-segment-{segment}.outputs.artifact-id }}}}"
            if field == "artifact_id"
            else f"${{{{ steps.prepare-segment-{segment}.outputs.{field} }}}}"
        )
        for segment in (1, 2)
        for field in ("artifact_id", "artifact_name", "producer_attempt")
    }
    if workers.get("outputs") != expected_segment_outputs:
        failures.append("matrix segment outputs do not retain each exact ID/name/attempt")
    for segment in (1, 2):
        prepare = named_step(workers, f"Record segment {segment} result provenance")
        prepare_run = prepare.get("run", "")
        if "--payload worker-binaries-consumption.json" not in prepare_run:
            failures.append(f"segment {segment} omits the consumed binary receipt")

    summary = jobs["restate-postgres-workers-summary"]
    summary_steps = summary["steps"]
    guard_index = next(
        (
            index
            for index, step in enumerate(summary_steps)
            if step.get("name") == "Require exact segment result selections"
        ),
        -1,
    )
    for segment in (1, 2):
        name = f"Download segment {segment} completed workflow manifest"
        try:
            download_index = next(
                index
                for index, step in enumerate(summary_steps)
                if step.get("name") == name
            )
        except StopIteration:
            failures.append(f"summary lacks exact segment {segment} download")
            continue
        download = summary_steps[download_index]
        if guard_index < 0 or guard_index >= download_index:
            failures.append(
                f"summary does not refuse blank segment {segment} ID before download"
            )
        expected = {
            "artifact-ids": (
                "${{ needs.restate-postgres-workers.outputs."
                f"segment_{segment}_artifact_id }}}}"
            ),
            "path": f"target/restate-postgres-workers-e2e-download/segment-{segment}",
        }
        if download.get("with") != expected:
            failures.append(f"summary segment {segment} download is not exact-ID scoped")
    summary_verify = named_step(
        summary, "Verify completed workflow manifest provenance"
    ).get("run", "")
    for payload in (
        "workflow-inventory.tsv",
        "completed-workflows.txt",
        "worker-binaries-consumption.json",
    ):
        if f"--require-payload {payload}" not in summary_verify:
            failures.append(f"summary does not require verified payload {payload}")
    return failures


def source(**overrides: str):
    values = {
        "repository": "Ascending-AI/lash",
        "run_id": "345",
        "head_sha": "a" * 40,
        "ref": "refs/heads/main",
        "event_name": "push",
        "workflow_ref": "Ascending-AI/lash/.github/workflows/ci.yml@refs/heads/main",
        "actor_id": "1234",
    }
    values.update(overrides)
    return MODULE.SourceContext(**values)


class ArtifactProvenanceTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name)
        (self.root / "payload.tar").write_bytes(b"trusted payload")
        self.manifest = self.root / "provenance.json"
        MODULE.create_manifest(
            manifest_path=self.manifest,
            payload_root=self.root,
            payloads=["payload.tar"],
            artifact_name="worker-binaries-a-1",
            producer_job="worker-artifacts",
            producer_instance="worker-binaries",
            producer_attempt="1",
            source=source(),
        )

    def tearDown(self) -> None:
        self.temp.cleanup()

    def validate(self, **overrides: object) -> dict[str, object]:
        arguments = {
            "manifest_path": self.manifest,
            "payload_root": self.root,
            "expected_artifact_name": "worker-binaries-a-1",
            "expected_producer_job": "worker-artifacts",
            "expected_producer_instance": "worker-binaries",
            "expected_producer_attempt": "1",
            "selected_artifact_id": "987",
            "current_attempt": "2",
            "required_payloads": ["payload.tar"],
            "source": source(),
        }
        arguments.update(overrides)
        return MODULE.validate_manifest(**arguments)

    def test_same_attempt_and_retained_producer_are_attributed(self) -> None:
        same = self.validate(current_attempt="1")
        retained = self.validate(current_attempt="2")
        self.assertEqual("1", same["artifact"]["producer_attempt"])
        self.assertEqual("1", same["consumer_attempt"])
        self.assertEqual("1", retained["artifact"]["producer_attempt"])
        self.assertEqual("2", retained["consumer_attempt"])
        self.assertEqual("987", retained["selected_artifact_id"])

    def test_wrong_run_head_and_trust_context_refuse(self) -> None:
        for label, changed_source in (
            ("run", source(run_id="346")),
            ("head", source(head_sha="b" * 40)),
            ("repository", source(repository="attacker/lash")),
            ("ref", source(ref="refs/pull/12/merge")),
            ("event", source(event_name="pull_request")),
            ("workflow", source(workflow_ref="attacker/workflow.yml@refs/heads/main")),
            ("actor", source(actor_id="9999")),
        ):
            with self.subTest(label=label), self.assertRaisesRegex(
                MODULE.ProvenanceError, "source/trust mismatch"
            ):
                self.validate(source=changed_source)

    def test_wrong_producer_or_attempt_refuses(self) -> None:
        cases = (
            {"expected_producer_job": "other-job"},
            {"expected_producer_instance": "segment-1"},
            {"expected_producer_attempt": "2"},
            {"expected_artifact_name": "worker-binaries-a-2"},
        )
        for changed in cases:
            with self.subTest(changed=changed), self.assertRaisesRegex(
                MODULE.ProvenanceError, "artifact provenance mismatch"
            ):
                self.validate(**changed)

        with self.assertRaisesRegex(MODULE.ProvenanceError, "cannot be newer"):
            self.validate(expected_producer_attempt="2", current_attempt="1")

    def test_missing_manifest_and_invalid_selected_id_refuse(self) -> None:
        self.manifest.unlink()
        with self.assertRaisesRegex(MODULE.ProvenanceError, "manifest is missing"):
            self.validate()
        self.assertRaisesRegex(
            MODULE.ProvenanceError,
            "selected artifact ID",
            self.validate,
            selected_artifact_id="",
        )

    def test_payload_mutation_and_missing_payload_refuse(self) -> None:
        (self.root / "payload.tar").write_bytes(b"mutated")
        with self.assertRaisesRegex(MODULE.ProvenanceError, "(size|digest) mismatch"):
            self.validate()
        (self.root / "payload.tar").unlink()
        with self.assertRaisesRegex(MODULE.ProvenanceError, "payload is missing"):
            self.validate()

    def test_required_payload_set_refuses_omission_or_unexpected_files(self) -> None:
        with self.assertRaisesRegex(MODULE.ProvenanceError, "payload set mismatch"):
            self.validate(required_payloads=["payload.tar", "receipt.json"])

    def test_selection_refuses_blank_fallback_and_future_or_wrong_name(self) -> None:
        valid = {
            "artifact_name": "worker-binaries-a-1",
            "expected_artifact_name": "worker-binaries-a-1",
            "selected_artifact_id": "987",
            "producer_attempt": "1",
            "current_attempt": "2",
        }
        MODULE.validate_selection(**valid)
        for changed, diagnostic in (
            ({"selected_artifact_id": ""}, "selected artifact ID"),
            ({"producer_attempt": "3"}, "cannot be newer"),
            ({"artifact_name": "worker-binaries-a-2"}, "name mismatch"),
        ):
            arguments = valid | changed
            with self.subTest(changed=changed), self.assertRaisesRegex(
                MODULE.ProvenanceError, diagnostic
            ):
                MODULE.validate_selection(**arguments)

    def test_manifest_rejects_extra_fields_and_path_escape(self) -> None:
        parsed = json.loads(self.manifest.read_text(encoding="utf-8"))
        parsed["trusted"] = True
        self.manifest.write_text(json.dumps(parsed), encoding="utf-8")
        with self.assertRaisesRegex(MODULE.ProvenanceError, "keys mismatch"):
            self.validate()

        with self.assertRaisesRegex(MODULE.ProvenanceError, "unsafe payload path"):
            MODULE.create_manifest(
                manifest_path=self.manifest,
                payload_root=self.root,
                payloads=["../payload.tar"],
                artifact_name="escape",
                producer_job="worker-artifacts",
                producer_instance="worker-binaries",
                producer_attempt="1",
                source=source(),
            )

    def _tar(self, members: list[tuple[tarfile.TarInfo, bytes]]) -> pathlib.Path:
        archive = self.root / "archive.tar"
        with tarfile.open(archive, "w") as output:
            for info, payload in members:
                output.addfile(info, io.BytesIO(payload))
        return archive

    def test_safe_tar_extraction_preserves_regular_executables(self) -> None:
        root = tarfile.TarInfo("./")
        root.type = tarfile.DIRTYPE
        info = tarfile.TarInfo("./bin/worker")
        info.size = 6
        info.mode = 0o755
        archive = self._tar([(root, b""), (info, b"worker")])
        destination = self.root / "extracted"
        MODULE.safe_extract_tar(archive, destination)
        worker = destination / "bin" / "worker"
        self.assertEqual(b"worker", worker.read_bytes())
        self.assertEqual(0o755, worker.stat().st_mode & 0o777)

    def test_safe_tar_extraction_rejects_traversal_and_links(self) -> None:
        traversal = tarfile.TarInfo("../escape")
        traversal.size = 1
        archive = self._tar([(traversal, b"x")])
        with self.assertRaisesRegex(MODULE.ProvenanceError, "unsafe"):
            MODULE.safe_extract_tar(archive, self.root / "out-traversal")

        link = tarfile.TarInfo("link")
        link.type = tarfile.SYMTYPE
        link.linkname = "/etc/passwd"
        archive = self._tar([(link, b"")])
        with self.assertRaisesRegex(MODULE.ProvenanceError, "not a regular file"):
            MODULE.safe_extract_tar(archive, self.root / "out-link")

    def test_receipt_summary_records_immutable_id_and_both_attempts(self) -> None:
        receipt = self.validate()
        receipt_path = self.root / "receipt.json"
        summary_path = self.root / "summary.md"
        MODULE.write_receipt(receipt, receipt_path, summary_path)
        persisted = json.loads(receipt_path.read_text(encoding="utf-8"))
        summary = summary_path.read_text(encoding="utf-8")
        self.assertEqual("987", persisted["selected_artifact_id"])
        self.assertIn("| Producer attempt | `1` |", summary)
        self.assertIn("| Consumer attempt | `2` |", summary)


class WorkflowArtifactProvenanceTest(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))

    def test_production_wiring_uses_exact_ids_and_retained_attempts(self) -> None:
        self.assertEqual([], workflow_contract_failures(self.workflow))

    def test_contract_check_rejects_permission_fallback_and_receipt_mutations(self) -> None:
        mutations: list[tuple[str, dict[str, object]]] = []

        expanded = copy.deepcopy(self.workflow)
        expanded["permissions"]["actions"] = "read"
        mutations.append(("permission expansion", expanded))

        fallback = copy.deepcopy(self.workflow)
        download = named_step(
            fallback["jobs"]["functional-e2e-process-operations"],
            "Download worker binaries",
        )
        download["with"] = {
            "name": "worker-binaries-latest",
            "path": "${{ runner.temp }}/worker-download",
        }
        mutations.append(("name fallback", fallback))

        missing_guard = copy.deepcopy(self.workflow)
        summary_steps = missing_guard["jobs"]["restate-postgres-workers-summary"]["steps"]
        summary_steps[:] = [
            step
            for step in summary_steps
            if step.get("name") != "Require exact segment result selections"
        ]
        mutations.append(("missing pre-download guard", missing_guard))

        missing_receipt = copy.deepcopy(self.workflow)
        prepare = named_step(
            missing_receipt["jobs"]["restate-postgres-workers"],
            "Record segment 1 result provenance",
        )
        prepare["run"] = prepare["run"].replace(
            "--payload worker-binaries-consumption.json",
            "--payload omitted-consumption.json",
        )
        mutations.append(("missing binary receipt", missing_receipt))

        for name, mutation in mutations:
            with self.subTest(name=name):
                self.assertNotEqual([], workflow_contract_failures(mutation))


if __name__ == "__main__":
    unittest.main()
