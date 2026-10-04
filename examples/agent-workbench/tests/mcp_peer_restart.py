#!/usr/bin/env python3
"""S28: workbench browser/API/store/native-journal witnesses across MCP restart.

Ports the ten MCP gates of the previous product host, plus reload and unique
result delivery. Fixture answers are never the oracle for native tool outcomes.
"""
import argparse
import hashlib
import json
import sqlite3
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any
from playwright.sync_api import expect, sync_playwright

BADGE = b"workbench workspace badge v1\x00\x01\x02\x03"
STDIO = ["mcp__workspace_stdio__" + name for name in
         ("sample_summary", "elicit_confirmation", "elicit_via_url", "list_host_roots")]
BADGE_TOOL = "mcp__workspace_http__workspace_badge"

class Journey:
    def __init__(self, args):
        self.args = args
        self.root = args.directory
        self.data = self.root / "workbench-data"
        self.session_db = self.data / "lash-sessions/durable-core.db"
        self.gates = []
        self.pages = []
        self.session = self.api("/api/state")["settings"]["session_id"]

    def save(self, name, value):
        (self.root / name).write_text(json.dumps(value, indent=2) + "\n")

    def api(self, path, method="GET", body=None):
        payload = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.args.base_url + path, data=payload, method=method,
                                         headers={"content-type": "application/json"})
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)

    def state(self):
        return self.api("/api/state?" + urllib.parse.urlencode({"session_id": self.session}))

    def controller(self, request):
        print("H6_CONTROL " + json.dumps(request), flush=True)
        receipt = json.loads(sys.stdin.readline())
        assert "error" not in receipt, receipt
        return receipt

    @staticmethod
    def sql(path, query, params=()):
        with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
            db.row_factory = sqlite3.Row
            return [dict(row) for row in db.execute(query, params)]

    def traces(self):
        return [json.loads(line) for line in (self.data / "trace.jsonl").read_text().splitlines() if line.strip()]

    @staticmethod
    def trace_type(record):
        return record.get("type")

    def turns(self):
        return [r for r in self.traces() if r.get("type") == "turn_completed"
                and r.get("context", {}).get("session_id") == self.session]

    def poll(self, predicate):
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(.05)
        raise AssertionError("S28 predicate watchdog")

    def gate(self, checkpoint, layer, rule, condition):
        self.gates.append({"checkpoint": checkpoint, "layer": layer, "rule": rule, "passed": bool(condition)})
        self.save("gates.json", self.gates)
        assert condition, f"{checkpoint}/{layer}: {rule}"

    def dom(self, page):
        return page.locator("#timeline .message").evaluate_all(
            "els => els.map(e => ({role: e.classList.contains('assistant') ? 'assistant' : 'user', text: e.innerText}))")

    def send(self, marker, wait=True):
        before = len(self.turns())
        receipt = self.api("/api/turn?" + urllib.parse.urlencode({"session_id": self.session}), "POST", {"text": marker})
        assert receipt["accepted"] is True and receipt.get("turn_id"), receipt
        if wait:
            self.poll(lambda: len(self.turns()) == before + 1 and not self.state()["active_turns"])
            for page in self.pages:
                expect(page.locator("#timeline .message.assistant")).to_have_count(before + 1, timeout=30000)
        self.save(marker + "-receipt.json", receipt)
        return receipt

    def store(self):
        return {table: self.sql(self.session_db, f"SELECT * FROM {table} WHERE session_id = ?", (self.session,))
                for table in ("graph_nodes", "runtime_turn_commits", "pending_turn_inputs")}

    def capture(self, checkpoint):
        self.save(checkpoint + "-four-layers.json", {"api": self.state(), "store": self.store(),
            "trace": self.traces(), "dom": [self.dom(page) for page in self.pages]})
        for index, page in enumerate(self.pages):
            page.screenshot(path=str(self.root / f"{checkpoint}-{index}.png"), full_page=True)

    def attach(self):
        return self.api("/api/mcp/servers", "POST", {"name": "workspace_http", "url": self.args.mcp_url,
                        "token": "workbench-mcp-fixture-token"})

    def detach(self):
        return self.api("/api/mcp/servers/workspace_http", "DELETE")

    def tool_receipts_for_turn(self, source: str, *, terminal: bool) -> list[dict[str, Any]]:
        run = source
        rows = self.sql(self.session_db, "SELECT session_id, executor_json FROM session_runs WHERE run = ?", (run,))
        if len(rows) != 1:
            raise AssertionError(f"tool Run has no unique retained executor: {run}, {rows}")
        owner_session = rows[0]["session_id"]
        native = self.controller({"action": "tool-journal", "source": source, "run": run,
                                  "session": owner_session, "executor": json.loads(rows[0]["executor_json"])})
        if native.get("work", {}).get("run") != run or native.get("invocation", {}).get("pinned_service_protocol_version") != 7:
            raise AssertionError(f"tool evidence has another original owner/protocol: {native}")
        path = self.root / ("tool-run-" + hashlib.sha256(run.encode()).hexdigest() + ".json")
        path.write_text(json.dumps(native, indent=2) + "\n")
        stored = self.sql(self.session_db,
                          "SELECT request_json, completion_json FROM tool_call_receipts WHERE session_id = ? ORDER BY requested_at_ms, request_key",
                          (owner_session,))
        receipts = []
        for stored_row in stored:
            request = json.loads(stored_row["request_json"])
            owner = request["owner"]
            if owner.get("turn_id", owner.get("run")) != run:
                continue
            if owner.get("session_id") != owner_session or owner.get("kind") not in ("turn", "run"):
                raise AssertionError(f"tool request has another owner: {request}")
            call_id = request["payload"]["call_id"]
            completion = json.loads(stored_row["completion_json"]) if stored_row["completion_json"] else None
            observed = [record for record in self.traces() if self.trace_type(record) == "tool_receipt" and record.get("call_id") == call_id]
            starts = [record for record in observed if record.get("terminal") is None]
            terminals = [record for record in observed if record.get("terminal") is not None]
            outcomes = [outcome for outcome in native["outcomes"] if outcome["call_id"] == call_id]
            if len(starts) != 1 or starts[0].get("name") != request["payload"]["tool_name"]:
                raise AssertionError(f"tool has no unique canonical accepted receipt: {request}, {observed}")
            if terminal:
                if completion is None or completion["owner"] != owner or completion["request_key"] != request["request_key"] or completion["payload_digest"] != request["payload_digest"]:
                    raise AssertionError(f"tool completion changed the admission: {request}, {completion}")
                if len(terminals) != 1 or terminals[0].get("terminal") != "final" or len(outcomes) != 1:
                    raise AssertionError(f"tool has no unique final and native outcome: {observed}, {outcomes}")
                result = completion["result"]
                reference = result.get("presentation")
                material = native["materials"].get(reference["digest"]) if reference else None
                if result.get("event") != "presented" or result.get("call_id") != call_id or material is None or material["reference"] != reference:
                    raise AssertionError(f"tool final has no matching retained presentation: {completion}")
                opener = reference["owner"].get("opener", {})
                if opener.get("session_id") != owner_session or opener.get("turn_id", opener.get("run")) != run:
                    raise AssertionError(f"tool presentation belongs to another original owner: {reference}")
                output = outcomes[0]["output"]
                presentation = json.loads(material["payload"]["text"])
                receipts.append({"call_id": call_id, "request": request, "completion": completion,
                                 "output": output, "presentation": presentation, "traces": observed})
            else:
                receipts.append({"call_id": call_id, "request": request, "trace": starts[0]})
        return receipts

    @staticmethod
    def receipt_tool_name(receipt: dict[str, Any]) -> str:
        return receipt["request"]["payload"]["tool_name"]

    @staticmethod
    def receipt_tool_succeeded(receipt: dict[str, Any]) -> bool:
        return receipt["output"]["outcome"]["status"] == "success"


    @staticmethod
    def walk(value):
        yield value
        if isinstance(value, dict):
            for child in value.values():
                yield from Journey.walk(child)
        elif isinstance(value, list):
            for child in value:
                yield from Journey.walk(child)

    def payload(self, receipt):
        # The native success value retains MCP's own structured content.
        native = receipt["output"]["outcome"]
        assert native["status"] == "success", native
        candidates = [v["structuredContent"] for v in self.walk(native) if isinstance(v, dict) and "structuredContent" in v]
        assert len(candidates) == 1, native
        value = candidates[0]
        assert value.get("$lash_tool_value") == "untrusted_json", value
        return value["value"]

    def run(self):
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            try:
                for _ in range(2):
                    page = browser.new_context(viewport={"width": 1440, "height": 1000}).new_page()
                    page.goto(self.args.base_url + "/?" + urllib.parse.urlencode({"session_id": self.session}))
                    expect(page.locator("#sessionId")).to_have_text(self.session, timeout=30000)
                    self.pages.append(page)
                before = self.state()
                assert not before["messages"] and not self.turns(), before
                peers = {peer["name"]: peer for peer in self.api("/api/mcp/servers")["servers"]}
                assert peers["workspace_stdio"]["connected"] is True, peers
                assert peers["parallel"]["connected"] is False, peers
                depth = self.send("MCP-DEPTH")
                receipts = self.tool_receipts_for_turn(depth["turn_id"], terminal=True)
                outputs = [self.payload(r) for r in receipts]
                self.gate("depth", "dom", "one request and deterministic summary render identically in both contexts",
                    all(len(self.dom(p)) == 2 and "Host-generated summary" in self.dom(p)[-1]["text"] for p in self.pages)
                    and self.dom(self.pages[0]) == self.dom(self.pages[1]))
                store = self.store()
                self.gate("depth", "api/store", "one accepted input and one attributed answer persist",
                    len(self.state()["messages"]) == 2 and len(store["runtime_turn_commits"]) == 1
                    and len(store["pending_turn_inputs"]) == 1 and depth["turn_id"] in json.dumps(store))
                self.gate("depth", "native", "four typed host-owned MCP results commit",
                    [self.receipt_tool_name(r) for r in receipts] == STDIO and len(outputs) == 4
                    and outputs[0] == {"model": "dev/failure-paths", "summary": "Host-generated summary."}
                    and outputs[1] == {"action": "accept", "answer": "yes"}
                    and outputs[2] == {"action": "accept", "completion_notified": True, "elicitation_id": "workbench-demo-url-1"}
                    and outputs[3]["roots"][0]["name"] == "workbench" and outputs[3]["roots"][0]["uri"].startswith("file://"))
                starts = self.tool_receipts_for_turn(depth["turn_id"], terminal=False)
                self.gate("depth", "trace", "four ordered admission/final pairs and URL completion in one Run",
                    len(self.turns()) == 1 and [self.receipt_tool_name(r) for r in starts] == STDIO
                    and [r["call_id"] for r in starts] == [r["call_id"] for r in receipts]
                    and "MCP URL elicitation completed: server=workspace_stdio, elicitation_id=workbench-demo-url-1" in
                        (self.root / "workbench-1.log").read_text())
                self.capture("depth")
                attached = self.attach()
                assert attached["connected"] is True and BADGE_TOOL in attached["tools"], attached
                interrupted = self.send("MCP-RECONNECT", wait=False)
                self.poll(lambda: (self.root / "mcp-gates/badge-entered").exists())
                fault = self.controller({"action": "mcp-restart", "event": interrupted["turn_id"]})
                self.save("controller-faults.json", fault)
                self.gate("restart", "native", "entered peer killed, reaped and restarted on the same address",
                    fault["reaped"] is True and fault["ready"] is True and fault["process"]["pid"] != fault["successor"]["pid"]
                    and fault["successor"]["incarnation"] == fault["process"]["incarnation"] + 1)
                self.poll(lambda: len(self.turns()) == 2 and not self.state()["active_turns"])
                interrupted_results = self.tool_receipts_for_turn(interrupted["turn_id"], terminal=True)
                typed = False
                if len(interrupted_results) == 1:
                    outcome = interrupted_results[0]["output"]["outcome"]
                    if outcome["status"] == "success":
                        typed = True
                    elif outcome["status"] == "failure":
                        failure = outcome["payload"]
                        envelope = failure.get("raw") or {}
                        raw = envelope.get("value", {}) if envelope.get("$lash_tool_value") == "untrusted_json" else {}
                        timeout = (raw.get("kind") == "call_timeout" and isinstance(raw.get("timeout_ms"), int)
                            and raw["timeout_ms"] > 0 and isinstance(raw.get("deadline"), bool) and failure.get("class") == "timeout"
                            and failure.get("code") == ("mcp_call_deadline_exceeded" if raw["deadline"] else "mcp_call_timeout"))
                        lost = (raw.get("kind") == "connection_lost" and raw.get("cause", {}).get("kind") in ("transport_closed", "transport_send")
                            and failure.get("class") == "unavailable" and failure.get("code") == "mcp_connection_lost")
                        typed = failure.get("source") == "plugin" and raw.get("server") == "workspace_http" and (timeout or lost)
                self.gate("restart", "trace", "one admitted badge call resolves or retains its typed transport failure",
                    len(interrupted_results) == 1 and self.receipt_tool_name(interrupted_results[0]) == BADGE_TOOL and typed)
                self.detach()
                self.capture("restart")
                before = len(self.state()["messages"])
                attached = self.attach()
                badge = self.send("MCP-ATTACH")
                results = self.tool_receipts_for_turn(badge["turn_id"], terminal=True)
                detached = self.detach()
                servers = self.api("/api/mcp/servers")["servers"]
                self.send("MCP-DETACHED")
                self.gate("attach", "dom", "attach and post-detach turns render exactly one reply each in both contexts",
                    all(len(self.dom(p)) == before + 4 and "workspace badge came back" in json.dumps(self.dom(p)) for p in self.pages))
                store = self.store()
                self.gate("attach", "api/store", "both requests and attributed answers persist once",
                    len(self.state()["messages"]) == before + 4 and len(store["runtime_turn_commits"]) == 4
                    and len(store["pending_turn_inputs"]) == 4)
                refs = [v["source"] for r in results for v in r["output"].get("view", {}).get("blocks", [])
                        if v.get("type") == "attachment"]
                assert len(refs) == 1, refs
                reference = refs[0]
                ref = reference["attachment_ref"]
                stored = self.sql(self.session_db, "SELECT content FROM attachment_blobs WHERE attachment_id = ?", (ref["id"],))
                with urllib.request.urlopen(self.args.base_url + "/api/attachments/" + urllib.parse.quote(ref["id"], safe="")) as response:
                    retrieved, media = response.read(), response.headers["content-type"]
                self.gate("attach", "native", "connected integration retains one stored binary reference with exact bytes, then detaches",
                    attached["connected"] is True and BADGE_TOOL in attached["tools"] and reference["source"] == "stored"
                    and ref["byte_len"] == len(BADGE) and ref["media_type"] == "application/octet-stream"
                    and [r["content"] for r in stored] == [BADGE] and retrieved == BADGE and media == "application/octet-stream"
                    and detached == {"detached": "workspace_http"} and sorted(v["name"] for v in servers) == ["parallel", "workspace_stdio"])
                requests = [json.loads(line) for line in (self.root / "provider-requests.jsonl").read_text().splitlines()]
                offered = lambda r: json.dumps(r.get("instructions", "")) + json.dumps(r.get("tools", []))
                # Identify each request by its last actual user input, not earlier transcript markers.
                def marker(r):
                    texts = [b.get("text", "") for m in r.get("messages", []) if m.get("role", "").lower() == "user"
                             for b in m.get("blocks", []) if "MCP-" in b.get("text", "")]
                    return texts[-1] if texts else ""
                during = [r for r in requests if "MCP-ATTACH" in marker(r) and "MCP-DETACHED" not in marker(r)]
                after = [r for r in requests if "MCP-DETACHED" in marker(r)]
                self.gate("attach", "trace", "successful tool offered during attach and absent after detach",
                    len(self.turns()) == 4 and [self.receipt_tool_name(r) for r in results] == [BADGE_TOOL]
                    and all(self.receipt_tool_succeeded(r) for r in results) and bool(during) and bool(after)
                    and all("workspace_http" in offered(r) for r in during) and all("workspace_http" not in offered(r) for r in after))
                self.capture("attach")
                before_reload = [self.dom(page) for page in self.pages]
                for page in self.pages:
                    page.reload()
                    expect(page.locator("#sessionId")).to_have_text(self.session)
                    expect(page.locator("#timeline .message")).to_have_count(8, timeout=30000)
                self.gate("reload", "dom/api/store/trace", "session survives reload after peer restart with no duplicate result delivery",
                    [self.dom(page) for page in self.pages] == before_reload and before_reload[0] == before_reload[1]
                    and len(self.state()["messages"]) == 8 and len(self.turns()) == 4
                    and len(self.tool_receipts_for_turn(badge["turn_id"], terminal=True)) == 1)
                self.capture("reload")
                self.save("scorecard.json", {"scenario": "S28", "selected": 1, "executed": 1,
                    "verdict": "PASS", "assertions": len(self.gates), "gates": self.gates})
            finally:
                browser.close()

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--mcp-url", required=True)
    Journey(parser.parse_args()).run()
