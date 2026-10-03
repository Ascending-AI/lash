#!/usr/bin/env python3
"""Run H05's existing handler laws once on their owning developer targets."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import tomllib
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
CASES = {
    "handlers": (
        "//crates/lash-restate-test:server_semantics__test",
        (
            "sdk_sleep_deadline_survives_a_held_response_frame",
            "sdk_delayed_send_deadline_survives_a_held_response_frame",
            "object_state_is_serialized_per_key_and_calls_return_results",
            "a_workflow_runs_once_sleeps_on_virtual_time_and_waits_for_its_promise",
            "a_crash_before_a_run_result_is_stored_replays_and_reruns_the_effect",
            "awakeables_route_their_completion_to_the_owning_invocation",
            "exhausting_the_handler_retry_policy_pauses_the_invocation_until_resumed",
            "a_purged_workflow_run_starts_over_under_its_key_with_an_empty_journal",
            "cancelling_a_suspended_workflow_ends_it_as_cancelled",
            "removing_a_deployment_with_pinned_invocations_is_refused_unless_forced",
            "a_new_build_serves_new_invocations_while_pinned_ones_finish_on_theirs",
            "concurrent_runs_record_independent_receipts_in_completion_order",
            "a_crash_reruns_only_calls_without_a_recorded_result",
            "reported_transient_failures_retry_only_calls_without_a_recorded_result",
            "exhausting_a_bounded_policy_records_a_terminal_result_for_that_call",
            "a_terminal_failure_is_recorded_once_as_that_calls_result",
            "cancellation_settles_each_handle_once_by_journal_order",
            "a_failed_handle_ends_the_invocation_and_drops_unfinished_siblings",
            "dropping_a_result_future_neither_cancels_nor_settles_its_call",
        ),
    ),
    "wake": (
        "//crates/lash-restate-test:run_wake__test",
        (
            "completed_runs_wake_their_handler_without_another_input_frame_on_v6",
            "completed_runs_wake_their_handler_without_another_input_frame_on_v7",
        ),
    ),
    "adapter": (
        "//crates/lash-restate:lash-restate__unit_test",
        (
            "tests::tool_run_sdk_contract::hosts_and_lash_name_one_endpoint_through_the_reexport",
            "tests::guarded_surface_tests::every_guarded_surface_decodes_its_supported_range",
            "services::tests::a_route_reads_back_as_the_route_it_names",
            "session_shifts::tests::a_turn_workflow_key_round_trips_any_session_and_run",
            "wire::tests::a_disjoint_call_is_refused_before_its_body_decodes",
            "wire::tests::call_accepts_exact_limits_and_refuses_each_overrun",
            "serve::tests::legal_input_at_the_message_budget_succeeds",
            "serve::tests::raw_http2_budget_plus_one_refuses_before_payload",
            "serve::tests::raw_http2_long_replay_of_legal_messages_succeeds",
        ),
    ),
    "driver": (
        "//crates/lash-restate-test:one_driver__test",
        ("a_host_submission_leaves_the_engine_the_only_driver",),
    ),
    "host": (
        "//crates/lash-restate-test:host_send_wait__test",
        (
            "a_host_replayed_after_acceptance_submits_once",
            "an_exclusive_handler_accepts_and_a_shared_handler_waits",
        ),
    ),
    "transition": (
        "//crates/lash:lash__unit_test",
        ("formats::tests::plugin_transition_generation::an_untagged_transition_parks_before_decoding_or_invoking_work",),
    ),
}


def provenance():
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    pin = manifest["workspace"]["dependencies"]["restate-sdk"]
    revision = pin["rev"]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("the SDK dependency must name a full git revision")
    if pin.get("default-features") is not False or pin.get("features") != ["http_server"]:
        raise ValueError("the SDK dependency must retain its endpoint feature contract")
    for patches in manifest.get("patch", {}).values():
        if any(name.startswith("restate-sdk") for name in patches):
            raise ValueError("the SDK must be a normal dependency, not a patch")
    packages = tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]
    versions = {}
    source = f"git+{pin['git']}?rev={revision}#{revision}"
    for name in ("restate-sdk", "restate-sdk-macros", "restate-sdk-shared-core"):
        matches = [package for package in packages if package["name"] == name]
        if len(matches) != 1:
            raise ValueError(f"expected one locked {name}, found {len(matches)}")
        package = matches[0]
        if name != "restate-sdk-shared-core" and package["source"] != source:
            raise ValueError(f"{name} does not match the workspace git pin")
        versions[name] = package["version"]
    owners = [
        package["name"]
        for package in packages
        if any(dep.split()[0] == "restate-sdk" for dep in package.get("dependencies", []))
    ]
    if owners != ["lash-internal-restate"]:
        raise ValueError(f"SDK dependency owners changed: {owners}")
    return {"git": pin["git"], "revision": revision, "versions": versions, "owners": owners}


def executed_cases(report_path, expected):
    report = json.loads(report_path.read_text())
    if report.get("session_complete") is not True or report.get("infrastructure_errors"):
        raise ValueError(f"incomplete or failed test report: {report_path}")
    names = []
    for result in report["results"].values():
        if result["status"] != "PASS" or result.get("cache"):
            raise ValueError(f"test did not pass uncached: {result['label']}")
        for case in ET.parse(result["outputs"]["junit_xml"]).getroot().iter("testcase"):
            if case.find("skipped") is not None:
                continue
            if case.find("failure") is not None or case.find("error") is not None:
                raise ValueError(f"test failed: {case.get('name')}")
            names.append(case.attrib["name"])
    if sorted(names) != sorted(expected):
        raise ValueError(f"executed cases differ from selection: expected {expected}, got {names}")
    return len(names)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", choices=CASES, action="append", help="select an owning target")
    parser.add_argument("--output-dir", type=Path, default=ROOT / ".tmp/restate-sdk-acceptance")
    args = parser.parse_args()
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    receipt_path = output / "receipt.json"
    receipt_path.unlink(missing_ok=True)
    receipt = {"provenance": provenance(), "groups": {}, "executed": 0, "complete": False}
    print(json.dumps(receipt["provenance"], indent=2), flush=True)
    for group in dict.fromkeys(args.group or CASES):
        target, cases = CASES[group]
        report = output / f"{group}.json"
        report.unlink(missing_ok=True)
        command = [
            "kiln", "test", target, "--no-test-cache", "--test_arg=--exact",
            "--test_arg=--nocapture", "--test-report", str(report),
            "--test-output-dir", str(output / group),
            *(f"--test_arg={case}" for case in cases),
        ]
        subprocess.run(command, cwd=ROOT, check=True)
        count = executed_cases(report, cases)
        receipt["groups"][group] = {"target": target, "executed": count, "report": str(report)}
        receipt["executed"] += count
        receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")
        print(f"H05 {group}: {count} executed, {count} passed", flush=True)
    receipt["complete"] = True
    receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"H05 total: {receipt['executed']} executed, {receipt['executed']} passed", flush=True)


if __name__ == "__main__":
    main()
