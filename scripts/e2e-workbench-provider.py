#!/usr/bin/env python3
"""FIG-4988: drive the real workbench against the recorded provider fixture.

The Rust host owns the fixture, Restate namespace and teardown; this script
owns Playwright and the product-layer assertions. Control actions go over the
H6_CONTROL stdin/stdout protocol exactly as scripts/e2e-workbench-recovery.py
does.
"""
from __future__ import annotations

import argparse
from importlib.util import module_from_spec, spec_from_file_location
import json
from pathlib import Path
import sqlite3
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

COUNTER = r"""(() => {
    window.replyCounts = [];
    // Done is a live product-stream event, retired from /api/state once the
    // turn settles. Tee the browser's own stream to keep what it received.
    window.productStreamItems = [];
    window.productStreamErrors = [];
    const fetch = window.fetch.bind(window);
    window.fetch = async (...args) => {
        const response = await fetch(...args);
        if (new URL(response.url).pathname === '/api/events') {
            const reader = response.clone().body.getReader();
            const decoder = new TextDecoder();
            (async () => {
                let buffer = '';
                while (true) {
                    const {value, done} = await reader.read();
                    if (done) break;
                    buffer += decoder.decode(value, {stream: true});
                    let newline;
                    while ((newline = buffer.indexOf('\n')) >= 0) {
                        const line = buffer.slice(0, newline).trim();
                        buffer = buffer.slice(newline + 1);
                        if (line) window.productStreamItems.push(JSON.parse(line));
                    }
                }
            })().catch(error => window.productStreamErrors.push(String(error)));
        }
        return response;
    };
    const install = () => {
        const timeline = document.querySelector('#timeline');
        if (!timeline) return;
        new MutationObserver(() => window.replyCounts.push(
            [...document.querySelectorAll('#timeline .message.assistant')]
                .filter(node => node.textContent.includes('one answer') ||
                                node.textContent.includes('recovered')).length
        )).observe(timeline, {childList: true, subtree: true, characterData: true});
    };
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', install);
    } else {
        install();
    }
})()"""


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def initial_state(base_url, deadline):
    """Bootstrap only after product health, within the enclosing case budget.

    The first state read can initialize the selected session. It shares the
    case deadline rather than the five-second budget of later settled reads.
    """
    def remaining():
        seconds = deadline - time.monotonic()
        assert seconds > 0, "watchdog: workbench startup missed the case deadline"
        return seconds

    while True:
        try:
            with urllib.request.urlopen(base_url + "/healthz", timeout=remaining()) as response:
                health = json.load(response)
            if health.get("service") == "agent-workbench" and health.get("status") == "ok":
                break
        except (urllib.error.URLError, TimeoutError):
            pass
        time.sleep(min(0.05, remaining()))
    with urllib.request.urlopen(base_url + "/api/state", timeout=remaining()) as response:
        return json.load(response)


def browser(args):
    from playwright.sync_api import expect, sync_playwright

    spec = spec_from_file_location(
        "projection", Path(__file__).with_name("workbench-transcript-projection-e2e.py"))
    projection = module_from_spec(spec)
    spec.loader.exec_module(projection)
    directory = args.directory
    database = directory / "workbench-data/lash-sessions/durable-core.db"
    rows = projection.StoreRows("sqlite_file", directory / "workbench-data")
    trace_path = directory / "workbench-data/trace.jsonl"

    def control(action, input=None):
        print("H6_CONTROL " + json.dumps({"action": action, "input": input or {}}), flush=True)
        receipt = json.loads(sys.stdin.readline())
        assert "error" not in receipt, receipt
        return receipt

    def state():
        with urllib.request.urlopen(args.base_url + "/api/state", timeout=5) as response:
            return json.load(response)

    def store(session):
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
            connection.row_factory = sqlite3.Row
            return {table: [dict(row) for row in connection.execute(
                f"SELECT * FROM {table} WHERE session_id=?", (session,))]
                for table in ("pending_turn_inputs", "session_meta", "graph_nodes",
                              "runtime_turn_commits", "session_runs")}

    def try_store(session):
        try:
            return store(session)
        except sqlite3.Error:
            return None

    def trace(session):
        if not trace_path.exists():
            return []
        return [record for line in trace_path.read_text().splitlines()
                if line.strip()
                and (record := json.loads(line)).get("context", {}).get("session_id") == session]

    def poll(label, predicate):
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.05)
        raise AssertionError("watchdog: " + label)

    def page_url(session):
        return args.base_url + "/?" + urllib.parse.urlencode({"session_id": session})

    def open_page(chrome, session):
        page = chrome.new_page()
        page.add_init_script(COUNTER)
        page.goto(page_url(session), wait_until="domcontentloaded")
        expect(page.locator("#prompt")).to_be_visible()
        expect(page.locator("#sessionId")).to_have_text(session)
        return page

    def submit(page, text):
        page.locator("#prompt").fill(text)
        with page.expect_response(
            lambda response: urllib.parse.urlsplit(response.url).path == "/api/turn"
            and response.request.method == "POST"
        ) as accepted_response:
            page.locator("#send").click()
        accepted = accepted_response.value.json()
        assert accepted["accepted"] is True and not accepted["queued"], accepted
        return accepted["turn_id"]

    def recorded_input(session, turn):
        def probe():
            snapshot = try_store(session)
            if snapshot is None:
                return None
            # A fast failed Run may settle before the first read. Admission
            # remains recorded on the Run; terminal input bindings are cleared.
            runs = [row for row in snapshot["session_runs"] if row["run"] == turn
                    and row["admission_json"]]
            if len(runs) != 1:
                return None
            admission = json.loads(runs[0]["admission_json"])
            inputs = admission["inputs"]["inputs"]
            assert len(inputs) == 1, admission
            assert inputs[0]["session_id"] == session and inputs[0]["source_key"] == turn, admission
            return next((row for row in snapshot["pending_turn_inputs"]
                         if row["input_id"] == inputs[0]["input_id"]), None)
        return poll(f"accepted input recorded by Run {turn}", probe)

    def settled_terminal(session, count):
        def probe():
            snapshot = state()
            terminals = [r for r in trace(session) if r["type"] == "turn_completed"]
            if not snapshot["active_turns"] and len(terminals) == count:
                return snapshot, terminals
        return poll(f"{count} turn terminal(s)", probe)

    def no_open_input(snapshot):
        return not any(row["status"]["kind"] == "open"
                       for row in snapshot["pending_turn_inputs"])

    def visible_assistants(snapshot):
        return [row for row in snapshot["transcript"]
                if row["kind"] == "assistant_reply" and not row["suppressed"]]

    def visible_users(snapshot):
        return [row for row in snapshot["transcript"]
                if row["kind"] == "user" and not row["suppressed"]]

    initial = initial_state(args.base_url, time.monotonic() + args.remaining_case_seconds)
    session = initial["settings"]["session_id"]
    cursor = initial["observation"]["cursor"]
    assert not initial["active_turns"] and not [row for row in initial["transcript"] if not row["suppressed"]], "session must be fresh"

    with sync_playwright() as playwright:
        chrome = playwright.chromium.launch()
        try:
            if args.scenario == "s26-rate-limit":
                page = open_page(chrome, session)
                turn = submit(page, "answer once")
                work = recorded_input(session, turn)
                entered = control("provider-barrier", {"barrier": "answer-started"})
                assert entered["entered"]["occurrence"] == "rate-limit-1", entered
                counts_a = page.evaluate("window.replyCounts")
                page.close()
                control("provider-release", {"barrier": "answer-started"})
                final, terminals = settled_terminal(session, 1)
                assert terminals[0]["outcome"]["status"] == "completed", terminals
                reply = visible_assistants(final)
                assert len(reply) == 1 and reply[0]["content"]["text"] == "one answer", reply
                assert reply[0]["provenance"]["is_turn_reply"], reply
                assert reply[0]["provenance"]["turn_id"] == turn, reply
                assert len(visible_users(final)) == 1, final["transcript"]
                assert no_open_input(final), final["pending_turn_inputs"]
                usage = final["observation"]["usage"]
                assert usage["input_tokens"] == 11 and usage["output_tokens"] == 2, usage
                bound = store(session)["pending_turn_inputs"]
                assert any(row["input_id"] == work["input_id"] and row["state"] == "completed"
                           and row["admitted_run"] is None and row["admitted_by"] is None
                           for row in bound), bound
                runs = [r for r in store(session)["session_runs"] if r["run"] == turn]
                assert len(runs) == 1 and runs[0]["terminal_kind"] == "answered", runs
                assert any(a["input_id"] == work["input_id"] and a["turn_id"] == turn
                           for a in final["turn_input_applications"]), final
                calls = [r for r in trace(session) if r["type"] == "llm_call_completed"]
                assert len(calls) == 1, calls
                attempts = calls[0].get("attempts") or []
                assert [a["ordinal"] for a in attempts] == [1, 2], attempts
                error = attempts[0]["detail"]["error"]
                assert error["class"] == "quota" and error["http_status"] == 429, error
                call_usage = calls[0].get("usage") or {}
                assert call_usage["input_tokens"] == 11, calls[0]
                assert call_usage["output_tokens"] == 2, calls[0]
                assert not [r for r in trace(session) if r["type"] == "llm_call_failed"]
                requests = control("provider-requests")["requests"]
                assert len(requests) == 2 and requests[0] == requests[1], requests
                observations = control("observations",
                                       {"session_id": session, "turn_id": turn,
                                        "cursor": cursor})["items"]
                assert "one answer" in json.dumps(observations), observations
                terminal_items = [i for i in observations if i["type"] == "terminal_replacement"]
                assert terminal_items, "no terminal_replacement observation"
                assert "one answer" in json.dumps(terminal_items), terminal_items
                late = open_page(chrome, session)
                assistant = late.locator("#timeline .message.assistant")
                expect(assistant).to_have_count(1, timeout=90000)
                assert "one answer" in assistant.inner_text()
                counts = late.evaluate("window.replyCounts")
                assert counts and max(counts) == 1, "observer duplicated or lost the answer"
                assert counts_a and max(counts_a) <= 1, "live observer saw a duplicate"
                write(directory / "s26-rate-limit-evidence.json", {
                    "api": final, "store": store(session), "trace": trace(session),
                    "requests": requests, "observations": observations,
                    "reply_counts": {"detached": counts_a, "late": counts},
                    "entered": entered, "turn": turn, "input_id": work["input_id"],
                })
                projection.assert_three_layers(late, final, rows,
                                               directory / "s26-late-browser.json",
                                               navigation_wait="domcontentloaded")
                write(directory / "scorecard.json", {
                    "scenario": "s26-rate-limit", "selected": 1, "executed": 1,
                    "verdict": "PASS", "turn": turn, "input_id": work["input_id"],
                    "requests": len(requests), "usage": usage,
                })
            elif args.scenario == "s26-partial-disconnect":
                page = open_page(chrome, session)
                turn = submit(page, "partial answer")
                work = recorded_input(session, turn)
                expect(page.locator("#timeline")).to_contain_text("partial", timeout=90000)
                final, terminals = settled_terminal(session, 1)
                outcome = terminals[0]["outcome"]
                assert outcome["status"] == "failed" and outcome["done_reason"] == "provider_error", outcome
                assert not visible_assistants(final), final["transcript"]
                assert no_open_input(final), final["pending_turn_inputs"]
                # The adapter refuses retry directly. ChargeSafety settlements
                # are for policy-denied retryable failures, a different cause.
                records = [e["record"] for e in final["product_events"]["events"]
                           if e["type"] == "model_call_recorded"]
                assert len(records) == 1 and len(records[0]["attempts"]) == 1, records
                attempt = records[0]["attempts"][0]
                assert attempt["protocol_position"] == "output_started", attempt
                assert attempt["outcome"] == "failed" and attempt["error"]["class"] == "transport", attempt
                assert attempt["retry_decision"] == {"outcome": "declined", "cause": "not_retryable"}, attempt
                runs = [r for r in store(session)["session_runs"] if r["run"] == turn]
                assert len(runs) == 1 and runs[0]["terminal_kind"] == "failed", runs
                cause = json.loads(runs[0]["terminal_cause_json"])
                assert cause["turn"] == turn and cause["outcome"] == {"stopped": "provider_error"}, cause
                failed = [r for r in trace(session) if r["type"] == "llm_call_failed"]
                assert len(failed) == 1, failed
                attempts = failed[0].get("attempts") or []
                assert len(attempts) == 1, attempts
                detail = attempts[0]["detail"]
                assert detail["error"]["class"] == "transport", detail
                decision = detail.get("retry_decision") or {}
                assert (failed[0]["error"]["retryable"] is False
                        or decision.get("outcome") == "declined"), failed[0]
                assert not [r for r in trace(session) if r["type"] == "llm_call_completed"]
                requests = control("provider-requests")["requests"]
                assert len(requests) == 1, requests
                expect(page.locator("#timeline .message.assistant")).to_have_count(0)
                page.reload(wait_until="domcontentloaded")
                expect(page.locator("#timeline .message.assistant")).to_have_count(0)
                projection.assert_three_layers(page, state(), rows,
                                               directory / "s26-partial-browser.json",
                                               navigation_wait="domcontentloaded")
                write(directory / "s26-partial-evidence.json", {
                    "api": final, "store": store(session), "trace": trace(session),
                    "requests": requests, "turn": turn, "input_id": work["input_id"],
                    "attempt": attempt,
                })
                write(directory / "scorecard.json", {
                    "scenario": "s26-partial-disconnect", "selected": 1, "executed": 1,
                    "verdict": "PASS", "turn": turn, "input_id": work["input_id"],
                    "attempt": attempt,
                })
            elif args.scenario == "s27-auth-next-run":
                page = open_page(chrome, session)
                first = submit(page, "invalid credentials")
                work_first = recorded_input(session, first)
                failed, terminals = settled_terminal(session, 1)
                outcome = terminals[0]["outcome"]
                assert outcome["status"] == "failed" and outcome["done_reason"] == "provider_error", outcome
                page.wait_for_function("""turn => window.productStreamItems.some(
                    item => item.type === 'event' && item.event.type === 'done'
                         && item.event.turn_id === turn
                )""", arg=first)
                done_events = [i["event"] for i in page.evaluate("window.productStreamItems")
                               if i["type"] == "event"
                               and i["event"].get("type") == "done"
                               and i["event"].get("turn_id") == first]
                assert len(done_events) == 1, done_events
                # Done.Completed means the turn committed its own outcome,
                # including ProviderError. Done.Failed is a host/commit failure.
                assert done_events[0].get("outcome", "completed") == "completed", done_events
                calls = [r for r in trace(session) if r["type"] == "llm_call_failed"]
                assert len(calls) == 1, calls
                attempts = calls[0].get("attempts") or []
                assert len(attempts) == 1, attempts
                assert attempts[0]["detail"]["error"]["class"] == "auth", attempts
                assert calls[0]["error"]["failure_kind"] == "auth", calls[0]
                assert calls[0]["error"]["retryable"] is False, calls[0]
                assert not [r for r in trace(session) if r["type"] == "llm_call_completed"]
                runs = [r for r in store(session)["session_runs"] if r["run"] == first]
                assert len(runs) == 1 and runs[0]["terminal_kind"] == "failed", runs
                cause = json.loads(runs[0]["terminal_cause_json"])
                assert cause["turn"] == first and cause["outcome"] == {"stopped": "provider_error"}, cause
                requests = control("provider-requests")["requests"]
                assert len(requests) == 1, "auth failure was retried"
                assert no_open_input(failed), failed["pending_turn_inputs"]
                second = submit(page, "fresh valid request")
                assert second != first
                work_second = recorded_input(session, second)
                assert work_second["input_id"] != work_first["input_id"]
                final, terminals = settled_terminal(session, 2)
                assert terminals[-1]["outcome"]["status"] == "completed", terminals
                reply = visible_assistants(final)
                assert len(reply) == 1 and reply[0]["content"]["text"] == "recovered", reply
                assert reply[0]["provenance"]["turn_id"] == second, reply
                assert len(visible_users(final)) == 2, final["transcript"]
                assert no_open_input(final), final["pending_turn_inputs"]
                requests = control("provider-requests")["requests"]
                assert len(requests) == 2 and requests[0] != requests[1], requests
                write(directory / "s27-auth-evidence.json", {
                    "api": final, "failed_api": failed, "store": store(session),
                    "trace": trace(session), "requests": requests,
                    "settlements": final["turn_failure_settlements"],
                    "turns": [first, second],
                    "inputs": [work_first["input_id"], work_second["input_id"]],
                    "done_events": done_events,
                })
                projection.assert_three_layers(page, final, rows,
                                               directory / "s27-browser.json",
                                               navigation_wait="domcontentloaded")
                write(directory / "scorecard.json", {
                    "scenario": "s27-auth-next-run", "selected": 1, "executed": 1,
                    "verdict": "PASS", "turns": [first, second], "requests": len(requests),
                })
            elif args.scenario == "s18-application-timer":
                page = open_page(chrome, session)
                turn = submit(page, "await the application timer")
                work = recorded_input(session, turn)
                suspended = control("restate-suspended")
                assert suspended["invocation"] and suspended["wake_up_time"] > 0, suspended
                write(directory / "s18-suspended.json", suspended)
                running = state()
                assert running["active_turns"], "the turn settled before the timer"
                assert not [r for r in store(session)["session_runs"]
                            if r["run"] == turn and r["terminal_kind"]]
                assert not [r for r in trace(session) if r["type"] == "turn_completed"]
                abort = page.locator("#abort")
                expect(abort).to_be_visible(timeout=90000)
                abort.click()
                final, terminals = settled_terminal(session, 1)
                assert terminals[0]["outcome"]["status"] == "cancelled", terminals
                assert not final["active_turns"], final["active_turns"]
                assert no_open_input(final), final["pending_turn_inputs"]
                assert "timer-elapsed" not in json.dumps(final["transcript"]), "cancelled timer output leaked"
                assert "timer-elapsed" not in page.locator("#timeline").inner_text()
                runs = [r for r in store(session)["session_runs"] if r["run"] == turn]
                assert len(runs) == 1 and runs[0]["terminal_kind"] == "cancelled", runs
                waits = control("waits", {"session_id": session})
                assert waits == [], waits
                requests = control("provider-requests")["requests"]
                assert len(requests) == 1, "cancellation caused a second model request"
                observations = control("observations",
                                       {"session_id": session, "turn_id": turn,
                                        "cursor": cursor})["items"]
                terminal_items = [i for i in observations if i["type"] == "terminal_replacement"]
                assert terminal_items, "late follower never observed the run's terminal"
                late = open_page(chrome, session)
                expect(late.locator("#abort")).to_be_hidden()
                expect(late.locator("#send")).to_be_enabled()
                projection.assert_three_layers(late, final, rows,
                                               directory / "s18-late-browser.json",
                                               navigation_wait="domcontentloaded")
                write(directory / "s18-evidence.json", {
                    "api": final, "store": store(session), "trace": trace(session),
                    "suspended": suspended, "observations": observations,
                    "waits": waits, "requests": requests,
                    "turn": turn, "input_id": work["input_id"],
                })
                write(directory / "scorecard.json", {
                    "scenario": "s18-application-timer", "selected": 1, "executed": 1,
                    "verdict": "PASS", "turn": turn, "input_id": work["input_id"],
                    "wake_up_time": suspended["wake_up_time"],
                })
            else:
                raise AssertionError(f"unknown scenario {args.scenario}")
        finally:
            chrome.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True, choices=(
        "s26-rate-limit", "s26-partial-disconnect", "s27-auth-next-run",
        "s18-application-timer"))
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--remaining-case-seconds", type=float, required=True)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    browser(args)


if __name__ == "__main__":
    main()
