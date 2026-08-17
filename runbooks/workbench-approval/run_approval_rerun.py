# /// script
# requires-python = ">=3.10"
# dependencies = ["playwright"]
# ///
import json
import os
import sqlite3
import subprocess
import traceback
from pathlib import Path
from urllib.parse import quote, urlparse

from playwright.sync_api import expect, sync_playwright


REPO = Path(__file__).resolve().parents[2]
BASE = os.environ["FIG1117_BASE_URL"]
ARTIFACTS = Path(os.environ["FIG1117_ARTIFACTS"])
DATA_DIR = Path(os.environ["AGENT_WORKBENCH_DATA_DIR"])
TRACE = DATA_DIR / "trace.jsonl"
APPROVALS_DB = DATA_DIR / "approvals.db"
PORT = str(urlparse(BASE).port)
SHAKEOUT_ONLY = os.environ.get("FIG1117_SHAKEOUT_ONLY") == "1"


def save(name, value):
    (ARTIFACTS / name).write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n"
    )


def api(page, path):
    return page.evaluate(
        """async path => {
          const response = await fetch(path);
          if (!response.ok) throw new Error(`${path}: ${response.status}`);
          return await response.json();
        }""",
        path,
    )


def post(page, path):
    return page.evaluate(
        """async path => {
          const response = await fetch(path, {method: "POST"});
          if (!response.ok) throw new Error(`${path}: ${response.status}`);
          const text = await response.text();
          return text ? JSON.parse(text) : null;
        }""",
        path,
    )


def trace_events():
    events = []
    if not TRACE.exists():
        return events
    for line in TRACE.read_text().splitlines():
        if line.strip():
            events.append(json.loads(line))
    return events


def turn_trace(turn_id):
    return [
        event
        for event in trace_events()
        if event.get("context", {}).get("turn_id") == turn_id
    ]


def tool_events(turn_id):
    return [
        event
        for event in turn_trace(turn_id)
        if event.get("type") in {"tool_call_started", "tool_call_completed"}
        and event.get("name") == "workbench_ops_apply_change"
    ]


def approval_ledger_rows(key_id):
    with sqlite3.connect(f"file:{APPROVALS_DB}?mode=ro", uri=True) as connection:
        rows = connection.execute(
            "SELECT key_id, completion_key_json, tool_name, arguments_json, "
            "session_id, requested_at_ms, decision, decided_at_ms "
            "FROM approval_waits WHERE key_id = ? ORDER BY rowid",
            (key_id,),
        ).fetchall()
    return [
        {
            "key": row[0],
            "completion_key": json.loads(row[1]),
            "tool": row[2],
            "arguments": json.loads(row[3]),
            "requesting_session": row[4],
            "requested_at_ms": row[5],
            "decision": row[6],
            "decided_at_ms": row[7],
        }
        for row in rows
    ]


def dom_rows(page):
    return page.locator("#timeline .message").evaluate_all(
        """rows => rows.map((row, index) => ({
          index,
          role: row.classList.contains("user") ? "user" :
                row.classList.contains("assistant") ? "assistant" :
                row.classList.contains("error") ? "error" :
                row.classList.contains("event") ? "event" : "other",
          text: (row.querySelector(".msg-body")?.innerText || "").trim()
        }))"""
    )


def message_projection(state):
    committed = [
        (message["role"], message["id"], message.get("text", ""))
        for message in state["messages"]
    ]
    transcript = [
        (row["message"]["role"], row["message"]["id"], row["message"].get("text", ""))
        for row in state["transcript"]
        if row.get("type") == "message"
    ]
    projected = [
        (event["message"]["role"], event["message"]["id"], event["message"].get("text", ""))
        for event in state["product_events"]["events"]
        if event.get("type") == "message"
    ]
    return {
        "committed": committed,
        "transcript": transcript,
        "product_message_events": projected,
    }


def normalize_semantic_text(value):
    return " ".join(str(value or "").split())


def rendered_message_text(page, role, source):
    return page.evaluate(
        """({role, source}) => {
          const body = document.createElement("div");
          body.className = "msg-body";
          body.style.position = "fixed";
          body.style.left = "-10000px";
          body.style.top = "0";
          body.style.width = "600px";
          document.body.appendChild(body);
          try {
            setMessageBody(body, role, source);
            return body.innerText;
          } finally {
            body.remove();
          }
        }""",
        {"role": role, "source": source},
    )


def semantic_role_rows(page, committed):
    return [
        (
            role,
            normalize_semantic_text(rendered_message_text(page, role, source)),
        )
        for role, _, source in committed
    ]


def screenshot(page, name):
    page.locator("#timeline").evaluate(
        "node => { node.scrollTop = node.scrollHeight; }"
    )
    page.screenshot(path=ARTIFACTS / name, full_page=True)


def assert_approval_checkpoint(
    page, state_name, approvals_name, screenshot_name, expected_args
):
    cards = page.locator(".approval-card")
    expect(cards).to_have_count(1, timeout=180_000)
    state = api(page, "/api/state")
    approvals = api(page, "/api/approvals")
    assert len(state["pending_approvals"]) == len(approvals) == 1
    approval = approvals[0]
    projected = state["pending_approvals"][0]
    for field in (
        "key",
        "tool",
        "arguments",
        "requesting_session",
        "requested_at_ms",
    ):
        assert approval[field] == projected[field]
    assert abs(approval["age_ms"] - projected["age_ms"]) < 2_000
    assert approval["tool"] == "workbench_ops_apply_change"
    assert approval["arguments"] == expected_args
    assert approval["requesting_session"] == state["settings"]["session_id"]
    assert approval["requesting_session"] == state["observation"]["session_id"]
    assert len(state["active_turns"]) == 1
    assert [message["role"] for message in state["messages"]] == ["user"]
    assert page.locator("#sessionId").inner_text() == approval["requesting_session"]
    assert cards.get_attribute("data-approval-key") == approval["key"]
    assert cards.locator(".approval-tool").inner_text() == approval["tool"]
    assert json.loads(cards.locator(".approval-args").inner_text()) == expected_args
    assert approval["requesting_session"] in cards.locator(".approval-meta").inner_text()
    assert approval["key"] in cards.locator(".approval-key").inner_text()
    assert "waiting " in cards.locator(".approval-meta").inner_text()
    rows = dom_rows(page)
    assert [
        row["role"] for row in rows if row["role"] in {"user", "assistant"}
    ] == ["user"]
    save(state_name, state)
    save(approvals_name, approvals)
    screenshot(page, screenshot_name)
    return state, approvals, rows


def assert_terminal(
    page,
    state_name,
    screenshot_name,
    evidence_name,
    expected_args,
    expected_output,
    allow_replayed_starts=False,
):
    expect(page.locator("#busyText")).to_have_text("idle", timeout=180_000)
    expect(page.locator(".approval-card")).to_have_count(0, timeout=10_000)
    state = api(page, "/api/state")
    approvals = api(page, "/api/approvals")
    assert approvals == state["pending_approvals"] == []
    assert state["active_turns"] == []
    rows = dom_rows(page)
    role_rows = [
        (row["role"], normalize_semantic_text(row["text"]))
        for row in rows
        if row["role"] in {"user", "assistant"}
    ]
    assert [role for role, _ in role_rows] == ["user", "assistant"]
    projection = message_projection(state)
    assert projection["committed"] == projection["transcript"]
    assert semantic_role_rows(page, projection["committed"]) == role_rows
    committed_ids = {
        message_id for _, message_id, _ in projection["committed"]
    }
    assert all(
        message_id in committed_ids
        for _, message_id, _ in projection["product_message_events"]
    )
    turn_id = state["messages"][0]["id"].removeprefix("workbench-user:")
    events = turn_trace(turn_id)
    starts = [event for event in events if event.get("type") == "turn_started"]
    completed_turns = [
        event for event in events if event.get("type") == "turn_completed"
    ]
    tools = [
        event
        for event in events
        if event.get("type") == "tool_call_started"
        and event.get("name") == "workbench_ops_apply_change"
    ]
    completed_tools = [
        event
        for event in events
        if event.get("type") == "tool_call_completed"
        and event.get("name") == "workbench_ops_apply_change"
    ]
    assert len(completed_turns) == 1
    assert len(completed_tools) == 1
    if allow_replayed_starts:
        assert starts
        assert tools
    else:
        assert len(starts) == 1
        assert len(tools) == 1
    completed_call_id = completed_tools[0]["call_id"]
    assert {event["call_id"] for event in tools} == {completed_call_id}
    assert all(event["args"] == expected_args for event in tools)
    payload = completed_tools[0]["output"]["outcome"]["payload"]
    assert completed_tools[0]["output"]["outcome"]["status"] == "success"
    assert payload == expected_output
    evidence = {
        "turn_id": turn_id,
        "dom_rows": rows,
        "message_projection": projection,
        "turn_started": starts,
        "turn_completed": completed_turns,
        "tool_call_started": tools,
        "tool_call_completed": completed_tools,
    }
    save(state_name, state)
    save(evidence_name, evidence)
    screenshot(page, screenshot_name)
    return state, evidence


def submit(page, prompt):
    expect(page.locator("#busyText")).to_have_text("idle", timeout=60_000)
    page.locator("#prompt").fill(prompt)
    page.locator("#send").click()


def reset_and_reload(page):
    old_session = api(page, "/api/state")["settings"]["session_id"]
    reset_state = post(page, "/api/reset")
    fresh_session = reset_state["settings"]["session_id"]
    assert fresh_session != old_session
    page.reload(wait_until="domcontentloaded")
    expect(page.locator("#sessionId")).to_have_text(
        fresh_session, timeout=60_000
    )
    expect(page.locator("#busyText")).to_have_text("idle", timeout=60_000)
    hydrated_state = api(page, "/api/state")
    assert hydrated_state["settings"]["session_id"] == fresh_session
    assert (
        hydrated_state["messages"]
        == hydrated_state["active_turns"]
        == hydrated_state["pending_approvals"]
        == []
    )
    return hydrated_state


def restart_and_reload(page):
    page.evaluate(
        """() => {
          window.__fig1117RestartObservations = [];
          window.__fig1117RestartObserver = setInterval(() => {
            window.__fig1117RestartObservations.push({
              at: Date.now(),
              busy: document.querySelector('#busyText')?.textContent || '',
              banner_hidden: document.querySelector('#shellStatus')?.hidden ?? true,
              banner: document.querySelector('#shellStatusText')?.textContent || '',
              session: document.querySelector('#sessionId')?.textContent || ''
            });
          }, 100);
        }"""
    )
    subprocess.run(
        ["just", "agent-workbench-restart", PORT],
        cwd=REPO,
        env=os.environ.copy(),
        check=True,
    )
    observations = page.evaluate(
        """() => {
          clearInterval(window.__fig1117RestartObserver);
          return window.__fig1117RestartObservations;
        }"""
    )
    save("06-restart-observer.json", observations)
    page.reload(wait_until="domcontentloaded")


with sync_playwright() as playwright:
    browser = playwright.chromium.launch(headless=True)
    page = browser.new_page(viewport={"width": 1600, "height": 1050})
    try:
        page.goto(BASE, wait_until="domcontentloaded")
        expect(page.locator("#busyText")).to_have_text("idle", timeout=60_000)
        fresh_state = api(page, "/api/state")
        fresh_approvals = api(page, "/api/approvals")
        if SHAKEOUT_ONLY:
            save("shakeout-initial-state.json", fresh_state)
            save("shakeout-initial-approvals.json", fresh_approvals)
            reset_and_reload(page)
        else:
            assert fresh_state["messages"] == []
            assert fresh_state["pending_approvals"] == fresh_approvals == []
            assert fresh_state["active_turns"] == []
            assert (
                page.locator("#sessionId").inner_text()
                == fresh_state["settings"]["session_id"]
            )
            save("00-state.json", fresh_state)
            save("00-approvals.json", fresh_approvals)
            screenshot(page, "00-fresh-workbench.png")

            approve_args = {"target": "demo-cluster", "change": "enable safe mode"}
            submit(
                page,
                'In one Lashlang cell call ops.apply_change with target "demo-cluster" and change '
                '"enable safe mode", unwrap it with ?, then finish the returned record. Do not merely explain the call.',
            )
            approve_parked_state, _, _ = assert_approval_checkpoint(
                page,
                "01-approve-state.json",
                "01-approve-approvals.json",
                "01-approve-parked.png",
                approve_args,
            )
            approve_turn = approve_parked_state["active_turns"][0]["turn_id"]
            assert len(
                [
                    event
                    for event in tool_events(approve_turn)
                    if event["type"] == "tool_call_started"
                ]
            ) == 1
            assert not [
                event
                for event in tool_events(approve_turn)
                if event["type"] == "tool_call_completed"
            ]
            page.locator(".approval-approve").click()
            _, approve_evidence = assert_terminal(
                page,
                "02-approved-state.json",
                "02-approved-complete.png",
                "02-approved-tool-call.json",
                approve_args,
                {"status": "applied", **approve_args},
            )
            assert approve_evidence["turn_id"] == approve_turn
            reset_and_reload(page)

        restart_args = {"target": "restart-demo", "change": "rotate workers"}
        submit(
            page,
            'In one Lashlang cell call ops.apply_change with target "restart-demo" and change '
            '"rotate workers", unwrap it with ?, then finish its status. Do not merely explain the call.',
        )
        if SHAKEOUT_ONLY:
            expect(page.locator(".approval-card")).to_have_count(
                1, timeout=180_000
            )
            restart_before_state = api(page, "/api/state")
            restart_before = api(page, "/api/approvals")
            restart_before_rows = dom_rows(page)
            assert len(restart_before) == 1
            assert len(restart_before_state["active_turns"]) == 1
            save("shakeout-parked-state.json", restart_before_state)
            save("shakeout-parked-approvals.json", restart_before)
            screenshot(page, "shakeout-parked.png")
        else:
            restart_before_state, restart_before, restart_before_rows = assert_approval_checkpoint(
                page,
                "05-restart-before-state.json",
                "05-restart-before-approvals.json",
                "05-restart-before.png",
                restart_args,
            )
        restart_key = restart_before[0]["key"]
        restart_session = restart_before[0]["requesting_session"]
        restart_turn = restart_before_state["active_turns"][0]["turn_id"]
        restart_ledger_before = approval_ledger_rows(restart_key)
        assert len(restart_ledger_before) == 1
        assert restart_ledger_before[0]["tool"] == "workbench_ops_apply_change"
        assert restart_ledger_before[0]["arguments"] == restart_args
        assert restart_ledger_before[0]["requesting_session"] == restart_session
        assert restart_ledger_before[0]["decision"] is None
        restart_message_ids = [
            message["id"] for message in restart_before_state["messages"]
        ]
        pre_trace = turn_trace(restart_turn)
        pre_tools = tool_events(restart_turn)
        if not SHAKEOUT_ONLY:
            assert len(
                [
                    event
                    for event in pre_tools
                    if event["type"] == "tool_call_started"
                ]
            ) == 1
            assert not [
                event
                for event in pre_tools
                if event["type"] == "tool_call_completed"
            ]

        restart_and_reload(page)
        expect(page.locator("#busyText")).to_have_text("running", timeout=60_000)
        expect(page.locator("#sessionId")).to_have_text(
            restart_session, timeout=60_000
        )
        if SHAKEOUT_ONLY:
            expect(page.locator(".approval-card")).to_have_count(
                1, timeout=60_000
            )
            restart_after_state = api(page, "/api/state")
            restart_after = api(page, "/api/approvals")
            assert len(restart_after) == 1
            assert restart_after[0]["key"] == restart_key
            assert restart_after_state["settings"]["session_id"] == restart_session
            save("shakeout-restart-state.json", restart_after_state)
            save("shakeout-restart-approvals.json", restart_after)
            screenshot(page, "shakeout-restart-parked.png")
        else:
            restart_after_state, restart_after, restart_after_rows = assert_approval_checkpoint(
                page,
                "06-restart-state.json",
                "06-restart-approvals.json",
                "06-restart-parked.png",
                restart_args,
            )
            assert restart_after[0]["key"] == restart_key
            assert restart_after[0]["requesting_session"] == restart_session
            assert restart_after_state["active_turns"][0]["turn_id"] == restart_turn
            assert [
                message["id"] for message in restart_after_state["messages"]
            ] == restart_message_ids
            assert restart_after_rows == restart_before_rows
            after_restart_tools = tool_events(restart_turn)
            after_restart_starts = [
                event
                for event in after_restart_tools
                if event["type"] == "tool_call_started"
            ]
            assert after_restart_starts
            assert {
                (
                    event["call_id"],
                    event["name"],
                    json.dumps(event["args"], sort_keys=True),
                )
                for event in after_restart_starts
            } == {
                (
                    pre_tools[0]["call_id"],
                    pre_tools[0]["name"],
                    json.dumps(pre_tools[0]["args"], sort_keys=True),
                )
            }
            assert not [
                event
                for event in after_restart_tools
                if event["type"] == "tool_call_completed"
            ]
            assert approval_ledger_rows(restart_key) == restart_ledger_before

        if SHAKEOUT_ONLY:
            post(
                page,
                f"/api/approvals/{quote(restart_key, safe='')}/approve",
            )
            expect(page.locator("#busyText")).to_have_text(
                "idle", timeout=180_000
            )
            expect(page.locator(".approval-card")).to_have_count(
                0, timeout=10_000
            )
            shakeout_terminal_state = api(page, "/api/state")
            assert shakeout_terminal_state["active_turns"] == []
            assert shakeout_terminal_state["pending_approvals"] == []
            save("shakeout-terminal-state.json", shakeout_terminal_state)
            save(
                "shakeout-complete.json",
                {
                    "session_id": restart_session,
                    "turn_id": restart_turn,
                    "approval_key": restart_key,
                    "status": "mechanically-complete",
                },
            )
            screenshot(page, "shakeout-complete.png")
        else:
            page.locator(".approval-approve").click()
            _, restart_evidence = assert_terminal(
                page,
                "07-restart-approved-state.json",
                "07-restart-approved.png",
                "07-restart-tool-call.json",
                restart_args,
                {"status": "applied", **restart_args},
                allow_replayed_starts=True,
            )
            assert restart_evidence["turn_id"] == restart_turn
            final_trace = turn_trace(restart_turn)
            terminal_slice = [
                event
                for event in final_trace
                if event.get("type")
                in {
                    "turn_started",
                    "tool_call_started",
                    "tool_call_completed",
                    "turn_completed",
                }
                or event.get("name") == "agent_workbench.approval.decided"
            ]
            terminal_tool_starts = [
                event
                for event in terminal_slice
                if event.get("type") == "tool_call_started"
            ]
            terminal_tool_completions = [
                event
                for event in terminal_slice
                if event.get("type") == "tool_call_completed"
            ]
            assert terminal_tool_starts
            assert len(terminal_tool_completions) == 1
            assert {event["call_id"] for event in terminal_tool_starts} == {
                terminal_tool_completions[0]["call_id"]
            }
            assert all(event["args"] == restart_args for event in terminal_tool_starts)
            assert (
                len(
                    [
                        event
                        for event in terminal_slice
                        if event.get("type") == "turn_completed"
                    ]
                )
                == 1
            )
            restart_ledger_after = approval_ledger_rows(restart_key)
            assert len(restart_ledger_after) == 1
            assert {
                key: value
                for key, value in restart_ledger_after[0].items()
                if key not in {"decision", "decided_at_ms"}
            } == {
                key: value
                for key, value in restart_ledger_before[0].items()
                if key not in {"decision", "decided_at_ms"}
            }
            assert restart_ledger_after[0]["decision"] == "approved"
            assert restart_ledger_after[0]["decided_at_ms"] is not None
            save("07-restart-trace-slice.json", terminal_slice)
            save("07-restart-approval-ledger.json", restart_ledger_after)
            save(
                "runbook-verdict-inputs.json",
                {
                    "approve": approve_evidence,
                    "restart": restart_evidence,
                    "restart_continuity": {
                        "session_id": restart_session,
                        "turn_id": restart_turn,
                        "approval_key": restart_key,
                        "message_ids": restart_message_ids,
                        "dom_rows": restart_before_rows,
                        "pre_restart_trace_event_count": len(pre_trace),
                        "post_terminal_trace_event_count": len(final_trace),
                        "approval_ledger_before": restart_ledger_before,
                        "approval_ledger_after": restart_ledger_after,
                    },
                    "deny": {"verdict": "DEFERRED", "blocked_by": "FIG-1125"},
                },
            )
    except Exception as error:
        save(
            "runbook-driver-error.json",
            {"error": repr(error), "traceback": traceback.format_exc()},
        )
        try:
            save("runbook-driver-last-state.json", api(page, "/api/state"))
            save(
                "runbook-driver-last-approvals.json",
                api(page, "/api/approvals"),
            )
            screenshot(page, "runbook-driver-failure.png")
        except Exception:
            pass
        raise
    finally:
        browser.close()
