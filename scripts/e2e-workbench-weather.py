#!/usr/bin/env python3
"""Collect the S36 weather row for its separate runbook judge.

This driver asserts transport, identity and terminal structure. Weather facts,
source rounding and conversions retain the runbook's independent judgement.
"""
import argparse
import json
import re
from pathlib import Path
import sqlite3
import time
import urllib.parse
import urllib.request

from playwright.sync_api import expect, sync_playwright

QUESTION = "What is the current weather in Utrecht, Netherlands?"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--model", required=True)
    args = parser.parse_args()
    root = args.directory
    data = root / "data"

    def save(name, value):
        (root / name).write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")

    def state(session=None):
        suffix = "?" + urllib.parse.urlencode({"session_id": session}) if session else ""
        with urllib.request.urlopen(args.base_url + "/api/state" + suffix, timeout=5) as response:
            return json.load(response)

    def trace(session):
        records = [json.loads(line) for line in (data / "trace.jsonl").read_text().splitlines() if line.strip()]
        return [record for record in records if record.get("context", {}).get("session_id") == session]

    def store(session):
        connection = sqlite3.connect(f"file:{data / 'lash-sessions/durable-core.db'}?mode=ro", uri=True)
        connection.row_factory = sqlite3.Row
        try:
            result = {}
            for name in ("graph_nodes", "runtime_turn_commits", "session_meta", "pending_turn_inputs"):
                result[name] = [dict(row) for row in connection.execute(
                    f"SELECT * FROM {name} WHERE session_id = ?", (session,))]
            return result
        finally:
            connection.close()

    initial = state()
    session = initial["settings"]["session_id"]
    assert initial["settings"]["model"] == args.model, initial["settings"]
    assert not [row for row in initial["transcript"] if not row["suppressed"]] and not initial["active_turns"], initial
    initial_store = store(session)
    assert not initial_store["graph_nodes"] and not trace(session), "weather session is not fresh"
    save("00-state.json", initial)
    save("00-store.json", initial_store)
    save("00-identities.json", {"session": session, "requested_model": args.model})
    save("00-trace.json", [])

    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            page = browser.new_page(viewport={"width": 1400, "height": 1000})
            page.goto(args.base_url + "/?" + urllib.parse.urlencode({"session_id": session}), wait_until="domcontentloaded")
            expect(page.locator("#prompt")).to_be_visible(timeout=30000)
            expect(page.locator("#sessionId")).to_have_text(session)
            expect(page.locator("#busyText")).to_have_text("idle")
            page.screenshot(path=str(root / "00-ready.png"), full_page=True)
            page.locator("#prompt").fill(QUESTION)
            page.locator("#send").click()
            expect(page.locator("#busyText")).to_have_text("running", timeout=30000)
            deadline = time.monotonic() + 300
            running = state(session)
            assert running["active_turns"], "UI running without an active API turn"
            save("01-running-state.json", running)
            while True:
                final = state(session)
                records = trace(session)
                terminals = [record for record in records if record["type"] == "turn_completed"]
                if not final["active_turns"] and len(terminals) == 1:
                    assert terminals[0]["outcome"]["status"] == "completed", terminals
                    assert terminals[0]["outcome"]["done_reason"] == "final_value", terminals
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError("weather terminal watchdog")
                page.wait_for_timeout(100)  # Bounded polling of the API/trace predicate above.
            expect(page.locator("#busyText")).to_have_text("idle", timeout=30000)
            expect(page.locator("#timeline .message.user")).to_have_count(1)
            expect(page.locator("#timeline .message.assistant")).to_have_count(1)
            page.locator("#timeline").evaluate("element => element.scrollTop = element.scrollHeight")
            dom = page.locator("#timeline").inner_text()
            page.screenshot(path=str(root / "01-finished.png"), full_page=True)
            save("01-finished-dom.json", {"text": dom, "html": page.locator("#timeline").inner_html()})
            save("01-finished-state.json", final)
            save("01-finished-trace.json", records)
            save("03-crosscheck-store.json", store(session))
            history = [record for record in records if record["type"] in (
                "exec_code_started", "exec_code_completed", "tool_call_started", "tool_call_completed", "llm_call_completed")]
            save("02-execution-history.json", history)
            tools = [record for record in records if record["type"] == "tool_call_completed"]
            assert tools, "no live tool result was captured"
            assert all(re.fullmatch(r"mcp__parallel__web_(search|fetch)(?:_[a-z0-9]+)?", record.get("name", "")) for record in tools), tools
            sources = [record for record in tools if record.get("output", {}).get("outcome", {}).get("status") == "success"]
            assert sources, "no successful live search/fetch result"
            save("02-live-sources.json", sources)
            previous_error = ""
            for record in records:
                if record["type"] == "exec_code_completed":
                    raw_error = record.get("error") or ""
                    if not isinstance(raw_error, str):
                        raw_error = json.dumps(raw_error, sort_keys=True)
                    error = " ".join(raw_error.split())
                    assert not error or error != previous_error, error
                    previous_error = error
            page.screenshot(path=str(root / "03-weather-answer.png"), full_page=True)
            save("row-collection.json", {"row": "S36/workbench-weather", "selected": 1, "executed": 1,
                "verdict": "NeedsJudgement", "runbook": "runbooks/workbench-weather/runbook.md",
                "pending_judge_gates": ["source support", "answer values and units", "rounding and conversions",
                    "placeholder scan", "DOM/API/active durable ancestry fidelity", "served model identity"]})
        except Exception as error:
            page.screenshot(path=str(root / "abort.png"), full_page=True)
            save("abort.json", {"error": str(error), "row": "S36/workbench-weather"})
            raise
        finally:
            browser.close()


if __name__ == "__main__":
    main()
