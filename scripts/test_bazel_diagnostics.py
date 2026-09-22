#!/usr/bin/env python3
"""The diagnostic join must never turn stale or ambiguous worker data into proof."""

import importlib.util
import io
import json
import os
import signal
from types import SimpleNamespace
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "diagnostics", ROOT / "tools/bazel/diagnostics.py"
)
diag = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diag)

DIGEST = {"hash": "ab", "sizeBytes": "12"}
METADATA = {
    "worker": "worker-a",
    "queuedTimestamp": "2026-09-22T12:00:02Z",
    "workerStartTimestamp": "2026-09-22T12:00:01Z",
    "executionStartTimestamp": "2026-09-22T12:00:03Z",
    "executionCompletedTimestamp": "2026-09-22T12:00:05Z",
}


def execute(metadata=METADATA, cached=False):
    return {
        "details": {
            "execute": {
                "request": {"actionDigest": DIGEST},
                "responses": [
                    {
                        "name": "operations/one",
                        "done": True,
                        "response": {
                            "cachedResult": cached,
                            "result": {"executionMetadata": metadata},
                        },
                    }
                ],
            }
        }
    }


class AttributionTest(unittest.TestCase):
    def summarize(self, grpc, cached=False):
        spawn = {
            "spawn": {
                "digest": DIGEST,
                "targetLabel": "//:one",
                "mnemonic": "Rustc",
                "cacheHit": cached,
            }
        }

        def decoded(_binary, _bundle, filename, _flag, _errors):
            return iter(grpc if filename == "grpc.bin" else [spawn])

        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(diag, "decoded", decoded),
            patch("sys.stdout", io.StringIO()),
        ):
            return diag.summarize("bb", Path(directory))["actions"][0]

    def test_fresh_phase_attribution_keeps_clock_skew_unknown(self):
        action = self.summarize([execute()])
        self.assertEqual("executed", action["attribution"])
        self.assertEqual("worker-a", action["worker"])
        self.assertEqual(
            {"seconds": None, "status": "clock_skew"}, action["phases"]["queue"]
        )
        self.assertEqual(2, action["phases"]["execution"]["seconds"])
        self.assertEqual("missing", action["phases"]["output_upload"]["status"])

    def test_cache_metadata_is_historical(self):
        call = {
            "details": {
                "getActionResult": {
                    "request": {"actionDigest": DIGEST},
                    "response": {"executionMetadata": METADATA},
                }
            }
        }
        action = self.summarize([call], cached=True)
        self.assertEqual("historical_cache_worker", action["attribution"])
        self.assertEqual("worker-a", action["worker"])

    def test_execute_cache_hit_is_not_fresh_worker_execution(self):
        action = self.summarize([execute(cached=True)])
        self.assertEqual("cache_hit", action["attribution"])
        self.assertIsNone(action["worker"])

    def test_conflicting_retries_never_pick_arbitrary_worker(self):
        action = self.summarize(
            [execute(), execute({**METADATA, "worker": "worker-b"})]
        )
        self.assertEqual("ambiguous_attempts", action["attribution"])
        self.assertIsNone(action["worker"])

    def test_unjoined_action_stays_unavailable(self):
        action = self.summarize([])
        self.assertEqual("unavailable", action["attribution"])
        self.assertIsNone(action["worker"])

    def test_wait_execution_joins_operation_after_log_reordering(self):
        wait = {
            "details": {
                "waitExecution": {
                    "responses": [
                        {
                            "name": "operations/one",
                            "done": True,
                            "response": {"result": {"executionMetadata": METADATA}},
                        }
                    ]
                }
            }
        }
        start = {
            "details": {
                "execute": {
                    "request": {"actionDigest": DIGEST},
                    "responses": [{"name": "operations/one"}],
                }
            }
        }
        action = self.summarize([wait, start])
        self.assertEqual("executed", action["attribution"])

    def test_interrupted_capture_cannot_be_complete_with_empty_valid_logs(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(diag, "decoded", return_value=iter([])),
            patch("sys.stdout", io.StringIO()),
        ):
            bundle = Path(directory)
            (bundle / "manifest.json").write_text('{"state":"interrupted"}')
            report = diag.summarize("bb", bundle)
            self.assertFalse(report["complete"])
            self.assertIn("unfinished actions", report["errors"][0])

    def test_source_identity_failure_remains_incomplete_when_reported_again(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(diag, "decoded", return_value=iter([])),
            patch("sys.stdout", io.StringIO()),
        ):
            bundle = Path(directory)
            (bundle / "manifest.json").write_text(json.dumps({
                "state": "failed", "source_identity_error": "source removed"
            }))
            report = diag.summarize("bb", bundle)
            self.assertFalse(report["complete"])
            self.assertIn("Source identity unavailable", report["errors"][0])

    def test_interrupt_during_post_build_hash_is_saved(self):
        script_spec = importlib.util.spec_from_file_location(
            "capture", ROOT / "scripts/bazel-diagnose.py"
        )
        capture = importlib.util.module_from_spec(script_spec)
        script_spec.loader.exec_module(capture)
        count = 0

        def identity():
            nonlocal count
            count += 1
            if count == 2:
                os.kill(os.getpid(), signal.SIGINT)
            return {"revision": "same", "content_sha256": "same"}

        child = SimpleNamespace(pid=123, wait=lambda: 0)
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(capture, "source_identity", identity),
            patch.object(capture.subprocess, "Popen", return_value=child),
            patch.object(capture.os, "killpg"),
            patch.object(capture, "summarize", return_value={"complete": True}),
            patch("sys.stdout", io.StringIO()),
        ):
            output = Path(directory) / "bundle"
            args = SimpleNamespace(
                output=output, baseline=None, operation="build", bazel_args=["//:probe"]
            )
            self.assertEqual(130, capture.capture(args, "bb"))
            manifest = json.loads((output / "manifest.json").read_text())
            self.assertEqual("interrupted", manifest["state"])
            self.assertEqual(0, manifest["build_exit_code"])
            self.assertFalse(manifest["diagnostics_complete"])

    def test_post_build_hash_failure_preserves_build_status_and_handlers(self):
        script_spec = importlib.util.spec_from_file_location(
            "capture", ROOT / "scripts/bazel-diagnose.py"
        )
        capture = importlib.util.module_from_spec(script_spec)
        script_spec.loader.exec_module(capture)
        original = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        child = SimpleNamespace(pid=123, wait=lambda: 42)
        try:
            with (
                tempfile.TemporaryDirectory() as directory,
                patch.object(capture, "source_identity", side_effect=[
                    {"revision": "same", "content_sha256": "same"},
                    FileNotFoundError("source removed while hashing"),
                ]),
                patch.object(capture.subprocess, "Popen", return_value=child),
                patch.object(capture, "summarize", return_value={"complete": True}),
                patch("sys.stdout", io.StringIO()),
            ):
                output = Path(directory) / "bundle"
                args = SimpleNamespace(
                    output=output, baseline=None, operation="build", bazel_args=["//:probe"]
                )
                self.assertEqual(42, capture.capture(args, "bb"))
                manifest = json.loads((output / "manifest.json").read_text())
                self.assertEqual("failed", manifest["state"])
                self.assertEqual(42, manifest["build_exit_code"])
                self.assertIsNone(manifest["source_changed_during_build"])
                self.assertIn("source removed", manifest["source_identity_error"])
                self.assertFalse(manifest["diagnostics_complete"])
                for sig, handler in original.items():
                    self.assertEqual(handler, signal.getsignal(sig))
        finally:
            for sig, handler in original.items():
                signal.signal(sig, handler)

    def test_streaming_json_rejects_truncated_tail(self):
        source = io.StringIO(json.dumps({"nested": {"text": "}"}}) + '\n{"unfinished":')
        values = diag.json_stream(source)
        self.assertEqual({"nested": {"text": "}"}}, next(values))
        with self.assertRaisesRegex(ValueError, "Truncated"):
            next(values)


if __name__ == "__main__":
    unittest.main()
