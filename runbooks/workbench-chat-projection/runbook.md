# E2E Scenario: Workbench Chat Projection Integrity

Read [../RULES.md](../RULES.md) before running this scenario. Its browser,
checkpoint, three-layer reconciliation, real-token, Abort/RCA and teardown
rules apply here.

**Purpose.** Judge the real-provider conversation across one composer send,
a watcher registration, one button wake and a reload. FIG-972 requires one
UI-owned user row for an opening input, correlated to committed input by
`provenance.turn_id`. FIG-984 requires one committed marked reply per settled
turn, independent of its termination or writer. Reasoning alongside a reply
must not hide the answer (FIG-1406).

The canonical source is `lash::transcript`. `/api/state.transcript` carries one
record for every retained committed node, including named suppressions. Visible
kinds are `user`, `assistant_reply`, `reasoning`, `tool_call`, `code_block`,
`attachment` and `event`. Each record carries an opaque node-backed `row_id`,
recorded `timestamp`, typed `provenance` and neutral `content`. The DOM tags all
pieces of a canonical row with `data-transcript-row-id` and `data-turn-id`.
The UI retains its own opening-input identity and attachments; compare its
canonical DOM tag with the committed node rather than its product message ID.

Settled answers consume `provenance.is_turn_reply`. They never select a reply
by part kind, ID spelling, output accessor or protocol payload. Live prose,
reasoning, code and tool activity are provisional. Settlement replaces the
matching turn's preview with canonical rows; a retry reset retracts abandoned
output. Reload renders those same records. Tool summaries retain only neutral
operation/status facts, with an explicit omitted count; unavailable arguments,
results, duration and stable live-call identity are not reconstructed.

Deterministic Surfaces A–E are enforced by
[scripts/workbench-transcript-projection-e2e.py](../../scripts/workbench-transcript-projection-e2e.py)
and the shared production-JavaScript harness. This judged runbook owns the
real-provider Phases 0–4 below.

## Scenario-specific golden rules

1. **Count rows, never read prose.** A rendered row is a `#timeline .message` element; its
   role is the `user` / `assistant` class, not the visible `you` / `agent` label. Two rows
   whose bodies are byte-identical are still two rows — identical text is the defect
   signature, never an excuse to merge them.
2. **Reconcile all three layers at every step, and record the split.** Per
   [../RULES.md](../RULES.md), rendered DOM vs durable state vs logs must agree. Do not stop
   at "the DOM has two rows": determine whether the store has one committed assistant
   message (a **render** defect) or two (a **commit** defect projected faithfully), and
   whether the trace shows one execution or two. That split *is* the diagnosis and decides
   the pipeline stage the RCA names. Apply the narrow settlement exception in
   `RULES.md` for a journal-first process-command refusal: the attempt frame/trace preserves
   the provider value while the intent outcome and turn/API/DOM projection carry the typed
   refusal. Count that exact, identity-matched two-row settlement as designed behavior.
3. **One turn execution per unit of work.** Exactly one `turn_completed` trace record per
   composer send and exactly one per Red press. Extra assistant rows over one
   `turn_completed` are projection duplication; extra `turn_completed` records are a
   scheduler or wake-delivery fault, a different failure with a different owner.
4. **A wake adds no user row.** The Red press is a host event, not a chat input. The wake
   turn must add exactly one assistant row and leave the user-row count unchanged. A new
   `you` row for a button press is a projection failure.
5. **Settle by stability, not by a sleep.** After the `turn_completed` and idle gates, poll
   until the row and message counts are unchanged across several consecutive samples before
   counting. A duplicate that lands a second late must still be caught; a fixed sleep either
   misses it or passes by luck.
6. **Reload must preserve every durable identity.** The post-reload message/reasoning/code
   multiset must equal the settled pre-reload multiset exactly. Tool details use the narrower
   durable contract above: retained source operation/outcome summaries plus an explicit omitted
   count. Backfill is a second, independent projection of committed state; never manufacture
   unavailable live fields to make the two paths look identical.
7. **Scope everything to one session id.** Shift `/?session_id=<S>` and scope every read —
   `/api/state?session_id=<S>`, the `graph_nodes.session_id` filter, the product-event map
   key, and the trace's `context.session_id` — to that id. An unscoped read mixes other
   tabs' conversations into the counts and voids the run.
8. **Use the browser dispatch hook for event identity.** Before navigation, install
   `window.__LASH_WORKBENCH_TURN_EVENT_HOOK__ = (event, turnId) => ...` and record a deep copy
   of every call. Correlate DOM checkpoints to that buffer. Trace vocabulary is corroboration,
   not the event-side answer key.
9. **Count nested tool rows where they live.** For each code block record
   `code.querySelectorAll(":scope > .tool")`, each badge, and each available call id from the
   hook. A timeline-child count cannot see tools. One live call must produce one child before
   and after completion even when `call_id` is absent. Reload must reproduce each retained source
   operation/outcome summary and one explicit `calls_omitted` row when the ledger overflowed;
   arguments, results, duration, and stable call identity must remain unavailable.
10. **Separate live from settled classes.** Capture transient retry/client-error/running rows at
    their named checkpoint. For the reload identity gate compare only the settled histogram,
    after `Done`, idle, empty `active_turns`, and count stability. Retry rows are turn-owned: a
    delayed `Done` for turn A must not clear turn B's retry row.

## Deterministic projection proof

Surfaces A–E are enforced by the scripted transcript projection gate. The
shared production-renderer harness checks canonical row identity, content,
suppression and typed input correlation. The deterministic Playwright scenario
reconciles DOM, `/api/state`, and graph nodes before and after reload and a
frame switch. CI runs this as the transcript projection functional gate.

This judged runbook retains the real-provider Phases 0–4 below.

## Working material

- Require `OPENROUTER_API_KEY`. Boot an empty, port-isolated stack with
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`
  (or `bash scripts/agent-workbench-dev.sh up --port <port>` with the same environment).
  Gate `GET /healthz` → 200. Teardown on success or Abort is
  `just agent-workbench-down <port>`.
- Pick one run session id `<S>` = `runbook-chatproj-<run-id>` and open `/?session_id=<S>`.
  Gate the rendered session id and `/api/state?session_id=<S>.settings.session_id` against
  `<S>` before sending anything.
- UI affordances: the chat composer and its **send** control, the transcript timeline, the
  running/idle pill, and the left-sidebar **RED** trigger button.
- **Layer 1 — rendered DOM:** `#timeline .message.user` and `#timeline .message.assistant`
  row counts, plus each row's body text. The live stream renders a streaming assistant draft
  that is replaced when the committed copy arrives; only count after the settle gate, or a
  draft inflates the count.
- **Layer 2 — durable state:** read `/api/state?session_id=<S>.transcript` and
  `<data-dir>/lash-sessions/durable-core.db`, table `graph_nodes`, filtered by
  `session_id = <S> AND tombstoned = 0`, ordered by generation. Require an exact
  node/row ID and timestamp match, including suppressed records. Count visible
  `user` rows and marked `assistant_reply` rows; raw assistant protocol records
  may be suppressed. Match DOM pieces using their canonical row tags and typed
  turn provenance. Record `/api/state.messages` and the session's product-event
  log as host evidence; host IDs may differ from canonical row IDs.
- **Layer 3 — logs:** the trace file, which is `$AGENT_WORKBENCH_TRACE` whenever that variable
  is set and only otherwise `<data-dir>/trace.jsonl`; reading the wrong one yields empty record
  lists that look like a product failure. Records with
  `context.session_id == <S>`. Count `type == "turn_completed"`; `context.turn_id` names the
  execution and distinguishes a composer turn (`workbench-turn-…`) from a wake
  (`workbench-queued-…`).
- Role vocabulary differs per surface: the store writes `User` / `Assistant`, the API
  `user` / `assistant`, the DOM classes `user` / `assistant` under `you` / `agent` labels.
  Normalize role before comparing; never treat a label difference as content drift.

Save every named artifact and API/store/trace extract under the run's artifact directory.

## Phase 0 — Boot and scope one session

Boot, gate `/healthz`, and open `/?session_id=<S>`. Require the composer, the RED trigger
button, an empty transcript, the rendered session id `<S>`, and
`/api/state?session_id=<S>` reporting `settings.session_id == <S>` with empty `messages`
and empty `active_turns`. All three layers start at zero: no `graph_nodes` conversation
rows, no `<S>` key in the product-event log, no `<S>` turn or conversation trace records
(periodic `/api/work` responses are expected and allowed). Screenshot
`00-scoped-empty.png`.

## Phase 1 — One composer send, one pair of rows

Send one short turn containing a unique literal marker, e.g.
`Reply with exactly this and nothing else: FIG985-PLAIN-<run-id> acknowledged`.

Gate, in order: one `turn_completed` for `<S>`; the idle pill and empty `active_turns`; then
the settle gate. Now require **exactly**:

- 1 rendered `.message.user` row and 1 rendered `.message.assistant` row;
- 1 `user` and 1 `assistant` message in `/api/state.messages`;
- 1 visible canonical `user` and 1 marked `assistant_reply`, each backed by its graph node;
- 1 `turn_completed` record.

Record every message id at each layer. This is the FIG-972 regression gate: a second `you`
row, or a second committed `User` message for one send, fails it. Screenshot
`01-one-send.png` and save the four extracts as `01-one-send-{dom,state,store,trace}.json`.

## Phase 2 — Register the watcher, then press RED once

**2a — register.** Send `Let me know when I press a button` and let the agent register the
button watcher. Gate a second `turn_completed`, idle, and the settle gate, then require the
same invariant cumulatively: 2 user rows, 2 assistant rows, 2 committed pairs, 2
`turn_completed`. Screenshot `02-watcher-registered.png`. A duplicate here is already a
failure — do not press RED to "get to the real test".

**2b — one press, one wake.** Press the **RED** button **exactly once**. Gate a
`turn_completed` whose `context.turn_id` starts with `workbench-queued-`, then idle, then
the settle gate. Require **exactly**:

- the user-row count **unchanged** at 2 (golden rule 4);
- 3 rendered `.message.assistant` rows — one new row, not two;
- 3 `assistant` messages in `/api/state.messages` and 3 marked canonical replies,
  each matched to a committed graph node;
- 3 `turn_completed` records — one wake, not two.

Read the wake's canonical reply using its typed turn provenance. Its text must
appear in exactly one agent row even when reasoning accompanies it. Re-read the
previous replies: a wake must neither duplicate its answer nor hide an earlier
answer. Record row IDs, provenance, content and source node IDs in
`03-red-press-{dom,state,store,trace}.json`. Screenshot
`03-after-red-press.png` with the newest rows visible.

On a surplus row, capture its canonical DOM tag and corresponding API/node
records in `03-duplicate-ids.json`. Use typed provenance and the recorded writer
to attribute it. A duplicate committed reply is a terminalization defect; extra
DOM pieces beyond the canonical renderer contract are a projection defect.
Preserve any mismatch between the layers in the RCA.

## Phase 3 — Reload and require an identical multiset

Record the pre-reload row multiset (role class + body text per row). Reload the page, gate
the rendered session id and the settle gate, and require:

- the post-reload row multiset **equals** the pre-reload multiset;
- the per-role counts still equal Phase 2b's store and trace counts;
- no reload-only and no live-only row.

After settlement both the live page and reload consume canonical rows; their
identity, content and class multisets must agree. Screenshot `04-after-reload.png` and save
both multisets as `04-reload-multiset.json`.

## Phase 4 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench and its port-derived
Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot/scope | `/healthz` 200; rendered and API session id both `<S>`; all three conversation layers empty | | `00-scoped-empty.png` |
| One send, one pair | 1 user + 1 assistant row = 1+1 API = 1+1 store = 1 `turn_completed` | | `01-one-send.png`, `01-one-send-*.json` |
| Watcher registration | cumulative 2+2 rows = 2+2 API = 2+2 store = 2 `turn_completed` | | `02-watcher-registered.png` |
| One press, one wake | exactly 1 `workbench-queued-` `turn_completed`; user rows unchanged | | `03-red-press-trace.json` |
| One press, one new agent row | 3 assistant rows = 3 API = 3 store = 3 `turn_completed` | | `03-after-red-press.png`, `03-red-press-*.json` |
| Reasoned wake reply | the marked wake reply renders in exactly one agent row and in `/api/state.messages`, with its reasoning retained | | `03-red-press-{dom,state,store}.json` |
| Reload identity | post-reload row multiset equals pre-reload multiset | | `04-after-reload.png`, `04-reload-multiset.json` |
| Three-layer cross-check | every step reconciles DOM vs durable vs trace pairwise; no mismatch normalized away | | all four extracts per phase |
| Duplicate attribution | on any surplus row, canonical DOM tags, row/node IDs, typed provenance and writers are recorded | | `03-duplicate-ids.json` |

**Aggregate:** did every unit of conversational work — two composer sends and one button
press — project into exactly one rendered row per role, identically on the live and reload
paths, with the rendered DOM, the durable session graph, the state and product-event
projections, and the turn trace all agreeing on how many messages and how many executions
the session contains?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
