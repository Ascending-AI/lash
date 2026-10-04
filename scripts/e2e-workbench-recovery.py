#!/usr/bin/env python3
"""S29: durable acceptance survives a real workbench kill and late browser replay."""
from __future__ import annotations

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import sqlite3
import sys
import threading
import time
import urllib.parse
import urllib.request

QUESTION = "S29-KILL-MID-TURN recover me"
ANSWER = "Recovered the interrupted S29 input"
TOOL_OUTPUT = "S29 retained tool output"


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def serve_provider(port, directory):
    release = threading.Event()
    lock = threading.Lock()
    requests = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"service": "s29-provider"}).encode())

        def do_POST(self):
            if self.path == "/release":
                release.set()
                self.send_response(204)
                self.end_headers()
                return
            assert self.path == "/chat/completions", self.path
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert request["stream"] is True, "fixture requires the production streaming client"
            assert QUESTION in json.dumps(request["messages"]), "provider lost the accepted input"
            with lock:
                requests.append(request)
                write(directory / "provider-requests.json", requests)
            assert release.wait(180), "provider barrier watchdog"
            text = f'<typescript>\nprint({json.dumps(TOOL_OUTPUT)});\nfinish({json.dumps(ANSWER)});\n</typescript>'
            frames = [
                {"id": "s29-recorded-call", "model": request["model"], "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": None}]},
                {"id": "s29-recorded-call", "model": request["model"], "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 100, "completion_tokens": 30, "total_tokens": 130}},
            ]
            try:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for frame in frames:
                    self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
                    self.wfile.flush()
                self.wfile.write(b"data: [DONE]\n\n")
            except (BrokenPipeError, ConnectionResetError):
                # The first physical connection belongs to the killed host.
                pass

    ThreadingHTTPServer(("127.0.0.1", port), Provider).serve_forever()


def browser(args):
    from playwright.sync_api import expect, sync_playwright
    from importlib.util import spec_from_file_location, module_from_spec

    spec = spec_from_file_location("projection", Path(__file__).with_name("workbench-transcript-projection-e2e.py"))
    projection = module_from_spec(spec)
    spec.loader.exec_module(projection)
    directory = args.directory
    database = directory / "workbench-data/lash-sessions/durable-core.db"

    def control(action):
        print("H6_CONTROL " + json.dumps({"action": action, "input": {}}), flush=True)
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
                for table in ("pending_turn_inputs", "session_meta", "graph_nodes", "runtime_turn_commits")}

    def trace(session):
        return [record for line in (directory / "workbench-data/trace.jsonl").read_text().splitlines()
                if line.strip() and (record := json.loads(line)).get("context", {}).get("session_id") == session]

    def poll(label, predicate):
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.05)
        raise AssertionError("watchdog: " + label)

    initial = state()
    session = initial["settings"]["session_id"]
    assert not initial["active_turns"] and not initial["messages"], "S29 session must be fresh"
    url = args.base_url + "/?" + urllib.parse.urlencode({"session_id": session})
    with sync_playwright() as playwright:
        chrome = playwright.chromium.launch()
        pages = [chrome.new_page() for _ in range(2)]
        try:
            for page in pages:
                page.goto(url, wait_until="networkidle")
                expect(page.locator("#prompt")).to_be_visible()
                page.evaluate("""() => {
                    window.s29Replies = [];
                    new MutationObserver(ms => ms.forEach(m => m.addedNodes.forEach(n => {
                        if (n.nodeType === 1 && n.matches?.('.message.assistant'))
                            window.s29Replies.push(n.textContent);
                    }))).observe(document.querySelector('#timeline'), {childList:true});
                }""")
            page = pages[0]
            page.locator("#prompt").fill(QUESTION)
            with page.expect_response(lambda response: urllib.parse.urlsplit(response.url).path == "/api/turn" and response.request.method == "POST") as accepted_response:
                page.locator("#send").click()
            accepted = accepted_response.value.json()
            assert accepted["accepted"] is True, accepted
            poll("actual provider entered", lambda: (directory / "provider-requests.json").exists())
            def admission():
                snapshot = store(session)
                pending = snapshot["pending_turn_inputs"]
                return snapshot if len(pending) == 1 and pending[0]["admitted_run"] else None
            admitted = poll("accepted input durably bound to its Run", admission)
            work = admitted["pending_turn_inputs"][0]
            before_meta = admitted["session_meta"][0]
            assert before_meta["shift_epoch"] > 0 and before_meta["shift_admission_id"]
            before_trace = trace(session)
            assert not [r for r in before_trace if r["type"] == "turn_completed"]
            killed = control("kill-host")
            assert killed["killed"] and killed["reaped"] and all(r["closed"] for r in killed["cleanup"]), killed
            killed_store = store(session)
            assert killed_store["pending_turn_inputs"] == admitted["pending_turn_inputs"], "kill changed durable acceptance"
            for observer in pages:
                expect(observer.locator("#timeline .message.assistant")).to_have_count(0)
                expect(observer.locator("#timeline .message.user")).to_have_count(1)
                assert QUESTION in observer.locator("#timeline").inner_text()
            assert not [r for r in trace(session) if r["type"] == "turn_completed"], "killed host published terminal"
            write(directory / "s29-killed.json", {"accepted": accepted, "store": killed_store, "trace": before_trace, "kill": killed})
            restarted = control("restart")
            assert restarted["protocol"] == 7 and restarted["process"]["pid"] != killed["process"]["pid"], restarted
            assert restarted["process"]["incarnation"] > killed["process"]["incarnation"]
            request = urllib.request.Request(f"http://127.0.0.1:{args.provider_port}/release", data=b"{}", method="POST")
            with urllib.request.urlopen(request, timeout=5):
                pass
            def complete():
                snapshot = state()
                terminals = [r for r in trace(session) if r["type"] == "turn_completed"]
                return snapshot if not snapshot["active_turns"] and len(terminals) == 1 else None
            final = poll("one recovered terminal", complete)
            reply = [r for r in final["transcript"] if r["kind"] == "assistant_reply" and not r["suppressed"]]
            assert len(reply) == 1 and reply[0]["content"]["text"] == ANSWER, reply
            assert reply[0]["provenance"]["is_turn_reply"]
            assert reply[0]["provenance"]["turn_id"] == work["admitted_run"], "recovery changed owning Run"
            for index, observer in enumerate(pages):
                expect(observer.locator("#timeline .message.assistant")).to_have_count(1, timeout=90000)
                assert sum(ANSWER in text for text in observer.evaluate("window.s29Replies")) == 1, "live observer duplicated recovered reply"
                projection.assert_three_layers(observer, final, database, directory / f"s29-observer-{index}.json")
            late = chrome.new_page()
            late.goto(url, wait_until="networkidle")
            projection.assert_three_layers(late, state(), database, directory / "s29-late-browser.json")
            after = store(session)
            meta = after["session_meta"][0]
            assert meta["shift_epoch"] > before_meta["shift_epoch"] or (
                meta["shift_epoch"] == before_meta["shift_epoch"] and meta["shift_admission_id"] == before_meta["shift_admission_id"]), "sealed fence regressed"
            assert len([r for r in final["transcript"] if r["kind"] == "user" and not r["suppressed"]]) == 1, "recovery duplicated input"
            terminals = [r for r in trace(session) if r["type"] == "turn_completed"]
            assert len(terminals) == 1 and terminals[0]["outcome"]["status"] == "completed", terminals
            assert any(TOOL_OUTPUT in str(r.get("output", "")) for r in trace(session) if r["type"] == "exec_code_completed"), "known program output lost"
            requests = json.loads((directory / "provider-requests.json").read_text())
            assert len(requests) >= 2 and all(r == requests[0] for r in requests), "cold replay changed original provider request"
            write(directory / "scorecard.json", {"scenario": "S29", "selected": 1, "executed": 1, "verdict": "PASS", "store": after, "trace": trace(session), "restart": restarted, "provider_requests": requests})
        finally:
            chrome.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--provider-port", type=int, required=True)
    parser.add_argument("--provider", action="store_true")
    parser.add_argument("--base-url")
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    if args.provider:
        serve_provider(args.provider_port, args.directory)
    else:
        assert args.base_url
        browser(args)


if __name__ == "__main__":
    main()
