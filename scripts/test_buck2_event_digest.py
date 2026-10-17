#!/usr/bin/env python3
"""Tests for the Buck2 event-log digest."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "buck2_event_digest", ROOT / "scripts/ci/buck2_event_digest.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def event(endpoint, kind, span_id, value=None, parent_id=0, timestamp=0, **fields):
    return {"Event": {"trace_id": "actual-invocation", "span_id": span_id,
                      "parent_id": parent_id, "timestamp": [1, timestamp],
                      "data": {endpoint: {"data": {kind: value or {}}, **fields}}}}


def read(*events):
    return MODULE.read_events(json.dumps(item) for item in events)


def action(cache_hit=False, digest="abc:42"):
    return {"key": {"owner": {"TargetLabel": {
                "label": {"package": "root//crates/example", "name": "example__rust_test"},
                "configuration": {"full_name": "actual-config#123"}}}},
            "name": {"category": "rustc", "identifier": "link [pic]"},
            "commands": [{"details": {"command_kind": {"command": {"RemoteCommand": {
                "action_digest": digest, "cache_hit": cache_hit, "queue_time_us": 7}}},
                "metadata": {"execution_time_us": 30, "queue_duration_us": None}}}]}


INVENTORY = {"packages": [{"package": "example-package", "targets": [
    {"label": "//crates/example:example", "cargo": "example", "kind": "test"}]}]}
USAGE = "1\taction_digest=abc/42\tcategory=rustc\tpkg=example-package\tcrate=example\tcpu_usec=25\tpeak_bytes=500\tanon_peak_bytes=400\texit=0\twall_ms=1\trequested_kb=512"


def joined(value=None, usage=(USAGE,), second=None):
    events = [event("SpanEnd", "ActionExecution", 2, value or action())]
    if second:
        events.append(event("SpanEnd", "ActionExecution", 3, second))
    receipt = read(*events)
    MODULE.join_usage(receipt, usage, INVENTORY)
    return receipt["spans"][0]["usage"][0]


class DigestTests(unittest.TestCase):
    @mock.patch.object(Path, "read_text", return_value=json.dumps(INVENTORY))
    @mock.patch.object(Path, "mkdir")
    @mock.patch.object(Path, "write_text")
    @mock.patch.object(MODULE, "file_identity", return_value={"sha256": "fixture"})
    @mock.patch.object(MODULE, "client", return_value="/fixture/buck2")
    @mock.patch.object(MODULE, "command")
    def test_digest_uses_the_event_log_for_summary_and_critical_path(
        self, command: mock.Mock, _client: mock.Mock, _identity, _write, _mkdir, _read
    ) -> None:
        command.side_effect = ["Actions: 12\nCache hits: 10", "\n".join(f"critical-{index}" for index in range(12)), json.dumps(event("SpanStart", "Command", 1))]
        lines = MODULE.digest(Path("events.pb.zst"))
        self.assertIn("  Actions: 12", lines)
        self.assertIn("  critical-11", lines)
        self.assertEqual(
            command.call_args_list[0],
            mock.call("/fixture/buck2", "summary", "events.pb.zst"),
        )
        self.assertEqual(command.call_args_list[1].args[-1], "events.pb.zst")

    @mock.patch.object(MODULE, "digest", side_effect=subprocess.CalledProcessError(1, "decode"))
    @mock.patch.object(Path, "is_file", return_value=True)
    def test_decode_failure_is_a_failed_command(self, _is_file, _digest):
        with mock.patch.object(sys, "argv", ["digest", "events.pb.zst"]):
            self.assertEqual(MODULE.main(), 1)

    def test_overlapping_action_walls_never_replace_command_elapsed(self):
        receipt = read(
            event("SpanStart", "Command", 1),
            event("SpanStart", "ActionExecution", 2, action(), timestamp=1),
            event("SpanStart", "ActionExecution", 3, action(), timestamp=2),
            event("SpanEnd", "ActionExecution", 2, action(), timestamp=8, duration_us=8),
            event("SpanEnd", "ActionExecution", 3, action(), timestamp=9, duration_us=7),
            event("SpanEnd", "Command", 1, {"is_success": True}, timestamp=10, duration_us=10))
        MODULE.enrich(receipt)
        self.assertEqual(receipt["commands"][0]["elapsed_us"], 10)
        self.assertEqual(len(receipt["action_intervals"]), 4)
        self.assertEqual(receipt["population"]["actions"], 2)

    def test_configuration_cache_stages_materialization_and_missing_joins_survive(self):
        value = action(cache_hit=True)
        stage = {"stage": {"Re": {"stage": {"Queue": {"action_digest": "abc:42"}}}}}
        graph = {"critical_path2": [{"span_ids": [2, 99], "queue_duration_us": None,
                                    "start_offset_ns": 3, "was_reused": True}]}
        receipt = read(
            event("SpanStart", "ActionExecution", 2, value),
            event("SpanEnd", "ActionExecution", 2, value, duration_us=40),
            event("SpanStart", "ExecutorStage", 3, stage, parent_id=2),
            event("SpanStart", "Materialization", 4, {"action_digest": None}),
            event("Instant", "BuildGraphInfo", 0, graph))
        MODULE.enrich(receipt)
        self.assertEqual(receipt["spans"][1]["action_span_id"], 2)
        self.assertEqual(payload_for_test(receipt["spans"][1], "SpanStart", "ExecutorStage"), stage)
        self.assertEqual(receipt["spans"][2]["action_join"], "missing_action_ancestor")
        self.assertEqual(receipt["spans"][2]["missing_endpoints"], ["SpanEnd"])
        self.assertEqual(receipt["critical_path_joins"][0]["missing_span_ids"], [99])
        self.assertEqual(payload_for_test(receipt["spans"][0], "SpanEnd", "ActionExecution"), value)
        self.assertEqual(receipt["identity_events"]["BuildGraphInfo"][0], event("Instant", "BuildGraphInfo", 0, graph)["Event"])

    def test_cpu_and_memory_require_unique_exact_digest_category_and_identity(self):
        # A worker start/heartbeat is not a second completed attempt.
        row = joined(usage=[USAGE, USAGE.replace("cpu_usec=25", "cpu_usec=-")])
        self.assertEqual(row["status"], "exact_action_digest")
        self.assertEqual(row["cpu_usec"], 25)
        self.assertEqual(row["cgroup_peak_bytes"], 500)
        self.assertEqual(row["anonymous_peak_bytes"], 400)
        self.assertIsNone(row["rss_peak_bytes"])
        for records, status in (
            ([USAGE, USAGE], "ambiguous_action_attempt"),
            ([USAGE.replace("abc/42", "-")], "missing_exact_usage"),
            ([USAGE.replace("pkg=example-package", "pkg=other")], "usage_identity_mismatch"),
            ([USAGE.replace("category=rustc", "category=clippy")], "usage_identity_mismatch")):
            with self.subTest(status=status):
                row = joined(usage=records)
                self.assertEqual(row["status"], status)
                self.assertIsNone(row["cpu_usec"])
        self.assertEqual(joined(second=action())["status"], "ambiguous_action_attempt")
        self.assertEqual(joined(action(cache_hit=True))["status"], "cache_hit_or_not_remote_execution")

    def test_scheduler_operation_retry_order_preserves_workers_and_effective_resources(self):
        usage = [USAGE.replace("action_digest=abc/42", "action_digest=-") +
                 f"\toperation_id=operation\tattempt=attempt-{index}" for index in range(2)]
        rows = [{"operation_id": "operation", "action_digest": "abc-42",
                 "worker": worker, "identity": "repo:lash|lane:actual",
                 "effective_cpu": 2, "effective_memory_kb": 1024,
                 "effective_priority": 0, "dispatch_s": index}
                for index, worker in enumerate(("worker-first", "worker-retry"))]
        records, coverage = MODULE.scheduler_usage(usage, map(json.dumps, rows), priority_valid_from=1)
        receipt = read(event("SpanEnd", "ActionExecution", 2, action()))
        MODULE.join_usage(receipt, records, INVENTORY)
        row = receipt["spans"][0]["usage"][0]
        self.assertEqual(row["status"], "exact_scheduler_operation_attempts")
        self.assertIsNone(row["cpu_usec"])
        attempts = row["worker_attempts"]
        self.assertEqual([r["_scheduler"]["worker"] for r in attempts], ["worker-first", "worker-retry"])
        self.assertEqual([r["_scheduler"]["attempt_index"] for r in attempts], [0, 1])
        self.assertIsNone(attempts[0]["_scheduler"]["effective_priority"])
        self.assertEqual(attempts[1]["_scheduler"]["effective_priority"], 0)
        self.assertEqual(attempts[0]["requested_kb"], "512")
        self.assertEqual(attempts[0]["_scheduler"]["effective_memory_kb"], 1024)
        self.assertEqual(coverage["usage_attempt_joins"], {"exact_operation_attempt_order": 2})

    def test_scheduler_partial_or_unordered_attempts_never_supply_a_digest(self):
        usage = [USAGE.replace("action_digest=abc/42", "action_digest=-") +
                 f"\toperation_id=operation\tattempt=attempt-{index}" for index in range(2)]
        row = {"operation_id": "operation", "action_digest": "abc-42",
               "worker": "worker", "identity": "repo:lash|lane:actual",
               "effective_cpu": 2, "effective_memory_kb": 1024,
               "effective_priority": 100, "dispatch_s": 2}
        for rows, status in (([], "missing_scheduler_operation"),
                             ([row], "ambiguous_operation_attempts"),
                             ([row, dict(row, dispatch_s=1)], "ambiguous_operation_attempts"),
                             ([dict(row, identity="repo:other")], "missing_scheduler_operation")):
            with self.subTest(status=status):
                records, coverage = MODULE.scheduler_usage(usage, map(json.dumps, rows))
                self.assertTrue(all(r["action_digest"] == "-" for r in records))
                self.assertEqual(coverage["usage_attempt_joins"], {status: 2})

    def test_empty_or_mixed_invocations_refuse_an_identity_claim(self):
        with self.assertRaises(ValueError):
            read()
        other = event("SpanStart", "Command", 2)
        other["Event"]["trace_id"] = "other-invocation"
        with self.assertRaises(ValueError):
            read(event("SpanStart", "Command", 1), other)


def payload_for_test(span, endpoint, kind):
    return span[endpoint]["data"][endpoint]["data"][kind]


if __name__ == "__main__":
    unittest.main()
