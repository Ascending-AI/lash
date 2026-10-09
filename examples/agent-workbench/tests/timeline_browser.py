#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["playwright==1.62.0"]
# ///
"""FIG-5086 timeline laws in a real browser.

Two layers, both in headless Chromium:

* every law in `timeline_laws.mjs` runs against the production timeline
  module and the real DOM, with a real MutationObserver counting removals;
* the page itself (`index.html`) runs against a scripted workbench that
  serves the production assets and streams product and observation events
  the scenario pushes, for the laws only the whole page can show: a send's
  row and running state appear before the request answers, and a second
  Enter while it is in flight sends nothing.

Usage: timeline_browser.py [--artifacts DIR] [--screenshots]
"""
from __future__ import annotations

import argparse
import json
import queue
import threading
import time
import urllib.parse
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from playwright.sync_api import sync_playwright

ASSETS = Path(__file__).resolve().parents[1] / "assets"
TESTS = Path(__file__).resolve().parent
SESSION = "workbench-browser-laws"
INCARNATION = "browser-incarnation"


def iso(moment: datetime) -> str:
    return moment.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


class Workbench:
    """A scripted workbench: the production page and assets, a controllable
    `/api/state`, and two NDJSON streams fed by the scenario."""

    def __init__(self) -> None:
        self.product: list[dict] = []
        self.observations: list[dict] = []
        self.transcript: list[dict] = []
        self.active_turns: list[dict] = []
        self.turn_posts: list[dict] = []
        self.turn_delay = 0.0
        self.condition = threading.Condition()
        self.turn_counter = 0
        self.page = ASSETS / "index.html"

    # scenario controls

    def push_product(self, item: dict) -> dict:
        with self.condition:
            event = {"event_id": f"event-{len(self.product) + 1}", "sequence": len(self.product) + 1, **item}
            self.product.append(event)
            self.condition.notify_all()
            return event

    def push_observation(self, kind: str, payload: dict, turn_id: str | None = None) -> str:
        with self.condition:
            cursor = f"cursor-{len(self.observations) + 1:04d}"
            event = {
                "session_id": SESSION,
                "replay_incarnation_id": INCARNATION,
                "revision": len(self.observations) + 1,
                "cursor": cursor,
                **({"turn_id": turn_id} if turn_id else {}),
                **payload,
            }
            item = {"type": kind, "event": event}
            if kind == "terminal_replacement":
                item["cursor"] = cursor
            self.observations.append(item)
            self.condition.notify_all()
            return cursor

    def activity(self, turn_id: str, activity: dict) -> str:
        sequence = len(self.observations) + 1
        return self.push_observation("observation", {
            "type": "turn_activity",
            "activity": {"sequence": sequence, "id": f"activity-{sequence}", "correlation_id": activity.pop("correlation_id", ""), **activity},
        }, turn_id)

    def commit(self, turn_id: str, rows: list[dict]) -> str:
        self.transcript.extend(rows)
        return self.push_observation("terminal_replacement", {"type": "committed", "rows": rows}, turn_id)

    def state(self) -> dict:
        with self.condition:
            return {
                "settings": {
                    "model": "test-model", "model_variant": None, "model_variants": ["", "low"],
                    "session_id": SESSION, "session_name": SESSION, "models": ["test-model"],
                },
                "observation": {"session_id": SESSION, "cursor": self.observations[-1]["event"]["cursor"] if self.observations else "cursor-0000",
                                "turn_index": 0, "usage": {}},
                "product_events": {"cursor": len(self.product), "events": list(self.product)},
                "active_turns": list(self.active_turns),
                "pending_turn_inputs": [],
                "queued_work": [],
                "turn_input_applications": [],
                "turn_failure_settlements": [],
                "pending_approvals": [],
                "transcript": list(self.transcript),
            }


def handler_for(bench: Workbench):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args) -> None:
            pass

        def send_body(self, body: bytes, content_type: str, status: int = 200, headers: dict | None = None) -> None:
            self.send_response(status)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(body)))
            for key, value in (headers or {}).items():
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(body)

        def send_json(self, value, status: int = 200) -> None:
            self.send_body(json.dumps(value).encode(), "application/json", status)

        def stream(self, backlog, headers: dict | None = None) -> None:
            self.send_response(200)
            self.send_header("content-type", "application/x-ndjson")
            self.send_header("transfer-encoding", "chunked")
            for key, value in (headers or {}).items():
                self.send_header(key, value)
            self.end_headers()
            sent = 0
            try:
                while True:
                    with bench.condition:
                        items = backlog()
                        while len(items) <= sent:
                            bench.condition.wait(timeout=0.5)
                            items = backlog()
                    for item in items[sent:]:
                        line = (json.dumps(item) + "\n").encode()
                        self.wfile.write(f"{len(line):x}\r\n".encode() + line + b"\r\n")
                        self.wfile.flush()
                    sent = len(items)
            except (BrokenPipeError, ConnectionResetError):
                return

        def do_GET(self) -> None:  # noqa: N802
            url = urllib.parse.urlparse(self.path)
            query = urllib.parse.parse_qs(url.query)
            if url.path == "/":
                self.send_body(bench.page.read_bytes(), "text/html; charset=utf-8")
            elif url.path == "/assets/timeline.js":
                self.send_body((ASSETS / "timeline.js").read_bytes(), "text/javascript; charset=utf-8")
            elif url.path == "/laws.mjs":
                self.send_body((TESTS / "timeline_laws.mjs").read_bytes(), "text/javascript; charset=utf-8")
            elif url.path == "/blank":
                self.send_body(b"<!doctype html><html><body></body></html>", "text/html; charset=utf-8")
            elif url.path == "/api/state":
                self.send_json(bench.state())
            elif url.path == "/api/events":
                cursor = int(query.get("cursor", ["0"])[0] or 0)
                self.stream(lambda: [{"type": "event", "event": event} for event in bench.product if event["sequence"] > cursor])
            elif url.path == "/api/observations":
                cursor = query.get("cursor", [""])[0]

                def backlog():
                    items = [{"type": "cursor", "cursor": cursor or "cursor-0000"}]
                    return items + [item for item in bench.observations if item["event"]["cursor"] > cursor]
                self.stream(backlog)
            elif url.path == "/api/sessions":
                self.send_json({"sessions": [{"session_id": SESSION, "name": SESSION, "created_at_ms": 0, "last_active_ms": 0, "current": True}],
                                "current_session_id": SESSION})
            elif url.path in ("/api/work", "/api/queued-work", "/api/approvals", "/api/triggers", "/api/accounts"):
                self.send_json([])
            elif url.path == "/api/lash-vm-graphs":
                self.send_json({"graphs": [], "lineage_edges": []})
            else:
                self.send_json({"error": "not scripted"}, 404)

        def do_POST(self) -> None:  # noqa: N802
            url = urllib.parse.urlparse(self.path)
            length = int(self.headers.get("content-length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}")
            if url.path == "/api/turn":
                bench.turn_counter += 1
                turn_id = f"workbench-turn-{bench.turn_counter}"
                bench.turn_posts.append({"at": time.monotonic(), **body})
                # Opening the session and taking the claim come first (the
                # slow part); the workbench then publishes the UI input row and
                # answers once the turn has started.
                time.sleep(bench.turn_delay)
                bench.active_turns = [{"session_id": SESSION, "turn_id": turn_id}]
                bench.push_product({"type": "message", "message": {
                    "id": f"workbench-user:{turn_id}", "role": "user", "text": body["text"],
                    "at": iso(datetime.now(timezone.utc)), "provenance": {"kind": "turn_input", "turn_id": turn_id},
                    **({"client_nonce": body["client_nonce"]} if body.get("client_nonce") else {}),
                }})
                time.sleep(0.2)
                self.send_json({"accepted": True, "queued": False, "turn_id": turn_id})
            else:
                self.send_json({"accepted": True})

    return Handler


def serve(bench: Workbench) -> tuple[ThreadingHTTPServer, str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(bench))
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}"


def row(row_id: str, kind: str, turn_id: str | None, at: datetime, **content) -> dict:
    return {
        "row_id": row_id, "kind": kind, "timestamp": iso(at), "suppressed": None,
        "provenance": {"turn_id": turn_id, "input_id": content.pop("input_id", None), "plugin_id": None,
                       "is_turn_reply": kind == "assistant_reply"},
        "content": {"text": "", "reasoning": [], "attachments": [], "language": None, "code": None,
                    "output": None, "success": None, "error": None, "tools": [], "tools_omitted": 0, **content},
    }


ROW_NODES = """() => [...document.querySelectorAll('#timeline .message, #timeline .reasoning, #timeline .code-block')]
  .filter(node => !node.parentElement.closest('.message, .reasoning, .code-block'))
  .map(node => ({ key: node.dataset.key || null, cls: node.className, hidden: node.hidden, text: node.textContent.slice(0, 60) }))"""

# Row-level nodes: what a reader sees as one row. Laws read them by class so
# they hold for any layout of the page, including the one before FIG-5086.
WATCH = """() => {
  window.__removed = [];
  const rowLevel = node => node.nodeType === 1 && node.matches('.message, .reasoning, .code-block, .tool');
  new MutationObserver(records => {
    for (const record of records) for (const node of record.removedNodes) {
      if (rowLevel(node)) window.__removed.push(node.className + ' ' + node.textContent.slice(0, 40));
    }
  }).observe(document.getElementById('timeline'), { childList: true, subtree: true });
}"""

CAPTURE = """() => {
  window.__identity = {
    user: document.querySelector('#timeline .message.user'),
    reply: document.querySelector('#timeline .message.assistant'),
    reasoning: document.querySelector('#timeline .reasoning'),
    code: document.querySelector('#timeline .code-block'),
    tool: document.querySelector('#timeline .code-block .tool'),
  };
  return Object.fromEntries(Object.entries(window.__identity).map(([key, node]) => [key, Boolean(node)]));
}"""

SAME_NODES = """() => {
  const now = {
    user: document.querySelector('#timeline .message.user'),
    reply: document.querySelector('#timeline .message.assistant'),
    reasoning: document.querySelector('#timeline .reasoning'),
    code: document.querySelector('#timeline .code-block'),
    tool: document.querySelector('#timeline .code-block .tool'),
  };
  return Object.entries(window.__identity).filter(([key, node]) => !node || now[key] !== node).map(([key]) => key);
}"""

ORDER = """() => {
  const event = document.querySelector('#timeline [data-key="msg:host-event-1"]');
  const reply = document.querySelector('#timeline .message.assistant');
  const code = document.querySelector('#timeline .code-block');
  if (!event || !reply || !code) return 'missing rows';
  const before = (a, b) => Boolean(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);
  if (!before(code, event)) return 'the host event renders above the code block that ran before it';
  if (!before(event, reply)) return 'the host event renders below the reply that began after it';
  return 'ok';
}"""


class Laws:
    def __init__(self) -> None:
        self.results: list[tuple[bool, str]] = []

    def check(self, passed: bool, rule: str, detail="") -> None:
        self.results.append((bool(passed), rule + ("" if passed else f" — {detail}")))


def page_laws(browser, page_file: Path, artifacts: Path | None) -> Laws:
    """The laws only the whole page can show, against a scripted workbench."""
    laws = Laws()
    bench = Workbench()
    bench.page = page_file
    bench.turn_delay = 1.5
    server, base = serve(bench)
    try:
        page = browser.new_page(viewport={"width": 1440, "height": 1000})
        errors: list[str] = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.goto(base + "/?session_id=" + SESSION)
        page.wait_for_function("() => document.getElementById('sessionId').textContent === %s" % json.dumps(SESSION))
        page.wait_for_function("() => !document.getElementById('send').disabled")
        page.evaluate(WATCH)

        page.fill("#prompt", "what is the weather?")
        page.evaluate("""() => {
          window.__sendClicked = performance.now();
          window.__userVisible = null;
          window.__busyAtVisible = null;
          new MutationObserver(() => {
            const row = [...document.querySelectorAll('#timeline .message.user')].find(n => n.textContent.includes('what is the weather?'));
            if (row && window.__userVisible === null) {
              window.__userVisible = performance.now();
              window.__busyAtVisible = document.getElementById('busyText').textContent;
            }
          }).observe(document.getElementById('timeline'), { childList: true, subtree: true, characterData: true });
          document.getElementById('send').click();
        }""")
        page.wait_for_timeout(100)
        visible_ms = page.evaluate("() => window.__userVisible === null ? null : window.__userVisible - window.__sendClicked")
        busy_text = page.evaluate("() => document.getElementById('busyText').textContent")
        laws.check(visible_ms is not None and visible_ms < 100, "the sent message is visible within 100ms of send",
                   f"not visible after {visible_ms if visible_ms is not None else '100+'}ms")
        laws.check(busy_text == "running", "the running state shows before the send's request answers", f"pill said {busy_text!r}")
        # The operator types the next message while the send is in flight.
        page.fill("#prompt", "and tomorrow?")
        page.press("#prompt", "Enter")
        page.wait_for_function("() => [...document.querySelectorAll('#timeline .message.user')].some(n => !n.classList.contains('pending'))", timeout=10000)
        page.wait_for_timeout(200)
        laws.check(len(bench.turn_posts) == 1, "a second Enter while the send is in flight sends nothing",
                   f"{len(bench.turn_posts)} turns were sent")
        turn = "workbench-turn-1"
        start = datetime.now(timezone.utc)

        bench.activity(turn, {"type": "turn_started"})
        bench.activity(turn, {"type": "stream_block", "phase": "delta", "kind": "reasoning", "text": "Checking the forecast.", "correlation_id": "r1"})
        bench.activity(turn, {"type": "code_block_started", "language": "typescript", "code": "await weather.forecast()"})
        bench.activity(turn, {"type": "tool_call_started", "call_id": "tc_1", "name": "mcp__parallel__web_search_x", "args": {"q": "weather"}})
        bench.activity(turn, {"type": "tool_call_completed", "call_id": "tc_1", "name": "mcp__parallel__web_search_x", "args": {"q": "weather"},
                              "output": {"outcome": {"status": "success", "payload": {"results": [1, 2]}}}, "duration_ms": 12})
        bench.activity(turn, {"type": "code_block_completed", "language": "typescript", "prints": [{"text": "sunny", "value": "sunny", "projection": {}}],
                              "result": {"kind": "completed"}, "duration_ms": 30,
                              "tool_call_ids": ["tc_1"]})
        page.wait_for_function("() => document.querySelector('#timeline .code-block .tool:not(.pending)')")
        happened = datetime.now(timezone.utc)
        page.wait_for_timeout(300)
        if artifacts:
            page.screenshot(path=str(artifacts / "mid-turn-1440.png"), full_page=True)
        # The host event happened before the reply began; it is published late.
        bench.activity(turn, {"type": "stream_block", "phase": "delta", "kind": "assistant_text", "text": "It will be ", "correlation_id": "p1"})
        page.wait_for_function("() => document.querySelector('#timeline .message.assistant')")
        bench.push_product({"type": "message", "message": {
            "id": "host-event-1", "role": "event", "text": "connected mock account `inbox.work`",
            "at": iso(happened)}})
        bench.activity(turn, {"type": "stream_block", "phase": "delta", "kind": "assistant_text", "text": "sunny.", "correlation_id": "p1"})
        page.wait_for_function("() => document.querySelector('#timeline [data-key=\"msg:host-event-1\"]')")
        page.wait_for_timeout(100)
        if artifacts:
            page.screenshot(path=str(artifacts / "event-before-reply-1440.png"), full_page=True)
        live_order = page.evaluate(ORDER)
        live = page.evaluate(ROW_NODES)
        page.evaluate(CAPTURE)

        committed = [
            row("n-user", "user", turn, start, text="what is the weather?"),
            row("n-reason", "reasoning", turn, start + timedelta(seconds=1), reasoning=["Checking the forecast."]),
            row("n-code", "code_block", turn, start + timedelta(seconds=2), language="typescript", code="await weather.forecast()",
                output="sunny", success=True, tools=[{"operation": "web.search", "status": "success"}]),
            row("n-reply", "assistant_reply", turn, datetime.now(timezone.utc), text="It will be sunny."),
        ]
        bench.commit(turn, committed)
        bench.push_product({"type": "message", "message": {
            "id": f"reply:{turn}", "role": "assistant", "text": "It will be sunny.", "at": iso(datetime.now(timezone.utc)),
            "provenance": {"kind": "turn_output", "turn_id": turn}}})
        bench.active_turns = []
        bench.push_product({"type": "done", "turn_id": turn})
        page.wait_for_function("() => document.getElementById('busyText').textContent === 'idle'", timeout=10000)
        page.wait_for_timeout(3500)  # past the commit grace: nothing may retire now
        if artifacts:
            page.screenshot(path=str(artifacts / "settled-1440.png"), full_page=True)
        settled = page.evaluate(ROW_NODES)
        settled_order = page.evaluate(ORDER)
        laws.check(live_order == "ok" and settled_order == "ok",
                   "a host event made before the reply renders above it, and stays there after settlement",
                   f"live: {live_order}; settled: {settled_order}")
        changed = page.evaluate(SAME_NODES)
        laws.check(not changed, "the user row, reasoning, code block, tool row and reply are the same nodes before and after commit",
                   f"replaced: {changed}")
        removed = page.evaluate("() => window.__removed")
        laws.check(not removed, "no row is removed and re-added during the turn or its settlement", f"removed: {removed}")
        replies = page.locator("#timeline .message.assistant").count()
        laws.check(replies == 1, "exactly one reply after the commit", f"{replies} replies")
        laws.check(not errors, "the page raises no errors", errors)
        if artifacts:
            (artifacts / f"page-laws-{page_file.stem}.json").write_text(json.dumps({"live": live, "settled": settled}, indent=2))
        page.close()
    finally:
        server.shutdown()
    return laws


MODULE_LAWS = """
async () => {
  const { laws } = await import('/laws.mjs');
  const fail = message => { throw new Error(message); };
  function same(left, right) {
    if (left === right) return true;
    if (left instanceof Node || right instanceof Node) return false;
    if (Array.isArray(left) || Array.isArray(right)) {
      return Array.isArray(left) && Array.isArray(right) && left.length === right.length
        && left.every((item, index) => same(item, right[index]));
    }
    if (left && right && typeof left === 'object' && typeof right === 'object') {
      const keys = Object.keys(left);
      return keys.length === Object.keys(right).length && keys.every(key => same(left[key], right[key]));
    }
    return false;
  }
  const assert = {
    ok: (value, message) => value || fail(message || 'expected a truthy value'),
    equal: (actual, expected, message) => actual === expected || fail(`${message || 'not equal'}: ${actual} !== ${expected}`),
    deepEqual: (actual, expected, message) => same(actual, expected)
      || fail(`${message || 'not deep-equal'}: ${JSON.stringify(actual)} vs ${JSON.stringify(expected)}`),
  };
  const env = {
    assert,
    timeline(time) {
      const [list, footer, empty] = ['div', 'div', 'div'].map(tag => document.body.appendChild(document.createElement(tag)));
      return { timeline: createWorkbenchTimeline({ list, footer, empty, hooks: { now: time.now } }), list, footer, empty };
    },
    removals(list) {
      const observer = new MutationObserver(() => {});
      observer.observe(list, { childList: true });
      return () => observer.takeRecords().flatMap(record => [...record.removedNodes]).map(node => node.dataset?.key);
    },
  };
  const results = [];
  for (const law of laws) {
    try {
      await law.run(env);
      results.push([true, law.name, '']);
    } catch (error) {
      results.push([false, law.name, String(error?.message || error)]);
    }
  }
  return results;
}
"""


def module_laws(browser, laws: Laws) -> None:
    """Every law of `timeline_laws.mjs`, over the real DOM."""
    bench = Workbench()
    server, base = serve(bench)
    page = browser.new_page()
    try:
        page.goto(base + "/blank")
        page.add_script_tag(url=base + "/assets/timeline.js")
        for passed, name, detail in page.evaluate(MODULE_LAWS):
            laws.check(passed, "module: " + name, detail)
    finally:
        page.close()
        server.shutdown()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--page", type=Path, default=ASSETS / "index.html",
                        help="the page to hold to the laws (default: the production page)")
    args = parser.parse_args()
    if args.artifacts:
        args.artifacts.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        try:
            laws = page_laws(browser, args.page, args.artifacts)
            module_laws(browser, laws)
        finally:
            browser.close()
    for passed, rule in laws.results:
        print(("ok - " if passed else "not ok - ") + rule)
    failed = sum(not passed for passed, _ in laws.results)
    print(f"{len(laws.results) - failed} passed, {failed} failed")
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
