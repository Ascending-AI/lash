#!/usr/bin/env python3
"""Behavior tests for exact same-run CI artifact provenance."""

from __future__ import annotations

import importlib.util
import io
import json
import pathlib
import sys
import tarfile
import tempfile
import unittest



ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location(
    "artifact_provenance", ROOT / "scripts" / "ci" / "artifact_provenance.py"
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


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


if __name__ == "__main__":
    unittest.main()
