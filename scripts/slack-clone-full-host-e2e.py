#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["playwright==1.62.0"]
# ///
"""Hermetic browser/full-host acceptance for examples/slack-clone."""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable

from playwright.sync_api import Page, expect, sync_playwright

from slack_clone_thread_evidence import (
    THREAD_ROOT_SEED_PREFIX,
    inherited_prefix_nodes,
    seed_label_line_starts,
    select_thread_root,
)


LAYERS = ("dom", "platform", "bot", "trace")


class LayerFailure(AssertionError):
    def __init__(self, layer: str, message: str):
        super().__init__(f"LAYER {layer} FAIL: {message}")
        self.layer = layer


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--artifact-dir", type=Path, required=True)
    return parser.parse_args()


class Journey:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.base = f"http://127.0.0.1:{args.port}"
        self.bot_base = f"http://127.0.0.1:{args.port + 1}"
        self.state_key = f"127.0.0.1_{args.port}"
        self.data_root = args.state_dir / self.state_key
        self.platform_db = self.data_root / "platform" / "workspace.db"
        self.bot_db = self.data_root / "bot" / "events.db"
        self.session_db = (
            self.data_root / "bot" / "lash" / "lash-sessions" / "durable-core.db"
        )
        self.trace_path = self.data_root / "bot" / "lash" / "trace.jsonl"
        self.bot_log = args.state_dir / "run" / f"bot-{self.state_key}.log"
        self.bot_pid_file = args.state_dir / "run" / f"bot-{self.state_key}.pid"
        self.provider_log = args.state_dir / "provider" / "provider-requests.jsonl"
        self.score: list[dict[str, Any]] = []
        self.pages: dict[str, Page] = {}
        self.channel = ""
        self.bot_user = ""
        self.room_event = ""
        self.kill_event = ""
        self.root_ts = ""
        self.kill_claim_owner = ""
        self.kill_lease_generation = 0
        self.kill_started = 0.0

    def gate(
        self,
        checkpoint: str,
        layer: str,
        description: str,
        condition: bool,
        evidence: str,
    ) -> None:
        row = {
            "checkpoint": checkpoint,
            "layer": layer,
            "assertion": description,
            "verdict": "PASS" if condition else "FAIL",
            "evidence": evidence,
        }
        self.score.append(row)
        self.write_scorecard()
        if not condition:
            raise LayerFailure(layer, f"{checkpoint}: {description}; evidence={evidence}")

    def write_scorecard(self) -> None:
        payload = {
            "schema": "lash.slack-clone.full-host-scorecard.v1",
            "layers": list(LAYERS),
            "assertions": self.score,
            "verdict": "FAIL" if any(row["verdict"] == "FAIL" for row in self.score) else "PASS",
        }
        (self.args.artifact_dir / "scorecard.json").write_text(
            json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        lines = [
            "# Slack-clone deterministic full-host scorecard",
            "",
            "| Checkpoint | Layer | Assertion | Verdict | Evidence |",
            "| --- | --- | --- | --- | --- |",
        ]
        for row in self.score:
            fields = [
                str(row[key]).replace("|", "\\|").replace("\n", " ")
                for key in ("checkpoint", "layer", "assertion", "verdict", "evidence")
            ]
            lines.append("| " + " | ".join(fields) + " |")
        (self.args.artifact_dir / "scorecard.md").write_text(
            "\n".join(lines) + "\n", encoding="utf-8"
        )

    def http_json(
        self,
        url: str,
        *,
        method: str = "GET",
        body: Any | None = None,
        headers: dict[str, str] | None = None,
    ) -> Any:
        data = None
        request_headers = dict(headers or {})
        if body is not None:
            data = json.dumps(body).encode()
            request_headers.setdefault("content-type", "application/json")
        request = urllib.request.Request(url, data=data, method=method, headers=request_headers)
        with urllib.request.urlopen(request, timeout=3) as response:
            raw = response.read()
        return json.loads(raw) if raw else None

    def poll(
        self,
        description: str,
        check: Callable[[], Any],
        *,
        timeout: float = 30,
    ) -> Any:
        deadline = time.monotonic() + timeout
        last: Any = None
        while time.monotonic() < deadline:
            try:
                last = check()
                if last:
                    return last
            except (OSError, sqlite3.Error, urllib.error.URLError, json.JSONDecodeError) as error:
                last = repr(error)
            time.sleep(0.1)
        raise AssertionError(f"timed out polling {description}; last={last!r}")

    def sql(self, path: Path, query: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:
        connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=2)
        connection.row_factory = sqlite3.Row
        try:
            return [dict(row) for row in connection.execute(query, params).fetchall()]
        finally:
            connection.close()

    def platform_rows(self) -> list[dict[str, Any]]:
        return self.sql(
            self.platform_db,
            "SELECT channel_id, ts, author_user_id, bot_id, subtype, text, thread_ts, metadata_json "
            "FROM messages WHERE channel_id = ? ORDER BY ts",
            (self.channel,),
        )

    def outbox_rows(self) -> list[dict[str, Any]]:
        return self.sql(
            self.platform_db,
            "SELECT event_id, payload_json, attempts, delivered_at, abandoned_at "
            "FROM event_outbox ORDER BY id",
        )

    def ledger_rows(self) -> list[dict[str, Any]]:
        return self.sql(
            self.bot_db,
            "SELECT h.event_id, h.channel_id, h.message_ts, h.kind, h.stage, h.input_text, "
            "h.reply_ts, h.detail, h.deliveries, r.thread_ts, r.input_id, r.fork_node_id "
            "FROM handled_events h LEFT JOIN event_routes r USING(event_id) ORDER BY h.first_seen_at, h.event_id",
        )

    def session_snapshot(self) -> dict[str, Any]:
        tables = {
            "pending": "SELECT input_id, session_id, source_key, state, input_json, claim_id, "
            "claim_owner_incarnation_id, claim_session_lease_generation FROM pending_turn_inputs ORDER BY enqueue_seq",
            "nodes": "SELECT session_id, node_id, parent_node_id, generation, node_json FROM graph_nodes "
            "WHERE tombstoned = 0 ORDER BY seq",
            "turns": "SELECT session_id, turn_id, result_json FROM runtime_turn_commits ORDER BY committed_at_ms",
            "meta": "SELECT session_id, relation_kind, parent_session_id, source_session_id, source_node_id "
            "FROM session_meta ORDER BY session_id",
            "lineage": "SELECT session_id, ancestor_session_id, fork_node_id, fork_generation "
            "FROM fork_lineage ORDER BY session_id, ancestor_session_id",
            "usage": "SELECT session_id, model, input_tokens, output_tokens FROM usage_deltas ORDER BY seq",
            "leases": "SELECT session_id, lease_owner_incarnation_id, lease_fencing_token, "
            "lease_expires_at_ms FROM session_execution_leases ORDER BY session_id",
        }
        if not self.session_db.exists():
            return {name: [] for name in tables}
        return {name: self.sql(self.session_db, query) for name, query in tables.items()}

    def traces(self) -> list[dict[str, Any]]:
        if not self.trace_path.exists():
            return []
        records = []
        for line in self.trace_path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                records.append(value)
        return records

    @staticmethod
    def trace_type(record: dict[str, Any]) -> str | None:
        return record.get("type") or record.get("event", {}).get("type")

    @staticmethod
    def trace_session(record: dict[str, Any]) -> str | None:
        context = record.get("context") or record.get("event", {}).get("context") or {}
        return context.get("session_id")

    def turn_traces(self, session: str | None = None) -> list[dict[str, Any]]:
        result = [record for record in self.traces() if self.trace_type(record) == "turn_completed"]
        if session is not None:
            result = [record for record in result if self.trace_session(record) == session]
        return result

    def traces_for_turn(self, turn_id: str, kind: str | None = None) -> list[dict[str, Any]]:
        records = [
            record
            for record in self.traces()
            if (record.get("context") or record.get("event", {}).get("context") or {}).get(
                "turn_id"
            )
            == turn_id
        ]
        if kind is not None:
            records = [record for record in records if self.trace_type(record) == kind]
        return records

    @staticmethod
    def trace_tool_name(record: dict[str, Any]) -> str | None:
        return record.get("name") or record.get("event", {}).get("name")

    @staticmethod
    def trace_tool_succeeded(record: dict[str, Any]) -> bool:
        output = record.get("output") or record.get("event", {}).get("output") or {}
        return output.get("outcome", {}).get("status") == "success"

    def history(self, thread_ts: str | None = None) -> list[dict[str, Any]]:
        query = {"channel": self.channel}
        if thread_ts:
            query["thread_ts"] = thread_ts
        body = self.http_json(f"{self.base}/platform/history?{urllib.parse.urlencode(query)}")
        return body["messages"]

    def dom_rows(self, page: Page, selector: str = "#stream .msg") -> list[dict[str, Any]]:
        return page.locator(selector).evaluate_all(
            "els => els.map(e => ({ts: e.dataset.ts, bot: e.classList.contains('is-bot'), "
            "text: e.querySelector('.msg-body').textContent}))"
        )

    def screenshot(self, checkpoint: str) -> None:
        for name, page in self.pages.items():
            page.screenshot(
                path=self.args.artifact_dir / f"{checkpoint}-{name}.png", full_page=True
            )

    def write_extract(self, checkpoint: str) -> None:
        dom: dict[str, Any] = {}
        for name, page in self.pages.items():
            try:
                dom[name] = self.dom_rows(page)
            except Exception as error:
                dom[name] = {"capture_error": str(error)}
        value = {
            "dom": {
                "main": dom,
                "thread": {
                    name: self.dom_rows(page, "#threadStream .msg")
                    if page.locator("#threadPanel").is_visible()
                    else []
                    for name, page in self.pages.items()
                },
                "thread_badges": {
                    name: page.locator("#stream .thread-badge").all_text_contents()
                    for name, page in self.pages.items()
                },
            },
            "platform_api": self.history() if self.channel else [],
            "platform_db": self.platform_rows() if self.channel else [],
            "outbox": self.outbox_rows(),
            "bot_ledger": self.ledger_rows() if self.bot_db.exists() else [],
            "bot_sessions": self.session_snapshot() if self.session_db.exists() else {},
            "trace": self.traces(),
        }
        (self.args.artifact_dir / f"{checkpoint}-four-layers.json").write_text(
            json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    def join(self, page: Page, name: str) -> None:
        page.goto(self.base, wait_until="domcontentloaded")
        expect(page.locator("#namePicker")).to_be_visible(timeout=10_000)
        page.locator("#nameInput").fill(name)
        page.locator("#nameForm button").click()
        expect(page.locator("#whoami")).to_have_text(name, timeout=10_000)
        expect(page.locator("#text")).to_be_enabled(timeout=10_000)
        page.locator("#channels .channel", has_text="# general").click()
        expect(page.locator("#channelName")).to_have_text("# general", timeout=10_000)

    def send_main(self, page: Page, text: str) -> None:
        page.locator("#text").fill(text)
        page.locator("#send").click()

    def send_thread(self, page: Page, text: str) -> None:
        page.locator("#threadText").fill(text)
        page.locator("#threadComposer button").click()

    def wait_ledger(self, needle: str, stage: str) -> dict[str, Any]:
        def check() -> dict[str, Any] | None:
            for row in self.ledger_rows():
                if needle in (row.get("input_text") or "") and row["stage"] == stage:
                    return row
            return None

        return self.poll(f"ledger {needle} at {stage}", check)

    def checkpoint_baseline(self) -> None:
        health = self.http_json(f"{self.bot_base}/healthz")
        bootstrap = self.http_json(f"{self.base}/platform/bootstrap")
        self.bot_user = health["bot_user_id"]
        self.channel = next(channel["id"] for channel in bootstrap["channels"] if channel["name"] == "general")
        for page in self.pages.values():
            expect(page.locator("#channelId")).to_have_text(self.channel)
            expect(page.locator("#botMention")).to_have_text(f"<@{self.bot_user}>")
            expect(page.locator("#stream .msg")).to_have_count(0)
        self.gate("00-baseline", "dom", "two contexts render the same empty channel and mention", True, "00-baseline-*.png")
        self.gate("00-baseline", "platform", "bootstrap has two humans plus bot and no messages", len(bootstrap["users"]) == 3 and not self.history() and not self.platform_rows(), "00-baseline-four-layers.json")
        self.gate("00-baseline", "bot", "ledger and session graph are empty", not self.ledger_rows() and not self.session_snapshot()["nodes"], "00-baseline-four-layers.json")
        self.gate("00-baseline", "trace", "no turn completed during boot", len(self.turn_traces()) == 0, "00-baseline-four-layers.json")
        self.screenshot("00-baseline")
        self.write_extract("00-baseline")

    def checkpoint_ambient(self) -> None:
        markers = ("FIG1341-AMBIENT-ONE says cobalt", "FIG1341-AMBIENT-TWO says cedar")
        for marker in markers:
            self.send_main(self.pages["ada"], marker)
        for page in self.pages.values():
            expect(page.locator("#stream .msg")).to_have_count(2, timeout=15_000)
            expect(page.locator("#stream .msg.is-bot")).to_have_count(0)
        self.poll("two ambient ledger folds", lambda: len([r for r in self.ledger_rows() if r["stage"] == "folded"]) == 2)
        api_rows = self.history()
        db_rows = self.platform_rows()
        outbox = self.outbox_rows()
        session = self.session_snapshot()
        ambient_ledger = [r for r in self.ledger_rows() if r["stage"] == "folded"]
        outbox_payloads = [json.loads(row["payload_json"]) for row in outbox]
        identities_agree = {
            (row["event_id"], row["event"]["ts"], row["event"]["text"])
            for row in outbox_payloads
        } == {
            (
                row["event_id"],
                row["message_ts"],
                row["input_text"].removeprefix("ada: "),
            )
            for row in ambient_ledger
        }
        timestamps_agree = {row["event"]["ts"] for row in outbox_payloads} == {
            row["ts"] for row in api_rows
        }
        self.gate("02-ambient", "dom", "both humans render each ambient row once and no bot row", all(len(self.dom_rows(p)) == 2 and not any(r["bot"] for r in self.dom_rows(p)) for p in self.pages.values()), "02-ambient-*.png")
        self.gate("02-ambient", "platform", "API, outbox, and database correlate the same two delivered human messages", len(api_rows) == len(db_rows) == 2 and len(outbox) == 2 and timestamps_agree and all(r["delivered_at"] is not None and r["attempts"] == 1 for r in outbox), "02-ambient-four-layers.json")
        self.gate("02-ambient", "bot", "both exact outbox events are folded once, remain pending, and spend no model usage", identities_agree and all(r["deliveries"] == 1 for r in ambient_ledger) and len([r for r in session["pending"] if r["session_id"] == f"channel:{self.channel}"]) == 2 and not session["usage"], "02-ambient-four-layers.json")
        log = self.bot_log.read_text(encoding="utf-8", errors="replace")
        folded_logs = log.count("Disposition::Folded") >= 2 or log.count("Folded {") >= 2
        self.gate("02-ambient", "trace", "ambient input has Folded dispositions and zero completed turns", folded_logs and len(self.turn_traces()) == 0, "bot log + trace")
        self.screenshot("02-ambient")
        self.write_extract("02-ambient")

    def checkpoint_room_mention(self) -> None:
        text = f"<@{self.bot_user}> FIG1341-ROOM-MENTION recall ambient facts and list channels"
        self.send_main(self.pages["brix"], text)
        for page in self.pages.values():
            expect(page.locator("#stream .msg.is-bot")).to_have_count(1, timeout=30_000)
            expect(page.locator("#stream .msg")).to_have_count(4)
        row = self.wait_ledger("FIG1341-ROOM-MENTION", "replied")
        self.room_event = row["event_id"]
        ledger = self.ledger_rows()
        session = self.session_snapshot()
        node_text = json.dumps(session["nodes"], sort_keys=True)
        trace_text = json.dumps(self.traces(), sort_keys=True)
        api_rows = self.history()
        db_rows = self.platform_rows()
        self.gate("03-mention", "dom", "one identical bot reply renders in both contexts", all(len([r for r in self.dom_rows(p) if r["bot"]]) == 1 for p in self.pages.values()) and self.dom_rows(self.pages["ada"])[-1]["text"] == self.dom_rows(self.pages["brix"])[-1]["text"], "03-mention-*.png")
        self.gate("03-mention", "platform", "one API/database bot row carries the originating event metadata", len(api_rows) == len(db_rows) == 4 and sum(r["bot_id"] is not None for r in db_rows) == 1 and self.room_event in (next(r["metadata_json"] for r in db_rows if r["bot_id"] is not None) or ""), "03-mention-four-layers.json")
        twin_ok = any(r["kind"] == "message" and r["stage"] == "ignored" and r["detail"] == "superseded_by_app_mention" for r in ledger)
        drained = session["pending"] and all(
            pending["state"] == "completed" and pending["claim_id"] is None
            for pending in session["pending"]
        )
        self.gate("03-mention", "bot", "mention replied, twin ignored, pending drained, ambient provenance committed", row["reply_ts"] is not None and twin_ok and drained and "FIG1341-AMBIENT-ONE" in node_text and "turn_input" in node_text, "03-mention-four-layers.json")
        mention_turn = f"mention:{self.room_event}"
        starts = self.traces_for_turn(mention_turn, "tool_call_started")
        completions = self.traces_for_turn(mention_turn, "tool_call_completed")
        tool_pair_ok = (
            len(starts) == len(completions) == 1
            and self.trace_tool_name(starts[0]) == "list_channels"
            and self.trace_tool_name(completions[0]) == "list_channels"
            and starts[0].get("call_id") == completions[0].get("call_id")
            and self.trace_tool_succeeded(completions[0])
        )
        self.gate("03-mention", "trace", "one event-correlated channel turn and one successful list_channels start/completion pair completed", len(self.turn_traces(f"channel:{self.channel}")) == 1 and tool_pair_ok, "03-mention-four-layers.json")
        self.screenshot("03-mention")
        self.write_extract("03-mention")

    def open_root_thread(self, page: Page) -> None:
        # By `ts`, never by text: the root's marker also appears in the bot's own
        # channel reply, and a text locator matching two rows is a strict-mode
        # failure rather than a click on the root.
        page.locator(f'#stream .msg[data-ts="{self.root_ts}"]').click()
        expect(page.locator("#threadPanel")).to_be_visible(timeout=10_000)

    def checkpoint_thread(self) -> None:
        root = select_thread_root(self.history(), "FIG1341-AMBIENT-ONE")
        self.root_ts = root["ts"]
        self.open_root_thread(self.pages["brix"])
        self.send_thread(self.pages["brix"], f"<@{self.bot_user}> FIG1341-THREAD-ONE what did the root say?")
        expect(self.pages["brix"].locator("#threadStream .msg.is-bot")).to_have_count(1, timeout=30_000)
        root_recall = self.dom_rows(self.pages["brix"], "#threadStream .msg")[-1]["text"]
        self.open_root_thread(self.pages["ada"])
        for page in self.pages.values():
            expect(page.locator("#threadStream .msg")).to_have_count(3, timeout=15_000)
            expect(page.locator(f'#stream .thread-badge[data-thread-ts="{self.root_ts}"]')).to_have_text("2 replies")
            expect(page.locator("#stream .msg")).to_have_count(4)

        self.send_main(self.pages["ada"], "FIG1341-CHANNEL-AFTER-FORK must stay out of child")
        self.wait_ledger("FIG1341-CHANNEL-AFTER-FORK", "folded")
        self.send_thread(self.pages["brix"], f"<@{self.bot_user}> FIG1341-THREAD-TWO check isolation")
        for page in self.pages.values():
            expect(page.locator("#threadStream .msg.is-bot")).to_have_count(2, timeout=30_000)
            expect(page.locator("#threadStream .msg")).to_have_count(5)
            expect(page.locator(f'#stream .thread-badge[data-thread-ts="{self.root_ts}"]')).to_have_text("4 replies")
            expect(page.locator("#stream .msg")).to_have_count(5)

        session = self.session_snapshot()
        thread_id = f"thread:{self.channel}:{self.root_ts}"
        channel_id = f"channel:{self.channel}"
        thread_nodes = json.dumps([r for r in session["nodes"] if r["session_id"] == thread_id], sort_keys=True)
        channel_nodes = json.dumps([r for r in session["nodes"] if r["session_id"] == channel_id], sort_keys=True)
        inherited_nodes = json.dumps(
            inherited_prefix_nodes(session["nodes"], session["lineage"], thread_id, channel_id),
            sort_keys=True,
        )
        meta = next((r for r in session["meta"] if r["session_id"] == thread_id), None)
        lineage = next((r for r in session["lineage"] if r["session_id"] == thread_id and r["ancestor_session_id"] == channel_id), None)
        provider_requests = [
            json.loads(line)
            for line in self.provider_log.read_text(encoding="utf-8", errors="replace").splitlines()
            if line.strip()
        ]
        thread_requests = [
            json.dumps(request, sort_keys=True)
            for request in provider_requests
            if "FIG1341-THREAD-ONE" in json.dumps(request)
            or "FIG1341-THREAD-TWO" in json.dumps(request)
        ]
        channel_state = json.dumps(
            [
                row
                for name in ("nodes", "pending", "turns")
                for row in session[name]
                if row["session_id"] == channel_id
            ],
            sort_keys=True,
        )
        thread_api = self.history(self.root_ts)
        db_thread = [r for r in self.platform_rows() if r["thread_ts"] is not None]
        self.gate("03T-thread", "dom", "both contexts show parent plus four replies only in the thread and badge=4", all(len(self.dom_rows(p, "#threadStream .msg")) == 5 and len(self.dom_rows(p)) == 5 for p in self.pages.values()), "03T-thread-*.png")
        root_micros = int(self.root_ts.replace(".", ""))
        self.gate("03T-thread", "platform", "thread API returns parent plus four replies while database routes four rows by thread_ts", len(thread_api) == 5 and len(db_thread) == 4 and all(r["thread_ts"] == root_micros for r in db_thread), "03T-thread-four-layers.json")
        self.gate("03T-thread", "bot", "child has retained ancestry, its inherited ancestor chain carries the root and no post-fork channel input, both child requests agree, and channel state excludes both thread inputs", meta is not None and meta["source_session_id"] == channel_id and meta["source_node_id"] and lineage is not None and "FIG1341-AMBIENT-ONE" in channel_nodes and "FIG1341-AMBIENT-ONE" in inherited_nodes and "FIG1341-CHANNEL-AFTER-FORK" not in inherited_nodes and "FIG1341-CHANNEL-AFTER-FORK" not in thread_nodes and len(thread_requests) == 2 and all("FIG1341-AMBIENT-ONE" in request and "FIG1341-CHANNEL-AFTER-FORK" not in request for request in thread_requests) and "FIG1341-THREAD-ONE" not in channel_state and "FIG1341-THREAD-TWO" not in channel_state, "03T-thread-four-layers.json + event-scoped provider requests")
        self.gate("03T-thread", "trace", "two child turns completed and no extra channel turn ran", len(self.turn_traces(thread_id)) == 2 and len(self.turn_traces(channel_id)) == 1, "03T-thread-four-layers.json")
        self.screenshot("03T-thread")
        self.write_extract("03T-thread")

        # FIG-1403: inheritance is not recall. The child's prefix extends past the
        # root — the draining channel turn committed the room mention and the
        # bot's reply too — so the root is only answerable if the host said which
        # message it is.
        seeded_root = f"{THREAD_ROOT_SEED_PREFIX}ada: FIG1341-AMBIENT-ONE says cobalt"
        thread_one_request = next(
            (request for request in thread_requests if "FIG1341-THREAD-ONE" in request), ""
        )
        self.gate("03T-root-recall", "dom", "the first thread reply quotes the thread root rather than the later room mention", "FIG1341-AMBIENT-ONE says cobalt" in root_recall and "FIG1341-ROOM-MENTION" not in root_recall, "03T-thread-*.png")
        marker_rows = [r for r in self.history() if "FIG1341-AMBIENT-ONE" in r["text"]]
        self.gate("03T-root-recall", "platform", "the root is genuinely ambiguous on the platform: its marker also appears on a bot row, and later channel rows follow it", len(marker_rows) > 1 and any(r["is_bot"] for r in marker_rows) and self.history()[-1]["ts"] != self.root_ts, "03T-thread-four-layers.json")
        self.gate("03T-root-recall", "bot", "the child's first request carries the host-seeded thread root exactly once", thread_one_request.count(seeded_root) == 1, "event-scoped provider requests")
        seeded_traces = [r for r in self.traces() if seed_label_line_starts(r)[0]]
        seed_counts = [seed_label_line_starts(r) for r in seeded_traces]
        self.gate("03T-root-recall", "trace", "the seed label is traced only into the child's prompts and always starts its own line", bool(seeded_traces) and all(self.trace_session(r) == thread_id for r in seeded_traces) and all(total == at_line_start for total, at_line_start in seed_counts), "trace.jsonl")

    def checkpoint_redelivery(self) -> None:
        envelope_row = next(r for r in self.outbox_rows() if r["event_id"] == self.room_event)
        before_rows = len(self.platform_rows())
        before_turns = len(self.turn_traces())
        before = next(r for r in self.ledger_rows() if r["event_id"] == self.room_event)
        self.http_json(
            f"{self.bot_base}/slack/events",
            method="POST",
            body=json.loads(envelope_row["payload_json"]),
            headers={"x-slack-retry-num": "1", "x-slack-retry-reason": "http_timeout"},
        )
        row = self.poll("redelivery evidence", lambda: next((r for r in self.ledger_rows() if r["event_id"] == self.room_event and r["deliveries"] >= 2), None))
        self.gate("04-redelivery", "dom", "redelivery creates no new rendered row in either context", all(len(self.dom_rows(p)) == 5 for p in self.pages.values()), "04-redelivery-*.png")
        self.gate("04-redelivery", "platform", "redelivery creates no platform message", len(self.platform_rows()) == before_rows, "04-redelivery-four-layers.json")
        self.gate("04-redelivery", "bot", "the same ledger row increments exactly once while preserving stage and reply identity", row["stage"] == before["stage"] == "replied" and row["reply_ts"] == before["reply_ts"] and row["deliveries"] == before["deliveries"] + 1, "04-redelivery-four-layers.json")
        log = self.bot_log.read_text(encoding="utf-8", errors="replace")
        duplicate_line = re.search(rf"handled {re.escape(self.room_event)}: Duplicate \{{[^\n]+\}}", log)
        self.gate("04-redelivery", "trace", "an event-correlated Duplicate disposition appears and no new turn completes", duplicate_line is not None and len(self.turn_traces()) == before_turns, "bot log + trace")
        self.screenshot("04-redelivery")
        self.write_extract("04-redelivery")

    def read_verified_pid(self) -> tuple[int, int]:
        pid_raw, start_raw = self.bot_pid_file.read_text(encoding="utf-8").split()
        pid, expected_start = int(pid_raw), int(start_raw)
        actual_start = int(Path(f"/proc/{pid}/stat").read_text().split()[21])
        if actual_start != expected_start:
            raise AssertionError("bot PID identity changed before kill")
        return pid, expected_start

    def kill_bot_group(self) -> None:
        pid, _ = self.read_verified_pid()
        if os.getpgid(pid) != pid:
            raise AssertionError(f"bot process {pid} is not its owned process-group leader")
        os.killpg(pid, signal.SIGKILL)
        self.poll("bot process exit", lambda: not Path(f"/proc/{pid}").exists(), timeout=10)

    def restart_bot(self) -> None:
        log_path = self.args.artifact_dir / "05-restart-command.log"
        log = log_path.open("wb")
        process = subprocess.Popen(
            ["bash", str(self.args.repo / "scripts/slack-clone-dev.sh"), "up", "--port", str(self.args.port)],
            cwd=self.args.repo,
            env=os.environ.copy(),
            stdout=log,
            stderr=subprocess.STDOUT,
        )

        def ready() -> bool:
            if process.poll() not in (None, 0):
                raise AssertionError(f"restart command exited {process.returncode}")
            try:
                return self.http_json(f"{self.bot_base}/healthz")["service"] == "slack-clone-bot"
            except urllib.error.URLError:
                return False

        self.poll("restarted bot health", ready, timeout=60)
        code = process.wait(timeout=60)
        log.close()
        if code != 0:
            raise AssertionError(f"restart command exited {code}")

    def checkpoint_kill_restart(self) -> None:
        for page in self.pages.values():
            page.evaluate(
                "window.__fig1341Rows=[]; new MutationObserver(ms => ms.forEach(m => "
                "m.addedNodes.forEach(n => { if (n.nodeType === 1 && n.matches?.('.msg')) "
                "window.__fig1341Rows.push({ts:n.dataset.ts, bot:n.classList.contains('is-bot'), text:n.textContent}); })))"
                ".observe(document.querySelector('#stream'), {childList:true})"
            )
        before_platform_bot_rows = sum(r["bot_id"] is not None for r in self.platform_rows())
        before_dom_bot_rows = len([r for r in self.dom_rows(self.pages["ada"]) if r["bot"]])
        before_turns = len(self.turn_traces())
        self.send_main(self.pages["ada"], f"<@{self.bot_user}> FIG1341-KILL-MID-TURN recover me")
        self.kill_started = time.monotonic()
        accepted = self.wait_ledger("FIG1341-KILL-MID-TURN", "accepted")
        self.kill_event = accepted["event_id"]
        self.poll("provider entered kill gate", lambda: (self.args.state_dir / "provider" / "kill-provider-entered").exists())
        self.kill_bot_group()
        for page in self.pages.values():
            expect(page.locator("#stream .msg.is-bot")).to_have_count(before_dom_bot_rows)
        self.gate("05-killed", "dom", "both pages observe the mention and no reply while bot is down", all(any("FIG1341-KILL-MID-TURN" in r["text"] for r in self.dom_rows(p)) and len([r for r in self.dom_rows(p) if r["bot"]]) == before_dom_bot_rows for p in self.pages.values()), "05-killed-*.png")
        self.gate("05-killed", "platform", "platform remains healthy with the triggering message and no bot reply", self.http_json(f"{self.base}/healthz")["service"] == "slack-clone-platform" and sum(r["bot_id"] is not None for r in self.platform_rows()) == before_platform_bot_rows, "05-killed-four-layers.json")
        killed_ledger = next(r for r in self.ledger_rows() if r["event_id"] == self.kill_event)
        pending = [r for r in self.session_snapshot()["pending"] if r["input_id"] == accepted["input_id"]]
        self.kill_claim_owner = pending[0]["claim_owner_incarnation_id"] if pending else ""
        self.kill_lease_generation = pending[0]["claim_session_lease_generation"] if pending else 0
        self.gate("05-killed", "bot", "ledger is accepted and the claimed admission remains durable", killed_ledger["stage"] == "accepted" and len(pending) == 1 and pending[0]["claim_owner_incarnation_id"], "05-killed-four-layers.json")
        self.gate("05-killed", "trace", "interrupted turn emitted no turn_completed", len(self.turn_traces()) == before_turns, "05-killed-four-layers.json")
        self.screenshot("05-killed")
        self.write_extract("05-killed")

        self.restart_bot()
        self.poll(
            "live-lease deferral",
            lambda: "input_claimed_by_live_lease_generation"
            in self.bot_log.read_text(encoding="utf-8", errors="replace"),
            timeout=15,
        )
        for page in self.pages.values():
            expect(page.locator("#stream .msg.is-bot")).to_have_count(before_dom_bot_rows + 1, timeout=30_000)
        recovered = self.wait_ledger("FIG1341-KILL-MID-TURN", "replied")
        recovery_latency = time.monotonic() - self.kill_started
        recorders = {name: page.evaluate("window.__fig1341Rows") for name, page in self.pages.items()}
        self.gate("05-recovered", "dom", "recovery renders exactly one reply in each live observer", all(sum(1 for row in rows if row["bot"] and "Recovered the interrupted" in row["text"]) == 1 for rows in recorders.values()), "05-recovered-*.png")
        bot_rows = [r for r in self.platform_rows() if r["bot_id"] is not None]
        self.gate("05-recovered", "platform", "platform stores exactly one recovered reply with event metadata", len(bot_rows) == before_platform_bot_rows + 1 and sum(self.kill_event in (r["metadata_json"] or "") for r in bot_rows) == 1, "05-recovered-four-layers.json")
        recovery_log = self.bot_log.read_text(encoding="utf-8", errors="replace")
        deferred_lines = re.findall(rf"(?:recovered|settled deferred) event {re.escape(self.kill_event)}[^\n]*Deferred \{{[^\n]*input_claimed_by_live_lease_generation[^\n]*", recovery_log)
        settled = re.search(rf"settled deferred event {re.escape(self.kill_event)}: Replied \{{[^\n]*source: Turn[^\n]*", recovery_log)
        after_session = self.session_snapshot()
        channel_lease = next(r for r in after_session["leases"] if r["session_id"] == f"channel:{self.channel}")
        self.gate("05-recovered", "bot", f"dead incarnation {self.kill_claim_owner} generation {self.kill_lease_generation} defers, then one retry settles from Turn in {recovery_latency:.2f}s", bool(deferred_lines) and settled is not None and 4.0 <= recovery_latency < 20.0 and recovered["reply_ts"] is not None and channel_lease["lease_fencing_token"] > self.kill_lease_generation and any(str(r["ts"] // 1_000_000) + "." + str(r["ts"] % 1_000_000).zfill(6) == recovered["reply_ts"] for r in bot_rows), "05-recovered-four-layers.json + bot log")
        self.gate("05-recovered", "trace", "restart recovery completes exactly one replacement turn", len(self.turn_traces()) == before_turns + 1, "05-recovered-four-layers.json")
        self.screenshot("05-recovered")
        self.write_extract("05-recovered")

    def checkpoint_mcp_depth(self) -> None:
        before_turns = len(self.turn_traces())
        before_main = len(self.history())
        before_total = len(self.platform_rows())
        expected_bots = len([r for r in self.dom_rows(self.pages["ada"]) if r["bot"]]) + 1
        text = f"<@{self.bot_user}> FIG1341-MCP-DEPTH exercise sampling, form, URL, and roots"
        self.send_main(self.pages["brix"], text)
        for page in self.pages.values():
            expect(page.locator("#stream .msg.is-bot")).to_have_count(expected_bots, timeout=45_000)
        row = self.wait_ledger("FIG1341-MCP-DEPTH", "replied")
        session = self.session_snapshot()
        committed = "\n".join(
            [row["node_json"] for row in session["nodes"]]
            + [row["result_json"] for row in session["turns"]]
        )
        tool_results: dict[str, Any] = {}
        for node in session["nodes"]:
            value = json.loads(node["node_json"])
            conversation = value.get("event", {}).get("Conversation", {})
            for part in conversation.get("parts", []):
                if part.get("kind") == "ToolResult" and part.get("tool_name"):
                    tool_results[part["tool_name"]] = json.loads(part["content"])
        tools = (
            "mcp__slack_clone__sample_summary",
            "mcp__slack_clone__elicit_confirmation",
            "mcp__slack_clone__elicit_via_url",
            "mcp__slack_clone__list_host_roots",
        )
        self.gate("06-mcp-depth", "dom", "one request and one deterministic MCP result render identically", all("Host-generated summary" in self.dom_rows(p)[-1]["text"] and len(self.dom_rows(p)) == before_main + 2 for p in self.pages.values()), "06-mcp-depth-*.png")
        self.gate("06-mcp-depth", "platform", "platform API/database add exactly the MCP request and attributed bot reply", len(self.history()) == before_main + 2 and len(self.platform_rows()) == before_total + 2 and any(row["event_id"] in (r["metadata_json"] or "") for r in self.platform_rows()), "06-mcp-depth-four-layers.json")
        expected_results = (
            tool_results.get(tools[0])
            == {"model": "test/slack-clone-e2e", "summary": "Host-generated summary."}
            and tool_results.get(tools[1]) == {"action": "accept", "answer": "yes"}
            and tool_results.get(tools[2])
            == {
                "action": "accept",
                "completion_notified": True,
                "elicitation_id": "slack-clone-demo-url-1",
            }
            and tool_results.get(tools[3], {}).get("roots", [{}])[0].get("name")
            == "slack-clone"
            and tool_results.get(tools[3], {}).get("roots", [{}])[0]
            .get("uri", "")
            .startswith("file://")
        )
        self.gate("06-mcp-depth", "bot", "one durable turn commits four typed host-owned MCP results", expected_results and all(tool in committed for tool in tools), "06-mcp-depth-four-layers.json")
        mcp_turn = f"mention:{row['event_id']}"
        starts = self.traces_for_turn(mcp_turn, "tool_call_started")
        completions = self.traces_for_turn(mcp_turn, "tool_call_completed")
        started_names = [self.trace_tool_name(record) for record in starts]
        completed_names = [self.trace_tool_name(record) for record in completions]
        paired = {record.get("call_id") for record in starts} == {
            record.get("call_id") for record in completions
        }
        url_log = f"MCP URL elicitation completed: server=slack_clone, elicitation_id=slack-clone-demo-url-1"
        exact_attempts = (
            started_names == list(tools)
            and completed_names == list(tools)
            and paired
            and all(self.trace_tool_succeeded(record) for record in completions)
            and url_log in self.bot_log.read_text(encoding="utf-8", errors="replace")
        )
        self.gate("06-mcp-depth", "trace", "four ordered event-scoped start/success pairs and the URL completion notification occur inside one new turn", len(self.turn_traces()) == before_turns + 1 and exact_attempts, "06-mcp-depth-four-layers.json + bot log")
        self.screenshot("06-mcp-depth")
        self.write_extract("06-mcp-depth")

    def normalized_dom(self, page: Page) -> list[tuple[str, bool, str]]:
        return [(row["ts"], row["bot"], row["text"]) for row in self.dom_rows(page)]

    def normalized_api(self, messages: list[dict[str, Any]]) -> list[tuple[str, bool, str]]:
        return [
            (
                row["ts"],
                bool(row["is_bot"]),
                row["text"].replace(f"<@{self.bot_user}>", "@lashbot"),
            )
            for row in messages
        ]

    def checkpoint_reload(self) -> None:
        before = {name: self.normalized_dom(page) for name, page in self.pages.items()}
        for name, page in self.pages.items():
            page.reload(wait_until="domcontentloaded")
            expect(page.locator("#whoami")).to_have_text(name, timeout=10_000)
            page.locator("#channels .channel", has_text="# general").click()
            expect(page.locator("#channelId")).to_have_text(self.channel, timeout=10_000)
            expect(page.locator("#stream .msg")).to_have_count(len(before[name]), timeout=15_000)
        after = {name: self.normalized_dom(page) for name, page in self.pages.items()}
        api_rows = self.history()
        actual_api_projection = self.normalized_api(api_rows)
        db_rows = [r for r in self.platform_rows() if r["thread_ts"] is None]
        ledger = self.ledger_rows()
        traces = self.traces()

        self.gate("07-reload", "dom", "each context reloads the identical top-level row multiset", after["ada"] == before["ada"] == after["brix"] == before["brix"] == actual_api_projection, "07-reload-*.png")
        self.gate("07-reload", "platform", "platform API and database top-level projections agree", len(api_rows) == len(db_rows) and [r["ts"] for r in api_rows] == [f'{r["ts"] // 1_000_000}.{r["ts"] % 1_000_000:06d}' for r in db_rows], "07-reload-four-layers.json")
        replied = [r for r in ledger if r["stage"] == "replied"]
        platform_bot_rows = [r for r in self.platform_rows() if r["bot_id"] is not None]
        self.gate("07-reload", "bot", "one replied ledger row maps to every platform bot message", len(replied) == len(platform_bot_rows) and {r["reply_ts"] for r in replied} == {f'{r["ts"] // 1_000_000}.{r["ts"] % 1_000_000:06d}' for r in platform_bot_rows}, "07-reload-four-layers.json")
        completed = [r for r in traces if self.trace_type(r) == "turn_completed"]
        self.gate("07-reload", "trace", "completed turn count equals replied durable event count", len(completed) == len(replied), "07-reload-four-layers.json")
        self.screenshot("07-reload")
        self.write_extract("07-reload")

    def run(self) -> None:
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            try:
                for name in ("ada", "brix"):
                    context = browser.new_context(viewport={"width": 1440, "height": 1000})
                    page = context.new_page()
                    self.pages[name] = page
                    self.join(page, name)
                self.checkpoint_baseline()
                self.checkpoint_ambient()
                self.checkpoint_room_mention()
                self.checkpoint_thread()
                self.checkpoint_redelivery()
                self.checkpoint_kill_restart()
                self.checkpoint_mcp_depth()
                self.checkpoint_reload()
            except Exception:
                for name, page in self.pages.items():
                    try:
                        page.screenshot(
                            path=self.args.artifact_dir / f"failure-{name}.png",
                            full_page=True,
                        )
                    except Exception:
                        pass
                try:
                    self.write_extract("failure")
                except Exception as capture_error:
                    (self.args.artifact_dir / "failure-capture-error.txt").write_text(
                        f"{capture_error}\n", encoding="utf-8"
                    )
                raise
            finally:
                browser.close()


def main() -> int:
    args = parse_args()
    journey = Journey(args)
    try:
        journey.run()
    except Exception as error:
        print(error, file=sys.stderr)
        if not isinstance(error, LayerFailure):
            journey.score.append(
                {
                    "checkpoint": "harness",
                    "layer": "harness",
                    "assertion": str(error),
                    "verdict": "FAIL",
                    "evidence": "failure-*.png, failure-four-layers.json, failure-bot-log.txt",
                }
            )
        try:
            if journey.bot_log.exists():
                (args.artifact_dir / "failure-bot-log.txt").write_text(
                    journey.bot_log.read_text(encoding="utf-8", errors="replace"),
                    encoding="utf-8",
                )
            if not (args.artifact_dir / "failure-four-layers.json").exists():
                journey.write_extract("failure")
        except Exception as capture_error:
            print(f"failure evidence capture also failed: {capture_error}", file=sys.stderr)
        journey.write_scorecard()
        return 1
    journey.write_scorecard()
    print(f"scorecard PASS: {args.artifact_dir / 'scorecard.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
