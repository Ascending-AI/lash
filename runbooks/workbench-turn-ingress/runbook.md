# E2E Scenario: Workbench Turn Ingress — Inject Now vs Queue Next

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> objective-gate, screenshot, token, Abort/RCA, and teardown rules. This runbook adds only
> the turn-ingress scenario.

**Purpose.** Prove that a downstream host can submit user input while a workbench turn is
running with either Lash ingress contract: admit it as an ordinary committed user message at
the in-flight turn's next checkpoint, or preserve it as a draft that commits as a separate
next turn after the current turn settles. The rendered transcript, HTTP receipt and state,
durable row and session graph, trace, and provider evidence must all agree.

**Real tokens.** The browser run uses OpenRouter, so timing and prose are nondeterministic.
Gate on durable input identities, ingress scopes, turn boundaries, and provider/trace
structure rather than exact assistant wording.

## Scenario-specific golden rules

1. **Submit both inputs during one proven running turn.** Gate the running pill, the two
   running-turn cancel controls — `#stop` (**stop after step**) and `#abort` (**abort**);
   the workbench renders no control called "stop turn" — and exactly one `/api/state.active_turns` address before using either
   ingress action. A receipt outside that window does not prove mid-turn behavior.
2. **Intent and render must agree.** **inject now** must render `injected now` and return
   `ingress.scope: "active_turn"` targeting the exact active turn with
   `min_boundary: "after_work"`. **queue next** must render `queued next` and return
   `ingress.scope: "next_turn"`. Each rendered row carries the receipt's `input_id`.
3. **The durable row is authoritative before claim.** Reconcile each receipt against
   `/api/state.pending_turn_inputs` and the session SQLite `pending_turn_inputs` row.
   A claim may make a row disappear from the pending API quickly; in that case use the
   SQLite row plus trace, never timing alone.
4. **Injection is committed, model-visible, and exactly once.** When the turn crosses an
   admitted checkpoint and starts another provider iteration, that next request must contain
   the injected input as an ordinary user message. The same message must appear exactly once
   in the durable session graph, `GET /api/state`, and the rendered transcript, then flow
   through normal assembled history exactly once in later turns. An `ingress.settled`
   trace record naming the input proves settlement but does not replace provider and
   transcript evidence. If
   the input arrives after the turn's last checkpoint, it must become the next turn's first
   committed user input instead of being stranded.
5. **Queued means a full committed turn.** The queued marker must be absent from provider
   requests until the first turn settles, then appear in its own provider request and in
   committed session history. `/api/state` and the rendered transcript must show that
   committed user/assistant pair.
6. **Do not substitute ordinary Send.** The initial turn uses **send**; the two mid-turn
   inputs use their named running-turn controls and `POST /api/turn/input` only.

## Working material

- Boot with a fresh data directory:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. Teardown with
  `just agent-workbench-down <port>`.
- UI: composer, **inject now** (`#injectNow`), **queue next** (`#queueNext`), ingress receipt
  rows, transcript, running pill, and the `#stop` / `#abort` controls. Both ingress buttons are
  **hidden**, not merely disabled, unless a turn is running: they live inside
  `#runningActions`, which the page keeps `hidden` while idle (it unhides it and hides
  `#idleActions` together), so a driver that gates on the `disabled` property never sees them
  at all. Gate on
  presence and visibility, the same way the multi-tab runbook distinguishes `#idleActions`
  from `#runningActions`.
- HTTP truth: `GET /api/state`, `POST /api/turn`, and `POST /api/turn/input` with
  `{ "text": "...", "ingress": "active_turn" | "next_turn" }`.
- Disk truth: `<data-dir>/lash-sessions.db`, table `pending_turn_inputs` — note
  an ordinary composer **send** also lands there, with `ingress {"scope":"next_turn"}`, so a
  one-send run holds three rows, not two — and
  `<data-dir>/trace.jsonl` events named `agent_workbench.turn_input.enqueued` and
  `ingress.settled` (payload `run` and `input_ids`, settled by run and turn per ADR 0101's
  FIG-3927 amendment). An admitted row names its run in `admitted_run` and `admitted_by`;
  there are no claim columns and no `claimed_turn_id` column, and selecting one fails with
  `no such column`.
- Provider truth is the `llm_call_started` trace record, **not** `provider_event` (direction `request`). A
  `provider_event` (direction `request`) record drops its body once the assembled request exceeds the trace's
  inline limit — it then carries `raw_json_omitted_reason: "size_limit"` and a `raw_len`
  with no body — which is routine on a frontier run and makes every marker count read 0. A
  0 count across every iteration means the evidence was omitted, not that the marker was
  absent; re-read the same iterations from `llm_call_started`, which carries the assembled
  messages, before scoring any exactly-once gate.
- ADR 0101 defines current run admission and shift-fence settlement; its store-law
  evidence lives in
  `crates/lash-conformance/src/conformance/runtime_persistence/run_admissions.rs`.

## Phase 0 — Boot and pre-flight

Require `OPENROUTER_API_KEY`; missing credentials are a harness gap → Abort. Boot, gate
readiness, open the browser, and confirm the rendered session id equals
`/api/state.settings.session_id`. Confirm the idle composer offers **send**, not the two
running-turn actions. Screenshot `00-idle.png`.

Choose unique literal markers for this run, for example
`FIG425-NOW-<nonce>` and `FIG425-LATER-<nonce>`, and record them in the scorecard. Avoid
attack-flavored substrings: the FIG-425 judged-fleet finding observed models refusing to echo
tokens containing `INJECT`.

## Phase 1 — Establish an in-flight turn

Send a task that requires this exact multi-iteration shape: first use TypeScript `sleep` for
15 seconds without calling a tool, then call the Parallel web-search MCP tool for one side
of a current comparison, call it for the other side in a later iteration (do not batch the
searches), and only then answer. If provider evidence does not show the initial sleep before
the first tool batch, restart with a fresh turn; this scenario must not rely on a merely
"likely" multi-step task. During that initial sleep, inject before the first search/tool
batch. Poll, do not add an unrelated fixed delay, until all three gates hold:

- the UI shows the running pill and both cancel controls (`#stop`, `#abort`);
- the composer offers **inject now** and **queue next**;
- `/api/state.active_turns` has exactly one address for the rendered session.

Record that address and screenshot `01-running.png`.

## Phase 2 — Inject into the running turn

Enter the injection marker with a short instruction that the final response acknowledge
it, then press **inject now** while capturing `POST /api/turn/input`.

Gate the response: `accepted: true`, non-empty `input_id`, state `pending_active`, and an
`active_turn` ingress whose `turn_id` equals Phase 1 and whose minimum boundary is
`after_work`. Gate the page renders an `injected now` row with the marker and the same
input id in its element data. Reconcile the receipt with `/api/state.pending_turn_inputs`
or, if already claimed, its SQLite row and `agent_workbench.turn_input.enqueued` trace.

Save `02-inject-receipt.json`, the matching disk row as
`02-inject-store.json`, and screenshot `02-injected.png`.

## Phase 3 — Queue a separate next turn

Before the running turn settles, enter the queue marker with a self-contained instruction
and press **queue next**, capturing the response. Gate `accepted: true`, a different
non-empty `input_id`, state `deferred_next_turn`, and `ingress.scope: "next_turn"`.
Gate the page renders a separate `queued next` row carrying that id and marker. Reconcile
against the pending API or SQLite row and trace as in Phase 2.

Save `03-queue-receipt.json`, `03-queue-store.json`, and screenshot `03-queued.png`.

## Phase 4 — Settle and prove both semantics

Poll until the initial turn completes and the queued turn starts and completes; never use
a fixed delay. Gate in this order:

1. the first provider request after the admitted checkpoint — read from `llm_call_started`,
   per Working material — contains the injected marker exactly once as a user message,
   proving the model received it during the initial turn;
   exactly one `ingress.settled` record also places its input id under that turn id and run;
2. the injected marker appears exactly once as a committed user message in the durable
   session graph, `GET /api/state`, and the rendered page; capture all three surfaces and
   require their message text and ordering to agree;
3. the queued marker is absent from every provider request before the initial terminal;
4. a later provider request contains the queued marker and begins only after that terminal;
5. the later queued turn's assembled request contains the earlier injected marker exactly
   once in chronological history plus the queued marker once as its new committed user
   message; the UI and `/api/state` render both, and the queued turn has its own assistant
   result;
6. neither input remains pending, and the SQLite lifecycle rows are completed rather than
   duplicated or abandoned.

Save ordered provider/trace evidence as `04-provider-order.json`, committed store evidence
as `04-session-history.json`, and screenshot `04-two-turns-settled.png` with the latest
transcript rows visible.

## Phase 5 — Queue → view → edit → cancel (FIG-995)

This is a separate named guarantee: **queued inputs can be listed, single-cancelled,
suffix-edited and resubmitted without claiming withdrawal of admitted input**.
Use a fresh running turn with a proven sleep window as in Phase 1. For a bounded,
repeatable referee of queue management alone, boot a fresh workbench with
`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=exec-blocked`: its first cell sleeps,
leaving the durable engine active while the browser manages the queue. Gate
its dev-only startup warning, `warning: agent-workbench development provider
scenario enabled: exec-blocked`, and the served TypeScript cell
`await sleep(600000);` before queueing. The scripted follow-up answer is
`session recovered after break glass`; it is not a live-model assertion. This
fixture substitutes only the provider; the routes, engine and SQLite are real.
It does not certify Phases 2–4's live-provider assertions.

1. In the browser, **queue next** four unique messages in order: `KEEP`, `EDIT`,
   `LATER`, `SINGLE` (with a run nonce). Record each HTTP receipt and input id.
   All four must appear in the pending rail, with **edit** and **cancel input**.
   Reconcile the visible order and text with `/api/state.pending_turn_inputs`
   and SQLite `pending_turn_inputs`, ordered by `enqueue_seq`. Screenshot
   `05-queue-desktop.png` and `05-queue-mobile.png`.
2. Click **cancel input** on `SINGLE`. Capture
   `DELETE /api/turn/input/<input_id>`: its typed receipt must say `cancelled`.
   Only this row disappears; the running turn and the other three stay.
   The row on disk becomes `cancelled`, and the session trace carries
   `agent_workbench.turn_input.cancelled` with that input id.
3. Click **edit** on `EDIT`. The inline editor must contain its text and warn
   that this input **and all later pending inputs** are cancelled, and later
   inputs are not resubmitted. **keep queued** closes the editor without any
   withdrawal. Reopen, change the text to `EDITED`, and click
   **cancel from here & resubmit**. Screenshot `05-edit-warning.png` before
   submission. Capture `POST /api/turn/input/<input_id>/edit` and its typed
   suffix outcomes, followed by `POST /api/turn`. Only an anchor whose outcome
   is `cancelled` may be resubmitted. An `already_admitted` input keeps its run;
   the UI must state that fact and the number of later inputs withdrawn.
4. The resubmission has a fresh input id and next-turn ingress. `KEEP` stays,
   `EDIT` and `LATER` disappear, and `EDITED` appears once. SQLite must retain
   the original rows as `cancelled`, plus a new open `EDITED` row after `KEEP`.
   Reconcile the replacement's input id across page, API and disk. If the
   original had a PNG reference, the replacement must retain that same ref.
5. Reload the page to prove this list is recovered from durable state. Cancel
   `EDITED` through its own **cancel input**, and reconcile page/API/disk again:
   only `KEEP` remains pending behind the running input. A second browser tab
   must also retire confirmed cancellations through the product event lane.
   Screenshot `05-managed-queue.png`; save all receipts, the before/after API
   reads, SQLite lifecycle rows and cancellation trace as `05-management.json`.
6. No cancelled marker may enter committed transcript or provider history.
   Stop the running turn and withdraw `KEEP` before teardown. A network error
   during resubmission must leave the edited text available and state that the
   submission outcome is unknown, rather than silently issuing another send.

## Phase 6 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench is gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot | `/healthz` 200; rendered/API session ids agree | | `00-idle.png` |
| Running window | one active address; both ingress controls visible | | `01-running.png`, `/api/state` |
| Inject intent | UI label and receipt agree on exact active turn + `after_work` | | `02-injected.png`, receipt/store JSON |
| Queue intent | UI label and receipt agree on `next_turn` | | `03-queued.png`, receipt/store JSON |
| Exactly-once injection | input reaches the next initial-turn provider iteration, commits once as a normal user message, and appears once in later assembled history | | `04-provider-order.json`, session store |
| Post-settle dispatch | queue marker first appears after initial terminal in its own turn | | provider/trace ordering |
| Transcript fidelity | injected and queued user messages agree across rendered page, `/api/state`, and durable session graph | | `04-two-turns-settled.png`, history JSON |
| Queue management (FIG-995) | ordered list; one single cancellation; honest suffix withdrawal; fresh replacement; reload and two-tab reconciliation agree with API, SQLite and trace | | `05-management.json`, queue/editor screenshots |
| Claim settlement | both durable input ids settle with no duplicate or stranded row | | SQLite evidence |

**Aggregate:** did the host-selected ingress operation match the rendered intent and
durable evidence, with one checkpoint-committed injection and one later committed full turn?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
