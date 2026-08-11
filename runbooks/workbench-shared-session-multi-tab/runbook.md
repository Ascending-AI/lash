# E2E Scenario: Workbench Shared-Session Multi-Tab Convergence

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface, polling,
> named-checkpoint screenshot, **three-layer cross-check**, real-token, Abort/RCA, and
> teardown rules. This runbook adds only the shared-session multi-tab scenario.

**Purpose.** Referee **fan-out**: what happens when *two* browser clients are attached to
**one** session at the same time. Every other workbench runbook drives a single client.
[`workbench-session-isolation`](../workbench-session-isolation/runbook.md) drives two
clients that must **not** see each other's conversation; this one drives two clients that
must see the **same** conversation, and proves that one unit of conversational work
reaches every attached client **exactly once** and that both clients converge on an
identical transcript.

**Why this scenario exists.** The workbench scopes every request by `?session_id=` — a
`window.fetch` monkeypatch rewrites each `/api/` URL to carry the tab's scoped id — so two
browser contexts opened at the same `/?session_id=<S>` are two independent stream
consumers of one session, not two sessions. That is the shape downstream hosts actually
run: a user with the conversation open in two tabs, two devices, or a teammate watching
the same run. It is served by genuinely multi-consumer plumbing — the product stream is a
per-session `tokio::sync::broadcast` replayed from the caller's cursor
([`state.rs`](../../examples/agent-workbench/src/main_sections/state.rs)), and each
`/api/observations` request opens its **own** `subscribe_recoverable_chat` subscription
([`routes.rs`](../../examples/agent-workbench/src/main_sections/routes.rs)) — so no client
starves another and no client is privileged.

Nothing gates that. The two defect classes it invites are invisible to a one-client
runbook:

- **Divergence** — the two tabs settle on different transcripts. Each client's projection
  is a function of *its own* attach cursor and *its own* optimistic local rows, so a row
  rendered from local state in the acting tab and from the stream in the watching tab can
  land once in one tab and twice, or never, in the other.
- **Local echo double-counted on fan-out** — the acting tab renders a row it created
  optimistically, then the same logical row arrives over the shared stream under a
  different id. This is the FIG-972 / FIG-984 two-id-namespaces shape refereed by
  [`workbench-chat-projection`](../workbench-chat-projection/runbook.md), but fan-out
  doubles the surface: the *watching* tab has no optimistic copy to correlate against, so
  the same defect can present as a duplicate in one tab and a single row in the other.
  **Tab-vs-tab disagreement is a stronger signal than either tab's own count**, and it is
  the signal this runbook exists to read.

**Real tokens.** Every turn goes through OpenRouter, so prose and termination style are
nondeterministic. No exact model wording is an answer key. The answer key is **row
multiset equality between the two tabs**, plus per-role counts reconciled against durable
truth.

## Scenario-specific golden rules

1. **DOM-vs-DOM equality between the tabs is the headline gate.** After every settle,
   extract each tab's ordered conversation rows as `(row-kind, text)` pairs and require
   the two **multisets to be equal**. Equality, not similarity: a row present twice in A
   and once in B fails, and so does a differing body text. Phase 1 additionally requires
   identical **order**, because a single session has one committed conversation order and
   both clients render from it.
2. **Compare conversation rows, never client-local furniture.** A conversation row is
   `#timeline .message.user` or `#timeline .message.assistant`. Everything else in the
   timeline is per-tab furniture and is **excluded from the equality gate**:
   `.message.event` ("agent woken", "queued turn started", "red button trigger
   occurrence"), `.note`, `.message.error`, `.ingress-receipt`, the streaming assistant
   draft, and `.reasoning` / `.code-block` / tool rows. The discriminator is not taste:
   only `message`, `reasoning`, and `code_block` rows have a backfill path
   (`renderStateTranscript`), and `.message.event` rows have **no durable counterpart at
   all** — they exist only because the tab's observation subscription delivered a live
   activity. Whether a given tab re-renders them therefore depends on its **observation
   replay window**, not on committed truth, so requiring them to match across tabs, or
   across a reload, gates on a coincidence. Record furniture counts per tab as evidence at
   each checkpoint and gate only the durable rows. *Observed in this runbook's execution:
   a mid-stream reload of B reproduced its event-row count exactly (6 → 6) from the
   replay, and its reasoning/code-block rows grew by the new turn — so do not assert that
   a reload loses furniture either. Record; do not gate.*
3. **Both tabs must be scoped, and scoped to the same id.** Drive both at
   `/?session_id=<S>` with the same `<S>`. Gate the rendered session id and
   `/api/state?session_id=<S>.settings.session_id` in **each** tab before sending
   anything. An unscoped tab reads the workbench's default session and voids the run.
4. **Two independent clients, not two views of one client.** Use two separate browser
   **contexts** so nothing is shared but the server: separate storage, separate cookies,
   separate `fetch` stacks, separate stream connections.
5. **Assert the busy contract below, not a symmetric wish.** Busy is a **per-tab
   client-local boolean**, and which transitions reach other tabs — and *by what
   mechanism* — is the scenario's most surprising finding. Assert the mechanism, including
   its observable fingerprint, so a regression that keeps the pill correct while losing
   the mechanism is still caught.
6. **Settle by stability, not by a sleep.** After the `turn_completed` and idle gates,
   poll **both** tabs until their row counts are unchanged across several consecutive
   samples before comparing. A duplicate that lands one sample late in one tab is the
   whole point of the scenario; a fixed sleep either misses it or passes by luck.
7. **Per-tab evidence, always in pairs.** Every named artifact and screenshot exists for
   both A and B, and each checkpoint also gets a **side-by-side** composite so the two
   pills and the two transcripts are legible in one image.
8. **Report divergence, never repair it.** A tab-vs-tab mismatch is a finding: capture
   both tabs' rows, the durable counts, and which tab dissents, then FAIL the phase. Do
   not edit the client, do not reload a tab to "resettle" it into agreement, and do not
   weaken the multiset gate to a subset check.
9. **Advance expectations per unit of work, never by absolute constants.** Track expected
   `(user, assistant, turn_completed)` incrementally and assert the absolute totals
   against that tracker. An unplanned extra turn must fail one gate loudly, not cascade a
   wrong constant through every later phase.

## The busy-state contract

Discovering this is part of the run. What follows was derived from
[`index.html`](../../examples/agent-workbench/assets/index.html) **and confirmed by
execution**, including the stack that writes the flag. Re-derive it if the client changes.

`busy` is one client-local boolean per tab (`let busy = false`). Five things write it:

| writer | trigger | reaches | observed |
|---|---|---|---|
| `postCommand` → `setBusy(true, "starting turn")` | this tab POSTs `/api/turn` / `/api/reset` | **only this tab** (optimistic) | yes |
| composer submit → `setBusy(true, "queued next · waiting for the running turn")` | `/api/turn` answered `queued: true`, so this send is the next turn's input (FIG-1000) | **only this tab** (the queued receipt itself fans out) | Phase 6 |
| `applyStateSnapshot` → `setBusy(true, "turn running · restored")` | any authoritative `/api/state` read that sees `active_turns` non-empty | **every tab that performs one** | yes — the only cross-tab start path |
| `applyProductEvent` `done` → `setBusy(false, "ready")` | the turn's `done` product event on `/api/events` | **every attached tab** | yes |
| `renderQueuedWorkStarted` → `setBusy(true, "agent running")` | `queued_work_started` turn activity on `/api/observations` | every attached tab, in principle | **never fired** — see Phase 3d |

There is **no periodic `/api/state` poll** (the 1.4 s pollers are `/api/work`,
`/api/queued-work`, `/api/triggers`, `/api/accounts`) and **no dedicated turn-started
event**. Nevertheless a watching tab *does* reach `running`, promptly. The mechanism is
the finding:

> When a turn starts, its first commit makes every **other** tab's
> `subscribe_recoverable_chat` subscription emit a `terminal_replacement`. That drives
> `recoverTerminalReplacement` → `recoverFromState` →
> `applyStateSnapshot(state, true, true)`, an **authoritative** snapshot whose
> `busyAfterStateSnapshot` reads the session-scoped `active_turns` and flips the pill to
> `running`. The watching tab is never *told* a turn started; it **re-reads server truth**
> after a stream-recovery event.

Two observable fingerprints make that mechanism assertable rather than inferred, and Phase
3 gates both:

- the **subtitle** under the watching tab's pill reads `turn running · restored`, while
  the acting tab reads `starting turn` / `running · Ns`. Same pill text, different
  provenance.
- the watching tab's busy-write stack is `applyStateSnapshot ← recoverFromState ←
  recoverTerminalReplacement`.

So:

- **Turn end is agreed, directly.** Both tabs leave `busy` on the same shared `done`
  event.
- **Turn start reaches watching tabs indirectly but promptly** — measured at **0.26 s** in
  this runbook's execution. Phase 3 measures the latency and records it rather than
  assuming it.
- **There is exactly one cross-tab start mechanism, and it is this one.** A wake turn is
  *not* different: a trigger press reaches `postTriggerCommand`, which never touches
  `busy`, so for a RED-started turn **even the tab that pressed the button** learns via
  the same recovery re-read and shows `turn running · restored`. The
  `renderQueuedWorkStarted` → `agent running` path was **never observed to fire** for a
  trigger press in this configuration. Phase 3d asserts that observed behavior rather than
  the code path's promise.
- **A tab that arrives or reloads mid-turn agrees**, via the same `active_turns` read.

Two consequences that matter more than the pill:

- **Because the recovery is authoritative, `applyStateSnapshot` calls `clearTranscript()`
  and rebuilds the watching tab's transcript from the committed projection — at every turn
  start.** This is the deep reason multi-tab transcripts converge so robustly: a watching
  tab is continuously re-backfilled from durable truth, so it is structurally incapable of
  accumulating live-path drift. It is also a cost that scales with transcript size, not a
  free win — every turn start re-derives the whole conversation in every watching tab.
- **`POST /api/turn` admits a send against a busy session, it does not start one**
  (FIG-1000). `send_turn` reads `active_turns` and, when the session already has a running
  turn, enqueues the send as a `next_turn` input and answers `queued: true` with the same
  receipt `/api/turn/input` returns — the same admission shape as `inject now` ("inject now
  requires exactly one running turn") and `run_queued_work_batch` ("queued work cannot be
  run while this session has an active turn"), which both refuse rather than lie. So the
  **client-local** `busy` flag — `postCommand`'s early return and `sendButton.disabled` —
  is no longer the only thing standing between a second viewer and a doomed concurrent
  turn; it is now just the local affordance. The server check is advisory (two sends can
  race past it), so a head-CAS loser has to be visible too: a failed turn retires the
  rows the workbench published for it and publishes `done` with `outcome: "failed"`.
  Phase 6 gates both halves.

**What downstream multi-viewer hosts should expect.** Transcript convergence is strong and
needs no host work: every viewer is re-backfilled from committed truth at each turn start,
and the `done` event closes every viewer's busy state — both are real cross-viewer
guarantees. Running state also reaches every viewer, fast (0.26 s measured), but it is a
**side effect of stream recovery, not a signal**: nothing publishes "a turn started". Its
label literally says `restored`, it costs a full transcript re-derivation in every viewer
on every turn start, and it is the *only* path — a trigger press has no direct start
signal either, not even in the tab that pressed. A host that wants a first-class
turn-started fan-out must publish one itself. Concurrent submission, by contrast, needs no
host invention: `/api/turn` holds a second viewer's send as the next turn's input and says
so, and a turn that loses the head CAS surfaces as a failure row whose optimistic
rows retire in every viewer (Phase 6). What a host does have to copy is the *shape*: an
advisory admission check plus an honest failure path, because the lease and the CAS — never
the handler — decide who commits.

## Working material

- Require `OPENROUTER_API_KEY`. Boot one empty, port-isolated stack with
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench
  <port>` (or `bash scripts/agent-workbench-dev.sh up --port <port>` with the same
  environment). Gate `GET /healthz` → 200. Teardown on success or Abort is `just
  agent-workbench-down <port>`.
- Pick one run session id `<S>` = `runbook-multitab-<run-id>`. Open **two browser
  contexts**, A and B, both at `/?session_id=<S>`.
- UI affordances per tab: the chat composer and its **send** control, the transcript
  timeline, the running/idle **pill** *and its subtitle*, the **inject now** / **queue
  next** controls, the left-sidebar **RED** trigger button, the registrations rail, and
  the **work** rail.
- **Layer 1 — rendered DOM, per tab:** `#timeline .message.user` / `.message.assistant`
  counts and body texts; the pill text and subtitle; the rails' rows.
- **Layer 2 — durable state** (one session, so one copy shared by both tabs): the session
  graph in `<data-dir>/lash-sessions/durable-core.db`, table `graph_nodes`, filtered to
  `session_id = <S> AND tombstoned = 0`, `kind = "event"` nodes whose
  `event.Conversation.role` is `User` or `Assistant`; the projection `GET
  /api/state?session_id=<S>.messages`; and `<data-dir>/product-events.json` keyed by
  `<S>`. **The store runs in WAL mode** — snapshot `durable-core.db` together with its
  `-wal` and `-shm` siblings, or read the live file with `mode=ro`. Copying the main file
  alone reads as an empty graph and manufactures a phantom three-layer mismatch in every
  phase.
- **Layer 3 — logs:** `<data-dir>/trace.jsonl`, records with `context.session_id == <S>`;
  count `type == "turn_completed"`. `context.turn_id` distinguishes a composer turn
  (`workbench-turn-…`) from a wake or drained queued turn (`workbench-queued-…`).
- Layers 2 and 3 are **shared truth**: they answer "how many messages exist", and the
  per-tab DOM answers "how many each client rendered". Cross-check each tab against that
  one truth, then the two DOMs against each other.
- Mid-turn phases need a turn that stays in flight long enough to act on. A short marker
  echo settles in under ten seconds and collapses the window; use a deliberately long
  generation (e.g. *"write out the numbers 1 through 45, each on its own line, with one
  short sentence after each"*, ~20–30 s) for Phases 3–6. A collapsed window is a harness
  gap, not a finding: it silently converts a mid-turn assertion into a post-turn one.

Save every named artifact under the run's artifact directory, suffixed `-a` / `-b` per
tab.

## Phase 0 — Boot two clients on one session

Boot and gate `/healthz` → 200. Open context A at `/?session_id=<S>`, then context B at
the same URL. In **each** tab require: the composer, the RED trigger button, an empty
transcript, the `idle` pill, the rendered session id `<S>`, and
`/api/state?session_id=<S>` reporting `settings.session_id == <S>` with empty `messages`
and `active_turns`. Require both tabs to report the **same** origin and the **same**
session id — a tab that generated its own id is a setup failure, not a finding.

All three layers start at zero. Save `00-sessions.json`; screenshot `00-empty-a.png`,
`00-empty-b.png`, and side-by-side `00-empty-both.png`.

## Phase 1 — One send from A, both tabs render one pair

Send from **A** one short turn with a unique literal marker, e.g. `Reply with exactly this
and nothing else: FIG993-SHARED-<run-id> acknowledged`. **Do not touch B** — B is a pure
observer.

Gate: one `turn_completed` for `<S>`; empty `active_turns`; then the settle gate on
**both** tabs. Run the three-layer cross-check against shared truth (1 `user` + 1
`assistant` in `/api/state.messages`, 1 `User` + 1 `Assistant` in the graph, 1
`turn_completed`), then the fan-out gates. Require **per tab, independently**, exactly 1
`.message.user` and 1 `.message.assistant` row — **in B as well as A**. B never posted
anything and has no optimistic copy; it renders purely from the shared streams and must
arrive at the same 1+1. Then the headline gate: A's ordered rows **equal** B's, same
order, same multiplicity, marker text byte for byte.

This is the fan-out regression gate for the FIG-972 / FIG-984 shape. A second `you` row in
A only is a local-echo correlation defect; a second `agent` row in B only is a stream-side
dedupe defect; two rows in both over one committed message is a render defect projected to
every client; two rows in both over **two** committed messages is a commit defect. Name
which, using the shared durable counts.

Screenshot `01-one-pair-{a,b,both}.png`; save `01-rows-a.json`, `01-rows-b.json`,
`01-truth.json`, `01-dom-vs-dom.json`.

## Phase 2 — Register from A, press RED from B

**2a — register the watcher from A.** Send `Let me know when I press a button` from A and
let the agent register the button watcher. Gate a second `turn_completed`, idle, and the
settle gate on both tabs; require cumulatively 2+2 rows per tab against 2+2 API, 2+2
graph, 2 `turn_completed`, and A/B multisets still equal. A duplicate here is already a
failure — do not press RED to "get to the real test".

Require the registration in **both** rails: the rail is a 1.4 s scoped `/api/triggers`
poll, so a registration created by A's turn must appear in B without B acting. Screenshot
`02a-registered-{a,b,both}.png`; save `02a-triggers.json`.

**2b — press RED from B, exactly once.** The acting tab is now the one that did *not*
register the watcher. Gate a `turn_completed` whose `context.turn_id` starts with
`workbench-queued-`, then idle, then the settle gate. Require: user-row count **unchanged
in both tabs** (a wake is a host event, not a chat input); exactly **one new**
`.message.assistant` row **per tab**; the API and graph assistant counts matching; one
wake `turn_completed`, not two; and A/B multisets equal. Which id holds the reply is
termination-dependent (see the chat-projection runbook's termination table) — record ids,
gate on counts.

Note what this proves about ownership: a press in B produced an assistant row in A over a
watcher **A's own turn** registered. Neither registration nor delivery follows the tab.

**Work rails must agree.** Poll both rails to stability and require they render the same
process rows for the same scoped `/api/work?session_id=<S>` response: same count, same
cards. A row in one rail and not the other is a render defect even though the API agreed.
Save `02b-work-a.json`, `02b-work-b.json`, `02b-work-api.json`; screenshot
`02b-after-red-{a,b,both}.png` with the newest rows and the work rail visible.

## Phase 3 — The busy-state contract

Assert the contract above and record the **actual** behavior, one sub-gate per line: what
was promised, what was observed.

**3a — a composer turn from A.** Start a long turn from A. Require A's pill `running` with
subtitle `starting turn` / `running · Ns`, `runningActions` shown, `aria-busy="true"`.
Require B renders the turn it did not start (its timeline grows while `active_turns` is
non-empty).

**3b — measure the watching tab's convergence.** From the moment A submits, poll B every
~50 ms until its pill reads `running`, and **record the latency**. Require:

- B reaches `running` without acting (bound it explicitly, e.g. 15 s; a timeout is a real
  regression in the recovery path, not a slow model);
- B's subtitle is **`turn running · restored`** — the fingerprint that this is a
  server-truth re-read, not a start signal, and distinct from A's subtitle;
- B's busy-write stack is `applyStateSnapshot ← recoverFromState ←
  recoverTerminalReplacement`. Capture it by wrapping `window.setBusy` in B before the
  turn and recording each call with `new Error().stack`. Asserting the *mechanism*, not
  just the pill, is what makes this phase a regression gate rather than a screenshot.
- B's client-side concurrency guard arms on convergence: B's **send** control becomes
  disabled and its **inject now** / **queue next** controls become enabled.

Screenshot side-by-side `03ab-inflight-both.png` with both pills and both subtitles
legible; save `03-watcher-convergence.json` (latency, both shells) and
`03-b-busylog.json`.

**3c — turn end is agreed.** Let the turn settle. Require **both** pills `idle`, both
`runningActions` hidden, and the cross-check clean with multisets equal. This is the
direct cross-tab guarantee: the `done` product event reaches every attached client.
Screenshot `03c-settled-both.png`.

**3d — a wake turn's start uses the *same* mechanism.** Press RED once from A. Poll both
tabs until **both** pills read `running`, then let the wake turn settle and require both
pills return to `idle` with the cross-check clean.

The discriminating assertion is on the **labels, collected over the whole wake turn** —
not sampled at the instant the pills flip, which is too early to see a later writer.
Require that **both** tabs' only busy-true label is `turn running · restored`, and that
neither tab records `agent running`. That is the observed behavior, and it is the phase's
finding: `postTriggerCommand` never sets `busy`, so a trigger press has no optimistic
local start either, and the `queued_work_started` → `agent running` writer does not fire
for a button-triggered wake. Every tab — including the one that pressed — converges
through the single recovery path. If a future change makes `agent running` appear, that is
a **contract change to record**, not a silent pass: re-derive this section.

Screenshot `03d-wake-both.png`; save `03d-wake.json` with both tabs' complete busy logs,
each entry carrying its writer frame.

## Phase 4 — Mid-turn cross-tab queue-next

A starts a turn; **B** — a different client — queues the next turn's input; both tabs must
end up rendering the queued turn's rows identically.

Start a long composer turn from A. Wait for **B's `queue next` control to become enabled**
— per Phase 3b this happens on B's own convergence, so **no reload or other intervention
is needed**; record the path taken. Never remove the `disabled` attribute and never call
`/api/turn/input` directly: either one tests the API, not the browser surface.

Click **queue next** in B with a distinct literal marker, e.g. `Also reply with exactly:
FIG993-QUEUED-<run-id> delivered`. Require B renders a `queued next` ingress receipt and
`/api/state.pending_turn_inputs` carries the input. A is not required to show a receipt —
if it does, that is the shared `turn_input` product event and is equally correct; record
which happened rather than gating on it. Screenshot `04-queued-midturn-both.png`.

Now let A's turn settle and **click nothing else**. The runtime drains the deferred input
itself: `has_queued_work` counts pending `NextTurn` inputs, so after the active turn
terminalizes and releases the lease, `claim_and_run_pending` submits a `workbench-queued-`
turn (`WorkbenchQueuedWorkSubmitter` in
[`state.rs`](../../examples/agent-workbench/src/main_sections/state.rs)). Gate that turn's
`turn_completed`, then idle, then the settle gate. Require:

- the queued input committed as a `User` message and rendered as a `.message.user` row in
  **both** tabs, carrying B's marker;
- the queued turn's reply as one `.message.assistant` row in **both** tabs;
- per-role DOM counts per tab equal to the shared API and graph counts;
- exactly **one** drained queued turn, not two;
- **A's and B's multisets equal**, both including B's queued marker.

A queued input that never runs is a lost-input defect → FAIL with
`/api/state.pending_turn_inputs` and the queued-work rail as evidence. One that runs but
reaches only one tab is a fan-out defect → FAIL with both tabs' rows. Screenshot
`04-after-queued-both.png`; save `04-rows-{a,b}.json`, `04-truth.json`,
`04-queue-path.json`.

## Phase 5 — Reload B mid-stream

Record both tabs' multisets and B's furniture counts. Start a fresh long turn from **A**
and, once B is rendering it (`running`), **reload B**.

A is the control: require A's turn completes normally, A returns to `idle`, and A's
multiset grows by exactly the new turn's one pair.

B is the subject. **Wait for B to hydrate before reading anything** — a pill of
`connecting` means the shell has no snapshot yet, and reading the session label then
reports `connecting…` rather than a defect. Then require:

- B's rendered session id is still `<S>`;
- B restores busy from `/api/state.active_turns` (pill `running` if the turn was still
  active);
- after the settle gate, **B's multiset equals A's**;
- every pre-reload conversation row is still present after the reload — reload is an
  identity over durable rows, not a re-derivation that drops or doubles them;
- B's furniture counts before and after are **recorded, not gated** (golden rule 2). Do
  not assert that they vanish and do not assert that they survive: in this runbook's
  execution the event-row count was reproduced exactly from the observation replay, which
  is a property of the replay window rather than a contract.

A duplicate appearing **only** after reload localizes the defect to the backfill path; one
appearing **only** before it localizes to the live path; either is as much a failure as
one in both. Screenshot `05-converged-both.png`; save `05-multiset-a-{before,after}.json`,
`05-multiset-b-{before,after}.json`, `05-b-after-reload.json`.

## Phase 6 — Concurrent-submission admission gate

Run this **last**: it deliberately submits a second turn against a session that already
has one, so its side effects must not contaminate the convergence phases.

Start a long turn from A. While `active_turns` is non-empty, submit a second `POST
/api/turn` for the same session from B. B's own composer is disabled by then (Phase 3b),
so this is an **API-level probe of layer 2**, and the runbook says so plainly rather than
pretending it is a browser gesture: the point is to gate the server contract the browser
guard stands in for. Record the status, the body, and `active_turns` before and after.

**This is a hard gate, not a characterisation.** FIG-1000 fixed the contract: a session
runs one turn at a time, the lease and the commit CAS enforce that durably, and `/api/turn`
now reports what it actually did with the send instead of accepting a doomed second turn.
Require all of:

- the response is `200` with `queued: true` and a `queued_input` receipt whose `ingress` is
  `next_turn` and whose `text` is B's marker — an `{"accepted":true}` body with no `queued`
  field, or `queued: false`, is a **FAIL** (the old, lying contract);
- `active_turns` still holds exactly **one** turn — the queued send must not register a
  second one — and no second turn workflow is submitted;
- `/api/state.pending_turn_inputs` carries B's marker as a `deferred_next_turn` input, and
  **no** user row for it appears in either tab: the queued send renders as an
  `.ingress-receipt` (`queued next`), never as an optimistic `.message.user`;
- after A's turn settles, the queued input is drained as **exactly one** further turn whose
  committed user message carries B's marker and which produces **exactly one** assistant
  row, in **both** tabs — the same shape Phase 4 gates for `queue next`. A queued input
  that never runs is a lost-input **FAIL**, as is a second drained turn.

The pair of turns must never both be lost, and no tab may end on a phantom row: the DOM
multiset must equal durable truth exactly, with no row the graph does not have.

**The concurrent-append outcome is the hard replacement gate.** Against the same idle
session head, submit two uniquely marked graph appends concurrently and retain the typed
result of each operation. Exactly one of these outcomes is acceptable:

- both appends return `Appended`, and a fresh store-backed graph read contains both markers
  exactly once on one active path in the order reported by their durable node ordinals; or
- one append returns `Appended` and the other returns the typed
  `StoreError::HeadRevisionConflict { expected, actual }`, with `actual > expected`; the
  durable graph contains the winner exactly once and contains no node or projection fragment
  from the loser.

`Ok` without both durable markers, an untyped string error, a busy-lease refusal, reversed
durable ordering, a lost winner, or any partial loser state is a **FAIL**. Save both typed
results, the active-path node ids/ordinals, and marker counts in
`06-concurrent-append.json`. This assertion is intentionally stronger than "no error
occurred": removing either append from the probe must make the marker/count gate fail.

If the head CAS rejects one turn in the API race, require that rejection to be *visible*:
the failed turn publishes a `turn:<id>:failed` product row with the fixed public copy
`turn could not be completed`, its `done` event carries `outcome: "failed"`, and every tab
drops the losing turn's optimistic user row. A loser that leaves a rendered row durable
truth does not have, or returns both pills to `idle` with nothing said, is a **FAIL**.

Then gate what this scenario exists for: after everything settles, require **both tabs
still converge** — multisets equal, and each tab's per-role counts reconciled against the
shared API and graph counts. Tab-vs-tab **divergence** is a genuine FAIL: an overlapping
submission must not fork the two clients' transcripts. Screenshot `06-concurrent-both.png`;
save `06-concurrent-submit.json`, `06-truth.json`, `06-dom-vs-dom.json`.

## Phase 7 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the one workbench process and its
port-derived Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Two clients, one session | both tabs' URL/rendered/API session ids equal `<S>` on one origin; all layers empty | | `00-sessions.json`, `00-empty-both.png` |
| Send fans out once | 1 user + 1 assistant row **in each tab** = API = store = 1 `turn_completed` | | `01-rows-*.json`, `01-truth.json` |
| Phase 1 DOM-vs-DOM | A's ordered rows equal B's exactly | | `01-dom-vs-dom.json`, `01-one-pair-both.png` |
| Registration fans out | the watcher A registered renders in B's rail | | `02a-triggers.json`, `02a-registered-both.png` |
| Cross-tab wake | RED from B adds exactly 1 assistant row **per tab**, 0 user rows; counts = API = store = `turn_completed` | | `02b-*.json`, `02b-after-red-both.png` |
| Work rails agree | both rails render the same rows as the one scoped `/api/work` | | `02b-work-{a,b,api}.json` |
| Acting tab busy | A `running` with subtitle `starting turn` / `running · Ns` | | `03-watcher-convergence.json` |
| Watcher convergence | B reaches `running` without acting, within the bound; latency recorded | | `03-watcher-convergence.json`, `03ab-inflight-both.png` |
| Convergence **mechanism** | B's subtitle `turn running · restored` **and** stack `applyStateSnapshot ← recoverFromState ← recoverTerminalReplacement` | | `03-b-busylog.json` |
| Watcher guard arms | on convergence B's send disables and inject/queue-next enable | | `03-watcher-convergence.json` |
| Turn end agreed | both pills reach `idle` off the shared `done` event | | `03c-settled-both.png` |
| Wake start uses the same path | both pills `running`; over the whole wake turn neither tab records `agent running`, both record only `turn running · restored` | | `03d-wake.json`, `03d-wake-both.png` |
| Cross-tab queue-next | B's queued marker runs in exactly one drained turn and renders in both tabs | | `04-*.json`, `04-after-queued-both.png` |
| Reload convergence | post-reload B multiset equals A's; pre-reload rows all survive; A unaffected | | `05-multiset-*.json`, `05-converged-both.png` |
| Concurrent submission queued | `/api/turn` answers `queued: true` with a `next_turn` receipt; `active_turns` stays at 1; no second workflow; B's marker pending as `deferred_next_turn`; no optimistic user row for it in either tab | | `06-concurrent-submit.json`, `06-truth.json` |
| Queued send answered | the queued marker runs as exactly one drained turn: 1 committed user row + 1 assistant row, in **both** tabs; nothing lost | | `06-truth.json`, `06-concurrent-both.png` |
| Refused turn is visible | any fence-refused turn renders `turn could not be completed`, its `done` carries `outcome: "failed"`, and its optimistic row is gone from **every** tab | | `06-concurrent-submit.json`, `06-dom-vs-dom.json` |
| Concurrent submission: no fork | the two tabs agree exactly, and neither holds a row durable truth lacks | | `06-dom-vs-dom.json`, `06-concurrent-both.png` |
| Three-layer cross-check | every conversation-changing step reconciles each tab's DOM vs the shared durable state vs the trace, pairwise | | all `*-truth.json` |
| Divergence attribution | on any mismatch, both tabs' rows, the shared durable counts, and the dissenting tab are recorded | | `*-dom-vs-dom.json` |

**Aggregate:** with two independent browser clients attached to one session, did every
unit of conversational work — a send from A, a trigger press from B, a queue-next from B,
a reload of B mid-stream, and a pair of overlapping turns — reach **both** clients exactly
once, leaving the two transcripts identical and both consistent with the single durable
conversation; and does the busy pill behave exactly as the busy-state contract says, by
exactly the mechanism it names?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
