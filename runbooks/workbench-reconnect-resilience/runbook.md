# E2E Scenario: Workbench Reconnect Resilience — Web-Process Replacement Under a Live Turn

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface, polling,
> **don't-blind-yourself-with-the-fault-you-inject**, named-checkpoint screenshot,
> **three-layer cross-check**, real-token, Abort/RCA, and teardown rules. This runbook adds
> only the reconnect-resilience scenario.

**Purpose.** Referee the *client's reconnection machine*. Every other workbench runbook
drives the shell while the web process it talks to stays alive; `workbench-engine-restart`
bounces the Restate container underneath a living web process. Nothing drives the opposite
fault — **the web process dies and is replaced while a browser is attached and a turn is in
flight**, which is what every deploy of the workbench actually does to every connected tab.
That fault is the only one that exercises the reconnection machine end to end:
per-channel availability, the stream-connect watchdog, the NDJSON retry loops, stream
generations, `replay_gap` recovery, single-flight `/api/state` recovery with staleness
rejection, exponential state retry, and the manual "retry now" affordance.

**What the client actually has, and why none of it is currently gated.** All of it lives in
[`index.html`](../../examples/agent-workbench/assets/index.html):

- **Per-channel availability.** Three channels — `state`, `product`, `observation` — each
  `null` (unknown) / `true` / `false`. `shellPhase()` folds them with a `hydrated` flag into
  exactly four phases: `connecting`, `live`, `reconnecting`, `unavailable`. Unknown is
  deliberately *not* down: a stream that has not attached yet is not evidence of an outage,
  which is why an unlanded connect is bounded by a watchdog instead of being left unknown
  forever.
- **The 8s stream-connect watchdog** (`STREAM_CONNECT_TIMEOUT_MS`, `openStream`). It bounds
  *establishment*, not lifetime — a stream request stays open by design, so aborting it would
  tear down a slow but healthy connection. On expiry it marks the channel down rather than
  leaving the shell claiming a liveness nothing evidenced.
- **NDJSON readers with a 900ms retry loop and stream-generation abort**
  (`connectProductEvents`, `connectObservations`, `restartEventStreams`). A stream that ends
  on its own is marked down, not silently retried into a lie; each reconnect carries the
  channel's own cursor, and a superseded generation exits instead of double-rendering.
- **Recovery events.** `resync` on the product stream (emitted by `session_events` in
  [`routes.rs`](../../examples/agent-workbench/src/main_sections/routes.rs) when the
  subscriber lags the broadcast), and `replay_gap` + `terminal_replacement` on the observation
  stream (from `RecoverableChatUpdate` in
  [`recoverable_chat.rs`](../../crates/lash/src/recoverable_chat.rs)). All three funnel into
  the same `recoverFromState` single-flight snapshot recovery. `resident_replacement` takes a
  narrower path: it refetches resident authority while preserving provisional transcript rows.
- **Snapshot recovery with staleness rejection.** `beginStateRecovery` /
  `recoveryResponseIsCurrent` / `snapshotApplication` decide whether a `/api/state` response
  is applied `initial`, applied `authoritative`, or dropped as `ignore`. The retry button, the
  backoff timer and a reset can all be in flight at once, so a response that is no longer the
  newest is dropped outright, and a response *behind* the live projection must never erase
  rows it never saw.
- **Bounded, self-repairing snapshots.** `AbortSignal.timeout(5000)` on every `/api/state`
  fetch, and `scheduleStateRetry` backing off 900ms → 5s. A backend that accepts the
  connection and then blocks on an unavailable engine must converge on the same render as a
  refused connection.
- **A manual "retry now" button** (`#shellStatusRetry`) that forces a snapshot, a stream
  restart, and a work/queued-work refresh.

Every one of those is machinery whose whole purpose is to *hide* an outage from the user. The
failure mode is therefore never a stack trace — it is a **shell that lies**: a banner stuck
at `live` over a dead backend, a banner stuck at `reconnecting` over a healthy one, a
transcript that lost a row on the way through recovery, or a transcript that grew one because
two projection paths both drew the same message.

**Why the web process, not Restate.** Every composer send is submitted to Restate as a
workflow invocation (`submit_user_turn` in `routes.rs`) and executed through the workbench's
registered endpoint. So killing the web process mid-turn does **not** kill the turn: Restate
retains the invocation and drives it against the replacement process. The in-flight turn's
correct outcome is therefore *usually* completion, and the durable stores — not the DOM at
the moment of the kill — decide what "correct" means. This is what makes the scenario worth
running: the client must converge on a truth that changed while it was disconnected.

**Real tokens.** Turns go through OpenRouter; prose, termination style, and how long a turn
stays in flight are all nondeterministic. No exact model wording is an answer key, and
**neither is a recovery path**: whether the reconnect surfaces as `replay_gap`,
`terminal_replacement`, `resync`, or a plain reattach depends on where the kill landed
relative to the turn's commit. The answer key is **convergence** — the phase returns to
`live` without help, and the final transcript is row-identical across all three layers.

## Scenario-specific golden rules

1. **Replace only the web process.** `bash scripts/agent-workbench-dev.sh down --port <p>`
   runs `stop_started_restate`, which `docker rm -f`s the port-derived Restate container and
   destroys the durable invocation this scenario depends on. The narrow restart is
   `bash scripts/agent-workbench-dev.sh restart --port <p>` (equivalently
   `just agent-workbench-restart <p>`): it calls `stop_pid_file` on the recorded PID —
   `SIGTERM` to the process group, `SIGKILL` after 15s — then `run_up`, whose `ensure_restate`
   sees the running container and reuses it. Gate that the container **id and start time are
   unchanged** across the restart and that the workbench **PID changed**. A `down`/`up` pair,
   or an unchanged PID, voids the phase. Per [../RULES.md](../RULES.md), `restart` does not
   inherit `AGENT_WORKBENCH_DATA_DIR` / `AGENT_WORKBENCH_RUN_DIR` — export both, or the
   replacement process silently picks a different durable directory and the scenario becomes
   a fresh-session test wearing a restart's clothes.
2. **Gate on the phase, and read it where the phase actually lives.** The phase is not a
   single element. `#shellStatus` is hidden **exactly** when the phase is `live`, and its
   `#shellStatusText` carries `workbench unreachable — retrying` (`unavailable`) or
   `reconnecting — showing the last known state` / `live updates paused — reconnecting`
   (`reconnecting`). The `#busyText` pill carries the phase name verbatim while degraded
   (`connecting` / `reconnecting` / `unavailable`) and switches to `idle` / `running` only
   when live. Assert **both** — banner visibility and pill text — at every phase gate; either
   one alone cannot distinguish `live` from a banner that failed to render.
3. **The phase must actually move, and the window is short.** A run in which the phase never
   left `live` proves nothing about reconnection. That is a **void phase, not a pass** —
   retry it, and only score a restart whose phase transition was observed. Do not lengthen an
   outage by adding a sleep. Observe it properly instead: `restart` completes in about two
   seconds on a warm build, so per [../RULES.md](../RULES.md) the phase sampler must run **in
   the page** — a `setInterval` recording the phase pair into an array the driver reads
   afterwards — and must be started *before* the kill. Anything the driver must do *during*
   the outage (the degraded screenshot, the "retry now" press) needs the restart launched
   non-blocking or armed in the page.
4. **Convergence must be unassisted.** Between the kill and the return to `live`, issue
   **no** reload, **no** "retry now", and **no** API call that could nudge the client. The
   automatic path is the subject of the phase. "retry now" gets its own step, later, on a
   shell that is already healthy — where its job is to be a *no-op* on the transcript.
5. **Record which recovery path fired; never gate on it.** Capture the observation and
   product stream traffic across the reconnect and classify it. The paths differ in kind, and
   conflating them is the mistake this rule exists to prevent:
   - `replay_gap` — reconnect-specific. The replacement process's in-memory replay store
     (`InMemoryLiveReplayStore`, 2048 events / 120s TTL) holds no buffer for the session, so
     the browser's retained cursor is unservable and the gap reason is `unavailable`. A >120s
     outage with later activity trims an existing buffer instead → `trimmed`.
   - `terminal_replacement` — **not** reconnect evidence. Every `Committed` observation
     becomes one, so it fires on every turn commit; a healthy run shows many.
   - `resident_replacement` — revision-stable resident authority. It triggers an asynchronous
     state refetch that must preserve every provisional transcript row; only a terminal
     replacement may settle those rows.
   - `resync` — product-stream broadcast lag. It needs the client to fall behind the
     1024-deep registry and is not reliably reachable from a browser run.
   - a plain reattach with none of the above.

   Report the path as run evidence. The **invariant is convergence**; the path is
   timing-dependent.
6. **Judge the in-flight turn from durable truth.** Whatever the DOM showed at the instant of
   the kill is a claim about a connection, not about a turn. Resolve the turn from
   `trace.jsonl` (`turn_completed` for its exact turn id) and the session graph first, then
   require the rendered transcript to agree. Either outcome passes: the turn completed durably
   and recovery surfaced it, **or** it did not complete and the shell says so honestly. What
   fails is a mismatch — a rendered reply with no durable commit, a durable commit that never
   renders, or a shell left claiming `running` over a turn the trace has already terminated.
7. **Count rows, never read prose** (as in `workbench-chat-projection`). A row is a
   `#timeline .message` element; its role is the `user` / `assistant` class. Two rows with
   byte-identical bodies are still two rows. Recovery is a second, independent projection of
   the same conversation, so duplication is this scenario's most likely defect signature.
8. **Settle by stability, not by a sleep.** After each convergence gate, poll until row and
   message counts are unchanged across several consecutive samples before counting. A
   duplicate that lands one retry interval late must still be caught.
9. **Scope everything to one session id.** Drive `/?session_id=<S>` and scope every read —
   `/api/state?session_id=<S>`, the `graph_nodes.session_id` filter, the product-event log
   key, and the trace's `context.session_id`. An unscoped read mixes other tabs in and voids
   the run.

## Working material

- Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot an empty,
  port-isolated stack with
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_RUN_DIR=<fresh-tmp>/run
  AGENT_WORKBENCH_OPEN=0 bash scripts/agent-workbench-dev.sh up --port <p>`
  (or `just agent-workbench <p>` with the same environment). Gate `GET /healthz` → 200.
  Export the same `AGENT_WORKBENCH_DATA_DIR` and `AGENT_WORKBENCH_RUN_DIR` on **every**
  subsequent helper invocation.
- Pick one run session id `<S>` = `runbook-reconnect-<run-id>` and open `/?session_id=<S>`.
- Identities to record at boot and re-check after the restart: the workbench PID
  (`<run-dir>/workbench-127.0.0.1_<p>.pid`), the Restate container name
  (`lash-agent-workbench-dev-restate-<p>` unless `AGENT_WORKBENCH_RESTATE_CONTAINER` is set)
  with its `docker inspect` id and `.State.StartedAt`, and the rendered session id.
- UI affordances: the chat composer and its **send** control, the transcript timeline, the
  phase pill, the connection banner and its **retry now** button.
- **Layer 1 — rendered DOM:** `#timeline .message.user` / `.message.assistant` counts and
  each row's body text; plus the phase pair from golden rule 2.
- **Layer 2 — durable state:** the session graph in
  `<data-dir>/lash-sessions/durable-core.db`, table `graph_nodes`, filtered to
  `session_id = <S> AND tombstoned = 0`, reading `node_json` for `kind = "event"` nodes whose
  `event.Conversation.role` is `User` / `Assistant`; the app projection
  `GET /api/state?session_id=<S>.messages`; the product-event log
  `<data-dir>/product-events.json` keyed by `<S>`; and `<data-dir>/active-turns.json`.
- **Layer 3 — logs:** `<data-dir>/trace.jsonl`, records with `context.session_id == <S>`;
  count `type == "turn_completed"` and read `context.turn_id`. The workbench log
  (`<run-dir>/workbench-127.0.0.1_<p>.log`) carries the two process incarnations and is where
  the replacement process's boot is proven.
- **Stream traffic** is evidence for golden rule 5: capture the response bodies of
  `/api/observations` and `/api/events` in the driver (a `page.on("response")` hook, or a
  parallel driver-side reader on the same cursors) and keep every non-`observation`,
  non-`event` line. When `resident_replacement` appears, capture the provisional row multiset
  before the refetch and after it resolves.
- Teardown: `bash scripts/agent-workbench-dev.sh down --port <p>` and confirm the
  port-derived Restate container is gone.

Save every named artifact and API/store/trace extract under the run's artifact directory.

## Phase 0 — Boot, scope one session, record identities

Boot, gate `/healthz` → 200, and open `/?session_id=<S>`. Require the composer, an empty
transcript, and the rendered session id `<S>` agreeing with
`/api/state?session_id=<S>.settings.session_id`. Require the phase pair for `live`:
`#shellStatus` hidden **and** `#busyText` reading `idle`. Record the workbench PID, the
Restate container id and `StartedAt`, and confirm all three layers are empty for `<S>`.
Screenshot `00-live-empty.png`; save `00-identities.json`.

## Phase 1 — Baseline: one send settles, phase stays live

Send one short turn with a unique literal marker, e.g.
`Reply with exactly this and nothing else: FIG991-BASE-<run-id> acknowledged`.

Gate, in order: one `turn_completed` for `<S>`; the `idle` pill with `#shellStatus` still
hidden; then the settle gate. Require **exactly** 1 `.message.user` + 1 `.message.assistant`
row, 1 `user` + 1 `assistant` in `/api/state.messages`, 1 `User` + 1 `Assistant` in the
session graph, and 1 `turn_completed`. The phase must have stayed `live` throughout — a
banner that appeared during an ordinary turn is a false-positive outage and a finding.
Screenshot `01-baseline-live.png`; save `01-baseline-{dom,state,store,trace}.json`.

## Phase 2 — Replace the web process under a live turn

**2a — put a turn in flight and start watching.** Submit a turn that will stay in flight long
enough to be interrupted — ask the agent to do a small amount of real work before answering
(e.g. sleep a few seconds inside one Lashlang block, then answer with a unique marker
`FIG991-INFLIGHT-<run-id>`). Gate the `running` pill and exactly one
`/api/state.active_turns` address for `<S>`, and record that exact turn id. Start the in-page
**phase sampler** (~150ms into a timeline) **before** the kill — per golden rule 3 the
transition is the evidence and it cannot be recovered after the fact. Screenshot
`02-inflight-running.png`.

A turn that has already settled is a retry of this step: it exercises reload, not reconnection.
Note also that the interruption is deliberately *not* an interruption of execution — the send
was submitted to Restate, so the sleep is durable and the replacement process picks the
invocation back up.

**2b — restart only the web process.** Run `bash scripts/agent-workbench-dev.sh restart
--port <p>` with the exported data and run directories. Gate: the recorded PID is gone and
the new PID differs; the Restate container id and `StartedAt` are unchanged; `/healthz`
answers again.

**2c — require the phase to leave live and return, unassisted.** From the sampler timeline,
require at least one sample whose phase is `reconnecting` or `unavailable`, and a later
sample back at `live`, with **no** reload, retry-now, or other intervention in between
(golden rule 4). Require the shell never rendered the outage as an empty idle session: while
degraded, the banner is visible and the transcript retains the Phase 1 rows. Screenshot the
degraded shell as `03-degraded.png` (best effort — a screenshot taken during the outage is
evidence to record, not a gate) and the recovered shell as `04-reconverged-live.png`. Save
the sampler timeline as `04-phase-timeline.json`.

**2d — resolve the in-flight turn from durable truth.** Per golden rule 6, read the trace
first: does `<S>` have a `turn_completed` for the Phase 2a turn id? Then require the rendered
transcript to agree with that answer, and run the **three-layer cross-check** on the final
transcript — rendered DOM vs durable state (`graph_nodes` + `/api/state.messages` +
product-event log) vs trace executions — pairwise, as counts *and* identities. Row-identical,
no duplicates, no losses. `active-turns.json` and `/api/state.active_turns` must not still
carry a turn the trace has terminated. Save
`05-postrestart-{dom,state,store,trace}.json` and `05-crosscheck.json`.

## Phase 3 — Recovery paths and the manual affordance

**3a — classify the recovery path.** From the captured stream traffic across the reconnect,
record every `replay_gap` (with its `gap.reason`, `requested_cursor` and `latest_cursor`),
`terminal_replacement`, `resident_replacement`, and `resync` line, and classify the reconnect
per golden rule 5. Save
`06-recovery-paths.json`. If a `replay_gap` fired, additionally require that the rows it
recovered onto are the **same** rows — the post-gap multiset equals the pre-kill multiset plus
whatever the durable truth of 2d added, and nothing else. A `replay_gap` that lands on a
different row multiset is the defect this phase exists to catch.

If a `resident_replacement` fired while the in-flight turn still had provisional rows,
require the post-refetch provisional-row multiset to equal the pre-refetch multiset. Record a
resident replacement seen without provisional rows, but do not claim it exercised the
preservation contract.

Do not order the gap's `latest_cursor` against its `requested_cursor`. The two opaque tokens
belong to different replay incarnations, so neither "behind" nor "ahead" is meaningful. That
lack of a defined ordering is precisely why the observation channel recovers through a
snapshot instead of comparing cursors. Record both tokens verbatim; a run that reports only
the reason has thrown away evidence that distinguishes this fault from a trim.

**3b — force a gap deliberately if 3a saw none.** Optional, and only if the reconnect
produced a plain reattach: reconnect the observation stream at a cursor the replacement
process cannot serve (reload the page while injecting a stale cursor, or idle past the 120s
replay TTL and then drive activity so the buffer trims) and require `replay_gap` with reason
`unavailable` or `trimmed`, followed by convergence on the same multiset. Record it as
attempted-and-unreached rather than inventing a path if the fault does not materialize;
`resync` in particular needs >1024 unconsumed product events and is not expected to be
reachable here.

**3c — "retry now" forces convergence and changes nothing.** Make the banner appear once more
with a second `restart`, and press **retry now** exactly once while it is visible. Because the
window is about two seconds (golden rule 3), arm the press **in the page** — a watcher that
clicks `#shellStatusRetry` the first time `#shellStatus` is not hidden, recording the phase
pair and the row multiset at the moment of the click. That is a user pressing the button as
soon as they can see it; a driver-side click loop blocked inside the restart command will
simply miss it, and pressing it later on a healthy shell tests nothing. Require: the phase
returns to `live`; and the row multiset after the press equals the multiset captured at the
press — the button forces convergence, it does not re-derive the conversation. A row count that
changes across a retry-now press is a duplicate-projection defect. Screenshot
`07-retry-now.png`; save `07-retry-{before,after}.json`.

## Phase 4 — Reload after recovery renders the identical multiset

Record the pre-reload row multiset (role class + body text per row). Reload the page, gate the
rendered session id and the `live` phase pair, then the settle gate, and require the
post-reload multiset **equals** the pre-reload multiset, with per-role counts still equal to
the store and trace counts from Phase 2d. The live path renders from the streams and the
backfill path from the committed projection; a row that exists on only one of them localizes
the defect to that path and is as much a failure as a duplicate on both. Screenshot
`08-after-reload.png`; save both multisets as `08-reload-multiset.json`.

## Phase 5 — Teardown and score

Run `bash scripts/agent-workbench-dev.sh down --port <p>` and confirm the workbench process
and the port-derived Restate container are both gone (`docker ps -a` shows no
`lash-agent-workbench-dev-restate-<p>`).

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot/scope | `/healthz` 200; rendered and API session id both `<S>`; phase pair = `live`; all three layers empty | | `00-live-empty.png`, `00-identities.json` |
| Baseline send | 1 user + 1 assistant row = 1+1 API = 1+1 store = 1 `turn_completed`; phase stayed `live` | | `01-baseline-live.png`, `01-baseline-*.json` |
| Turn in flight | `running` pill and exactly one `active_turns` address recorded before the kill | | `02-inflight-running.png` |
| Web-process-only replacement | PID changed; Restate container id and `StartedAt` unchanged | | command log, `04-phase-timeline.json` |
| Phase left `live` and returned unassisted | a `reconnecting`/`unavailable` sample, then a later `live` sample, with no reload or retry-now between | | `04-phase-timeline.json`, `03-degraded.png`, `04-reconverged-live.png` |
| Degraded shell told the truth | while degraded: banner visible, prior rows retained, never an empty idle session | | `03-degraded.png`, `04-phase-timeline.json` |
| In-flight turn resolved from durable truth | trace/store decide the outcome; the rendered transcript agrees; no stale active address | | `05-postrestart-*.json` |
| Three-layer cross-check | DOM vs durable vs trace reconcile pairwise on the final transcript; no duplicates, no losses | | `05-crosscheck.json` |
| Recovery path recorded | the fired path(s) are named from captured stream traffic; gap recovery landed on the same multiset; a resident replacement preserved any provisional rows | | `06-recovery-paths.json` |
| "retry now" converges without re-deriving | phase returns to `live`; row multiset unchanged across the press | | `07-retry-now.png`, `07-retry-*.json` |
| Reload identity | post-reload row multiset equals pre-reload multiset | | `08-after-reload.png`, `08-reload-multiset.json` |
| Teardown | workbench and port-derived Restate container gone | | command log |

**Aggregate:** with a browser attached and a turn in flight, did replacing the workbench web
process — Restate untouched — drive the shell honestly through a degraded phase and back to
`live` with no human action, resolve the interrupted turn to whatever the durable stores
actually say happened, and leave one transcript that the rendered DOM, the durable session
graph, the state and product-event projections, and the turn trace all agree on — before and
after a reload, and unchanged by a manual retry?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
