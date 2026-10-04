#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["playwright==1.62.0"]
# ///
"""Deterministic DOM/API/SQL law replacing rendered Surfaces A-E."""
from __future__ import annotations

import base64
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
import urllib.request

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
SCENARIOS = ("rendered-surface", "tool-value", "code-failure", "retry-reset-partial", "transcript-projection")
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jX1sAAAAASUVORK5CYII=")


def snapshot(base: str) -> dict:
    with urllib.request.urlopen(base + "/api/state", timeout=5) as response:
        return json.load(response)


def settled(base: str, minimum_replies: int) -> dict:
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        state = snapshot(base)
        if not state["active_turns"] and sum(row["provenance"]["is_turn_reply"] for row in state["transcript"]) >= minimum_replies:
            return state
        time.sleep(0.1)
    raise AssertionError("deterministic turn never quiesced")


def assert_three_layers(page, state: dict, database: Path, artifact: Path, *, navigation_wait: str = "networkidle") -> None:
    rows = state["transcript"]
    session_id = state["settings"]["session_id"]
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        nodes = connection.execute("SELECT node_id, node_json FROM graph_nodes WHERE session_id=? AND tombstoned=0 ORDER BY generation", (session_id,)).fetchall()
    assert len(rows) == len(nodes), "a committed node has neither a row nor a named suppression"
    assert [row["row_id"] for row in rows] == [node_id for node_id, _ in nodes], "retained source order or identity changed"
    for row, (_, node_json) in zip(rows, nodes):
        node = json.loads(node_json)
        assert row["timestamp"] == node["timestamp"], "the host invented a row timestamp"
        assert "ordinal" not in row, "snapshot order became persistent state"
    visible = [row for row in rows if not row["suppressed"]]
    expected = Counter()
    for row in visible:
        count = len(row["content"]["reasoning"]) + (row["kind"] != "reasoning")
        expected[row["row_id"]] += count
    page.wait_for_function("() => !document.querySelector('#send').disabled")
    def scrape_dom() -> list[dict]:
        page.wait_for_function("count => document.querySelectorAll('#timeline [data-transcript-row-id]').length === count", arg=sum(expected.values()))
        dom = page.locator("#timeline [data-transcript-row-id]").evaluate_all("nodes => nodes.map(node => ({id:node.dataset.transcriptRowId, turn:node.dataset.turnId, text:node.textContent}))")
        assert Counter(node["id"] for node in dom) == expected, "settled DOM dropped or duplicated a canonical row"
        for node in dom:
            row = next(row for row in visible if row["row_id"] == node["id"])
            assert node["turn"] == (row["provenance"]["turn_id"] or ""), "DOM lost typed provenance"
            if row["kind"] == "assistant_reply":
                assert row["content"]["text"] in node["text"], "DOM changed the committed reply"
        return dom

    before = scrape_dom()
    page.reload(wait_until=navigation_wait)
    after = scrape_dom()
    assert after == before, "reload changed canonical identity, provenance, content or source order"
    artifact.write_text(json.dumps({"api":state,"sql":nodes,"dom_before":before,"dom_after":after}, indent=2) + "\n")


def main() -> None:
    gate_id = os.environ.get("KILN_GATE_ID", "transcript-" + os.environ.get("GITHUB_RUN_ID", str(os.getpid())))
    port = 61000 + int.from_bytes(hashlib.sha256(gate_id.encode()).digest()[:2], "big") % 3000
    artifact_dir = ROOT / "target/functional-e2e-artifacts/transcript-projection" / gate_id.replace("/", "-")
    artifact_dir.mkdir(parents=True, exist_ok=False)
    report = artifact_dir / "build.json"
    build = ["kiln", "build"] if (ROOT / ".buckconfig.local").exists() else ["bash", "scripts/hermetic-build.sh", "build"]
    subprocess.run([*build, "//examples/agent-workbench:agent-workbench", "--materializations", "final", "--build-report", str(report)], cwd=ROOT, check=True)
    binary = subprocess.check_output(["python3", "tools/buck2/outputs.py", "--report", str(report), "--label", "//examples/agent-workbench:agent-workbench", "--single"], cwd=ROOT, text=True).strip()
    if not Path(binary).is_absolute():
        binary = str(ROOT / binary)
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        try:
            for index, scenario in enumerate(SCENARIOS):
                data = artifact_dir / scenario
                env = {**os.environ, "AGENT_WORKBENCH_BIN":binary, "AGENT_WORKBENCH_DATA_DIR":str(data), "AGENT_WORKBENCH_RUN_DIR":str(data / "run"), "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO":scenario, "AGENT_WORKBENCH_POSTGRES":"0"}
                env.pop("AGENT_WORKBENCH_DATABASE_URL", None)
                address = f"127.0.0.1:{port + index}"
                command = ["bash", "scripts/agent-workbench-dev.sh"]
                page = browser.new_page()
                try:
                    subprocess.run([*command, "up", "--addr", address], cwd=ROOT, env=env, check=True, timeout=300)
                    base = "http://" + address
                    page.goto(base, wait_until="networkidle")
                    page.locator("#attachmentInput").set_input_files({"name":"pixel.png", "mimeType":"image/png", "buffer":PNG})
                    page.locator("#prompt").fill("deterministic canonical transcript question")
                    page.locator("#send").click()
                    state = settled(base, 1)
                    replies = sum(not row["suppressed"] and row["kind"] == "assistant_reply" for row in state["transcript"])
                    page.wait_for_function("count => document.querySelectorAll('#timeline .message.assistant').length === count", arg=replies)
                    before = page.locator("#timeline .message.assistant").count()
                    assert_three_layers(page, state, data / "lash-sessions/durable-core.db", artifact_dir / f"{scenario}.json")
                    assert page.locator("#timeline .message.assistant").count() == before, "quiescence left a provisional assistant copy"
                    assert page.locator("#timeline .message-attachment").count() == 1, "the UI-owned input lost its attachment"
                    if scenario == "transcript-projection":
                        users = [row["content"]["text"] for row in state["transcript"] if not row["suppressed"] and row["kind"] == "user"]
                        assert any(text == "canonical follow-frame task" for text in users), "frame switch dropped the follow task"
                    if scenario == "retry-reset-partial":
                        assert "superseded" not in page.locator("#timeline").inner_text(), "retry retained abandoned output"
                except Exception:
                    page.screenshot(path=str(artifact_dir / f"{scenario}-failure.png"), full_page=True)
                    raise
                finally:
                    page.close()
                    subprocess.run([*command, "down", "--addr", address], cwd=ROOT, env=env, check=True, timeout=60)
        finally:
            browser.close()


if __name__ == "__main__":
    main()
