# E2E Scenario: Workbench Inbox World — Chat, Web Search, and Mail-Trigger Forwarding

> **Read [../RULES.md](../RULES.md) first** — especially "The browser surface (example
> apps)": tooling, gate discipline, screenshot evidence, real-token designation, and
> boot/teardown ownership. This runbook only adds the scenario-specific parts.

**Purpose.** Drive `examples/agent-workbench` end-to-end through its browser UI with a
real model: a plain chat turn, a `web.search` turn (Tavily), a live mocked-inbox world
(two accounts), the agent operating an inbox through its typed `inbox.<slug>` authority,
and finally a **trigger-driven durable forwarding process** — register a concierge on
`mail.received` for one account, deliver a message into it from the UI, and watch a copy
land in the other account's inbox via a Restate-backed background process.

**Why this matters.** The workbench is the full demo surface: triggers, typed module
authorities, durable processes, and the split app/observation event stream. Forwarding is
the one flow that exercises the whole chain — UI compose → host `mail.received` emission
inside a Restate execution scope → trigger registration match → durable Lashlang process →
`inbox.personal.send` back through the same authority the chat uses. If any link drops,
the message never arrives — a single structural gate covers the chain.

**Real tokens.** OpenRouter for turns, Tavily for `web.search` — both keys from the
environment / repo `.env`. The model's prose and its exact Lashlang are its own; gate on
structural outcomes only.

## Scenario-specific golden rules

1. **The inbox API is the truth.** `GET /api/accounts/{slug}/inbox` decides whether a
   message exists. Inbox cards must agree with it — disagreement is a contract violation
   → Abort/RCA.
2. **Forwarding must be trigger-driven, not chat-driven.** The forwarded copy must appear
   **without any chat turn between compose and arrival** — the concierge process does the
   work. If you have to prompt the agent to make the copy appear, the trigger chain is
   broken: that is the finding, do not "help".
3. **The forwarding processes are durable and visible.** After the trigger fires, the
   process registry (`GET /api/work`, the right rail) must show the concierge run, and
   `GET /api/lashlang-graphs` must know its graph. Invisible background work is a finding.
4. **Instruct outcomes, not Lashlang.** Ask the agent *what to do* ("register a trigger
   that forwards…"); never paste ready-made Lashlang into the chat. The model authoring
   the process is part of what this scenario proves.

## Working material

- **Boot**: `just agent-workbench <port>` from the repo root — it starts Dockerized
  Restate, the workbench, registers the deployment, and exits after printing the URL.
  For an isolated run set `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp>` (golden rule 1 depends
  on an empty world) and `AGENT_WORKBENCH_OPEN=0` (headless boot — no browser open).
  Readiness: `GET /healthz` → 200. **Teardown owns Docker**: `just agent-workbench-down
  <port>` at the end, success or Abort.
- **UI affordances**: the center pane **chat / accounts** tab switch; the chat input and
  send control; the transcript stream; the right rail process registry; the accounts tab's
  account-name field + **add account** button, per-account inbox cards each with a compose
  (title/text) form and per-message delete.
- **Backend truth**: `GET /api/state` (settings + transcript snapshot),
  `GET /api/accounts`, `GET /api/accounts/{slug}/inbox`, `GET /api/work`,
  `GET /api/lashlang-graphs`. `GET /api/work/{process_id}/await` blocks until a
  work item reaches a terminal state (server-side timeout-bounded) and returns
  its outcome plus the authoritative event log reconciled from the durable
  store — the host-facing wait-on-work-item seam, an alternative to polling
  `/api/work` for a terminal row.
- The account compose form posts mail to `/api/accounts/{slug}/messages` as
  `{"title":"<title>","text":"<text>"}`; both `title` and `text` are required.
- **Disk** — two separate trees: `trace.jsonl` and `lashlang-execution.jsonl` live in the
  **data dir** (`AGENT_WORKBENCH_DATA_DIR`, default `.agent-workbench/`) and move with it
  when you override it; the dev script's pid/log/run metadata lives in its own state dir
  (`AGENT_WORKBENCH_RUN_DIR`, default `.agent-workbench/run/`) and stays at the repo
  default unless separately overridden.

## Phase 0 — Boot and pre-flight

Check both keys are present (`OPENROUTER_API_KEY`, `TAVILY_API_KEY`) — a missing key is a
harness gap → Abort. Boot, gate `/healthz`, open the UI, gate the chat pane rendering.
Screenshot `00-fresh.png`.

## Phase 1 — Chat smoke

Send a short prompt (e.g. ask it to answer with a specific word so you have a structural
marker). Gates: the transcript gains your user row and an assistant reply;
`GET /api/state` shows the same two rows. Screenshot `01-chat.png`.

## Phase 2 — Web search turn

Ask a question that requires current web knowledge (so the model must call `web.search`).
Gates: the turn completes with a non-empty answer; `trace.jsonl` (or the rendered tool
activity) shows a `web.search` call for this turn (that is the Lashlang authority name —
the underlying raw tool id is `search_web`, so grep for the former). The answer's correctness is judged,
lightly — the gate is the tool call happening and a grounded reply arriving. Screenshot
`02-web-search.png`.

## Phase 3 — Build the inbox world

Switch to the **accounts** tab. Add two accounts: `Work` and `Personal`. Gates:
`GET /api/accounts` lists both slugs (`work`, `personal`); both cards render with empty
inboxes and compose forms. Screenshot `03-accounts.png`.

Adding an account enqueues a durable tool-catalog refresh, so give the world a beat to
project the `inbox.<slug>` authorities before Phase 4 — poll by asking for the account
list, not by sleeping blind.

## Phase 4 — The agent operates an inbox

Back in the chat tab, ask the agent to send a message into the **work** inbox with a
title you choose (e.g. `Standup notes`). Gates:

1. `GET /api/accounts/work/inbox` contains a message with exactly that title, and the
   work inbox card shows it. This proves the `inbox.work` authority is live in the
   session.
2. **The turn's tool outcome agrees.** Read the send turn in `GET /api/state`
   (transcript) and require its tool row for the send to report `success`. The inbox
   row alone is not the gate: `send` runs only on the leaf attempt route, which
   commits the delivery and declares its `mail.received` emission as one outcome, so a
   row whose turn shows a failed send — or no send call at all — is a finding.
   The emission itself has no surface to check here: nothing subscribes to
   `mail.received` until Phase 5, so this occurrence reserves no delivery and leaves
   no trace, and neither `GET /api/state` nor any other route projects trigger
   occurrences or intent outcomes. What gets accepted later is the *route*, not this
   occurrence: in Phase 6 the concierge's own `inbox.personal.send` runs this same
   leaf attempt route, and gate 2's forwarded copy is what proves the route commits
   its row. That send's declared `mail.received` is the one with a subscriber, and
   gate 3's extra concierge run is it executing.

Screenshot `04-agent-mail.png`.

## Phase 5 — Register the forwarding concierge

Ask the agent (outcome, not code — golden rule 4) to **register a trigger** so that every
message delivered to the `work` inbox is automatically copied into the `personal` inbox,
named something recognizable (e.g. `forwarder`). Gates: the turn completes and the
assistant confirms a registration (judged); the real gate is Phase 6 — a "confirmed"
registration that never fires fails there. Screenshot `05-registered.png`.

## Phase 6 — Fire the trigger from the UI

In the **accounts** tab, use the **work** card's compose form to deliver a message with a
distinctive title (e.g. `Quarterly report`) and text. The form sends the real mail fields
`title` and `text` to `/api/accounts/work/messages`. This is the host emitting `mail.received`
inside a Restate execution scope — **do not touch the chat from here on** (golden rule 2).

Gates, in order:

1. `GET /api/accounts/work/inbox` contains `Quarterly report` (the original landed).
2. Within a generous poll window (~120s), `GET /api/accounts/personal/inbox` gains the
   forwarded copy — a message whose title/text traces to `Quarterly report`.
3. `GET /api/work` shows the concierge process run(s) for this delivery (golden rule 3),
   and the right rail renders them. Expect **possibly more than one run**: the
   concierge's own `inbox.personal.send` re-emits `mail.received` (account `personal`),
   which matches the same subscription and starts a second run that no-ops on the
   account filter — the filter is the loop-breaker. One user delivery, exactly one
   forwarded copy, one **or two** process runs are all healthy; a second **copy** is not.
4. Take a concierge run's `process_id` from `/api/work` and call
   `GET /api/work/{process_id}/await`. This is the wait-on-work-item seam
   (`ProcessWorkDriver::await_terminal`, ADR 0016) — prefer it over re-polling
   `/api/work` for a terminal row. It returns the terminal outcome plus the event
   log reconciled from the durable store (ADR 0017); an already-terminal run
   returns immediately. Gate: a `success` outcome for the forwarding run. The
   server bounds the wait (~120s) and answers 504 for a run still going —
   re-request; do not fall back to polling.
5. `GET /api/lashlang-graphs` includes the concierge's graph;
   `lashlang-execution.jsonl` grew.

Sink freshness is **evidence, never a gate**: when a run appends non-terminal
events, the workbench log (in the dev script's state dir — see Working material)
gains `agent-workbench process event:` lines from the best-effort
`ProcessEventSink` feed (ADR 0017). A quick concierge run may legitimately append
none, absence alone is not a finding, and completion never arrives through it —
that is what gate 4 is for.

Screenshot `06-forwarded.png` showing **both** inbox cards (original + copy) and
`07-process-rail.png` for the registry.

## Phase 7 — Teardown and score

`just agent-workbench-down <port>`; confirm the workbench and the Restate container are
gone. Then fill:

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot | `/healthz` 200, chat pane renders | | `00-fresh.png` |
| Chat turn | user+assistant rows in UI and `/api/state` | | `01-chat.png` |
| Web search | `web.search` call in trace; grounded reply | | `02-web-search.png` |
| Accounts world | `/api/accounts` lists `work`, `personal` | | `03-accounts.png` |
| Agent-sent mail | chosen title in `/api/accounts/work/inbox` | | `04-agent-mail.png` |
| Trigger registration | assistant confirms; fires in Phase 6 | | `05-registered.png` |
| Forwarding (the chain) | copy in `/api/accounts/personal/inbox`, **no chat turn involved** | | `06-forwarded.png` |
| Durable process visibility | concierge in `/api/work` + graphs API | | `07-process-rail.png` |
| Work-item await seam | `/api/work/{id}/await` returns `success` outcome + reconciled events | | API output |
| UI/API agreement throughout | cards match inbox API at every gate | | screenshots + API output |

**Aggregate:** did the chat, the search tool, the typed inbox authorities, and the
mail-trigger → durable-process → inbox-send chain all work end-to-end, with the UI and the
backend in agreement and the forwarding done entirely by the registered process.

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
