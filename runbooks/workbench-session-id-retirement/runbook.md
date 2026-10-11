# E2E Scenario: Workbench Session ID Retirement — Delete, Refuse, Rotate

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the session-id retirement scenario.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `just agent-workbench-restart <port>`, which keeps the
> application data. It is verified: the phases below execute it, and no step
> of this row is blocked any more. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive reset.

**Purpose.** Prove that a durable session id is single-use. Populate one named session,
delete it through the Workbench API, and require every attempt to reopen that exact id to
fail with the typed HTTP 409 retirement response and its explanatory message on the
rendered surface. Then prove the Workbench's rotated id is a fresh, fully usable session
that inherited no transcript, trigger registration, or scoped work, and that replacing
the web process does not resurrect the retired id.

**Why this matters.** Deletion is a permanent identity fence, not a request to clear a
name for reuse. Effect journals, revocation ledgers, and queued input can only remain unambiguous if a tombstoned id is never admitted again. The host must make
the refusal intelligible and rotate to a new id rather than silently manufacturing a
second lifetime behind the old name.

**Real tokens.** Turns use OpenRouter. Gate on the operator's literal markers, the
assistant rows that repeat them, registration records, process identity, cancellation
receipts, the rotated id, and the typed retirement response — never on model prose.

## Scenario-specific golden rules

1. **One named id is retired exactly once.** Choose the id before boot, persist it in the
   fresh Workbench data directory's `session-id` file, and carry it explicitly as
   `?session_id=<retired-id>` through the population and delete requests. Record the
   different id returned by deletion as `<rotated-id>`. Never substitute one for the
   other in a gate.
2. **Retirement is a typed refusal, not an empty view.** Opening `<retired-id>` after
   deletion through state, observations, turn submission, or turn-input submission must
   return HTTP 409 with an `error` containing the id and
   `was used and deleted; session ids cannot be reused in this store`. The Workbench page
   scoped to that id must render the same explanatory refusal. A 200 with an empty
   transcript, a generic 500, or a generic rendered error is a contract violation at
   HTTP error mapping or render → Abort/RCA.
3. **Rotation is proved at three surfaces.** The delete response's
   `settings.session_id`, the rendered Workbench session id after navigation, and
   `<data-dir>/session-id` must all equal `<rotated-id>` and differ from
   `<retired-id>`.
4. **The rotated session must be usable, not merely empty.** It must admit and settle a
   turn, register and deliver a fresh trigger, start and settle fresh durable work, and
   let Stop cancel the correct live turn. A refusal, a turn that never starts, or an
   unknown-or-revoked control result is a contract violation → Abort/RCA.
5. **Prove markers through assistant output.** The user's prompt contains each marker and
   cannot satisfy a gate. Require the corresponding assistant row in both the rendered
   timeline and `/api/state`.
6. **All work reads are session-scoped.** Every work-list request in this scenario is
   `GET /api/work?session_id=<id>`. Never issue an unscoped `/api/work` read: it is a
   runtime-wide view and cannot prove inheritance or isolation.
7. **Host-side view caches are not session state.** The execution explorer and mock mail
   world are process-wide UI material. Only `/api/state`, `/api/triggers`, scoped
   `/api/work`, the delete/refusal responses, and durable storage are evidence.
8. **A restart may not weaken the tombstone.** Replacing the web process must preserve
   `<rotated-id>` as the current usable session and must still refuse `<retired-id>` with
   the same typed 409.

## Deterministic companions

```sh
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --test_arg=tests::host_primitives_tests::a_retired_session_leaves_no_registration \
  --test_arg=tests::recoverable_chat_tests::retired_session_http_refusals_record_structured_admission_evidence
```

Expect **2 executed, 2 passed** on SQLite. The first pins removal of host
registrations at retirement; the second pins the typed retirement fence across
HTTP surfaces. Capture the full Kiln receipt. Use the
[host-trigger evidence procedure](../host-trigger-evidence.md) for mail fan-out,
host process bindings, typed notice provenance, and teardown. This row's model
is `deepseek/deepseek-v4-pro` at the economy tier, TypeScript, SQLite; run once.
The existing process-deletion assertions below are retained pending the audit
ruling; host-trigger delivery evidence does not settle that question.

## Working material

- Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Create one fresh
  data directory, choose `fig754-retired-<run-id>` as `<retired-id>`, and write exactly
  that value to `<data-dir>/session-id` before boot. Start one port-isolated stack:
  `AGENT_WORKBENCH_DATA_DIR=<data-dir> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. Teardown on success or Abort:
  `just agent-workbench-down <port>`.
- Browser affordances: the rendered session id, chat composer and timeline, running/idle
  pill, **stop turn**, Accounts and Triggers pages, and the work drawer (`#workToggle`). Two
  rendered literals are gates: the empty timeline reads
  `no turns yet. ask the agent something below, or see what it can watch on the triggers
  page.` and an empty registrations list reads
  `Nothing is registered yet. Ask the agent to watch a source above.`
- Backend truth for id `<S>`: `GET /api/state?session_id=<S>`,
  `GET /api/observations?session_id=<S>`, `POST /api/turn?session_id=<S>`,
  `POST /api/turn/input?session_id=<S>`, `GET /api/triggers?session_id=<S>`,
  `POST /api/accounts/work/messages?session_id=<S>`, `GET /api/work?session_id=<S>`,
  `GET /api/work/{process_id}/await`, `POST /api/turn/cancel?session_id=<S>`, and
  `DELETE /api/session?session_id=<S>`.
- Create Work in Accounts. Mail injection takes `{"title":"<marker>","text":"<body>"}`.
  One POST is one shared mail occurrence. Inspect deliveries by subscription id
  and owner in `<data-dir>/host-triggers.db`, then await their exact process ids;
  the session-scoped work drawer is an observer view, not host delivery ownership.
- Durable truth on the default SQLite stack:
  `<data-dir>/lash-sessions.db`. The live id has a `session_meta` row; the
  retired id has a `deleted_sessions` row and no live session metadata. Save query
  results as JSON artifacts rather than treating terminal output as the record.
- Trigger truth: `/api/triggers?session_id=<S>` records expose `id`,
  `owner_session`, `name`, `source`, `event_arg`, `args`, and `created_at_ms`.
  Save complete records. The returned registration result's `subscription_id`
  equals its list record's `id`. The Triggers page renders name and mail source.
- The browser's scoped fetch wrapper captures `session_id` when the page loads. After
  deletion, navigate to a new page at `/?session_id=<rotated-id>`; changing only the
  rendered label does not retarget the old page's API calls.

## Phase 0 — Boot and pin the named id

Boot, poll `/healthz`, and open `/?session_id=<retired-id>`. Require the URL parameter,
rendered session id, `/api/state?session_id=<retired-id>.settings.session_id`, and
`<data-dir>/session-id` to equal `<retired-id>`. Require one live `session_meta` row and
no `deleted_sessions` row for it. Save `00-retired-id-before-delete.json` and screenshot
`00-named-session-ready.png`.

## Phase 1 — Populate the id before retirement

Perform all four operations, polling each to a settled state before starting the next.

1. Submit a turn containing `FIG754-RETIRED-<run-id>` and ask that the marker be repeated
   verbatim. Poll to idle. Require the ordered user/assistant pair in the rendered
   timeline and `/api/state`, and require the **assistant** row in both surfaces to
   contain the exact marker.
2. Ask the agent to register **retirement-watch** on the mail source, with a
   short process returning `RETIREMENT-JOB-DONE` and no mail-sending effects.
   Ask it to acknowledge process-end notices through `chat.reply`. Settle and
   require one record with `name = retirement-watch`, `source.kind = mail`,
   `owner_session = <retired-id>`, and its id on the Triggers page. Save it as A.
3. Deliver `RETIREMENT-BEFORE-<run-id>` into Work once. Require one occurrence
   bound to registration A and one process completing with
   `RETIREMENT-JOB-DONE`; save the host rows and exact process await result.
   Require one committed notice input under `process-end:<pid>` and one sealed
   reply in the retired session. Count host delivery evidence separately from
   session-originated scoped work; record both views without assuming equality.
4. Start one more turn long enough to observe running, then press **stop turn**. Capture
   every `/api/turn/cancel` receipt the click produces: the after-step Stop answers
   `cancellation_recorded_terminal_pending`, and a second, escalated Abort follows only
   when the terminal has not committed within `STOP_ESCALATION_MS` — a Stop honoured at
   the step boundary yields exactly one receipt. Require the rendered
   `turn stopped · request <id>` to match the request id of the receipt whose committed
   terminal is cancelled — the escalated Abort's when escalation fires, the Stop's when
   it does not. A driver that waits for a second receipt that never arrives, or scores
   the rendered id against the wrong receipt, would wrongly score a FAIL.

Save `01-retired-host-deliveries.json`, `01-retired-process-await.json`,
`01-retired-state.json`, `01-retired-triggers.json`,
`01-retired-work.json`, and `01-retired-cancel.json`. Scroll the timeline and work drawer
to their newest entries and screenshot `01-retired-session-populated.png`.

## Phase 2 — Delete and record the rotation

From the page, call `DELETE /api/session?session_id=<retired-id>` exactly once. Require
HTTP 200 and a snapshot whose `settings.session_id` is a non-empty
`<rotated-id>` different from `<retired-id>`. Poll until
`<data-dir>/session-id` equals `<rotated-id>`, the durable store has exactly one
`deleted_sessions` row for `<retired-id>`, and `<retired-id>` has no live
`session_meta` row. Save the complete delete response and both storage query results as
`02-delete-and-rotation.json`.

Call the DELETE through an in-page fetch; do not substitute the rendered reset button,
which posts `POST /api/reset`. A bare DELETE fetch does not apply its returned snapshot
to the current page, so the rendered session-id label on that page does not rotate.

Do not use the delete response's empty transcript as proof of fresh state: it describes
the rotated target before the browser has independently opened and read it. Screenshot
the post-delete page as `02-delete-returned-rotation.png`. The still-rendered retired id
on that bare-fetch page is expected, not a contract violation; Phase 4 opens a page
scoped to the rotated id.

## Phase 3 — Prove the retired id is refused

Reload a browser page at `/?session_id=<retired-id>`. Require its initial
`GET /api/state?session_id=<retired-id>` to return HTTP 409 with JSON
`error` containing `<retired-id>` and the full single-use explanation from golden rule
2. Require the page to render that explanatory refusal in its `.session-error` element; a
blank or absent `.session-error`, a blank
timeline, the ordinary empty-state literal, or only `internal server error` fails this
gate. Gate on that element rather than on a body-text search, so the gate is mechanically
checkable.

Issue one second `GET /api/state?session_id=<retired-id>` and require the same status and
message so the refusal is stable rather than a transient race. Then gate every accepting
or observing surface changed by the retirement fence:

1. Record `GET /api/work?session_id=<retired-id>`. The **scoped** work read is behind the
   retirement fence and must return HTTP 409 with the same canonical `error`:
   `list_work` calls `state.admit_session(&query, "api.work.list")` for the
   explicit-session form in [routes.rs](../../examples/agent-workbench/src/main_sections/routes.rs).
   Only the **default (unscoped)** work query is unfenced. Put the before/after set
   comparison on that unscoped runtime-wide read, not on the scoped one. Submit
   `POST /api/turn?session_id=<retired-id>` with a non-empty marker prompt. Require HTTP
   409 and the exact same canonical `error`; then read the unscoped work list again and
   require the complete process-id set to remain identical to the pre-turn read. Any
   accepted response, new work row, or changed existing row means work escaped the fence →
   Abort/RCA. Note also that Phase 1 awaited the triggered process to terminal and session
   deletion prunes terminal process state (the delete response's `process_retention`
   counters record the pruning), so none of the retired session's processes are expected to
   survive into either read.
2. Call `GET /api/observations?session_id=<retired-id>` without a cursor. Require HTTP
   409 JSON with the exact canonical `error`, not an NDJSON stream, empty snapshot, or
   generic failure.
3. Submit `POST /api/turn/input?session_id=<retired-id>` with
   `{"text":"FIG754-RETIRED-INPUT-<run-id>","ingress":"next_turn"}`. Require HTTP 409
   JSON with the exact canonical `error`, not an acceptance receipt.

Save the two state refusals, turn refusal, observations refusal, turn-input refusal, and
before/after scoped-work comparison as `03-retired-id-refused.json`; screenshot the
explanatory browser error as `03-retired-id-refused.png`.

## Phase 4 — Prove rotation is empty, isolated, and fully usable

Open a new page at `/?session_id=<rotated-id>`. First gate all of the fresh-state
conditions at once:

- the URL, rendered id, `/api/state.settings.session_id`, and
  `<data-dir>/session-id` all equal `<rotated-id>`;
- the timeline renders the empty-state literal and `/api/state.transcript` is empty;
- `/api/triggers?session_id=<rotated-id>` is empty and the registrations list renders
  `Nothing is registered yet. Ask the agent to watch a source above.`;
- `GET /api/work?session_id=<rotated-id>` is empty;
- `FIG754-RETIRED-<run-id>`, registration A's id and owner, and the retired process id
  are absent from every rendered and scoped API surface.

Save `04-rotated-fresh-state.json`, `04-rotated-fresh-triggers.json`, and
`04-rotated-fresh-work.json`; screenshot `04-rotated-session-empty.png`.

Then prove the rotated id is alive:

1. Submit a turn containing `FIG754-ROTATED-<run-id>` and require the ordered pair in the
   page and `/api/state`. The **assistant** row in both surfaces must contain the exact
   marker, and the turn must settle.
2. Register **retirement-watch** again on the mail source, with the same
   name, event argument and fixed result marker as Phase 1. Require one record
   owned by `<rotated-id>` whose `id` differs from A's. Its name and source
   match A; no old registration id reappears. Model-authored definitions may
   differ, so definition identity is not the isolation gate.
3. Deliver `RETIREMENT-AFTER-<run-id>` once into Work. This is shared mail-source
   fan-out: require its occurrence to select only the new registration, with no
   delivery for A or owner `<retired-id>`. Require one new differently-id'd
   process completing with `RETIREMENT-JOB-DONE` and one committed
   `process-end:<pid>` notice plus sealed reply in `<rotated-id>`. The
   subscription-owner join, not the injecting query or global process list,
   proves routing. Save host bindings and the process await result.
4. Start another long turn and press **stop turn**. As in Phase 1.4, capture every
   `/api/turn/cancel` receipt the click produces and require the rendered
   `turn stopped · request <id>` to match the request id of the receipt carrying the
   committed cancellation.

Save `04-rotated-host-deliveries.json`, `04-rotated-process-await.json`,
`04-rotated-live-state.json`, `04-rotated-live-triggers.json`,
`04-rotated-live-work.json`, and `04-rotated-live-cancel.json`. Scroll to the settled
assistant row and fresh work card; screenshot `04-rotated-session-usable.png`.

## Phase 5 — Restart preserves rotation and retirement

Run this phase only if Phase 4 completed. If any Phase-4 gate aborts, follow the shared
Abort/RCA rule, tear down, and mark every Phase-5 score item
**not run because Phase 4 aborted**. Do not restart merely to produce a second verdict
from an invalid prerequisite.

Replace the Workbench process with the verified non-destructive same-configuration restart
this runbook's header names (FIG-1164, FIG-3035):
`AGENT_WORKBENCH_DATA_DIR=<same-data-dir> AGENT_WORKBENCH_RUN_DIR=<same-run-dir> just agent-workbench-restart <port>` (equivalently
`bash scripts/agent-workbench-dev.sh restart --port <port>`). The launcher records the
stack's run metadata under `AGENT_WORKBENCH_RUN_DIR` — the boot's value, or the default
`.agent-workbench/run` when boot did not set it — and the restart refuses without it, so
re-export both variables the boot used. It keeps the managed services and the application
data, and reports that it did. Never substitute the destructive reset.
After the restart, poll `/healthz`. Require:

- a new Workbench PID;
- `<data-dir>/session-id`, the default `/api/state` response, and a browser page at
  `/?session_id=<rotated-id>` still identify `<rotated-id>`;
- the rotated transcript renders its Phase-4 rows in order, with no retired marker,
  registration, or process appearing in the rotated scoped surfaces;
- `GET /api/state?session_id=<retired-id>` still returns HTTP 409 with the same
  explanatory error, and a page scoped to `<retired-id>` still renders it.

Save `05-restart-rotated-state.json` and `05-restart-retired-refusal.json`. Screenshot the
restored rotated page as `05-rotated-after-restart.png` and the repeated refusal as
`05-retired-still-refused.png`.

## Phase 6 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the Workbench process is gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Named session | URL, rendered, API, disk, and live metadata ids agree | | `00-retired-id-before-delete.json`, `00-named-session-ready.png` |
| Populated before delete | assistant marker, registration, one new process settling the marker, and cancellation | | `01-*` |
| Deleted and rotated | 200 response returns a different id; disk rotates; tombstone persists | | `02-*` |
| Retired id refused | repeated state HTTP 409 and rendered single-use explanation | | `03-retired-id-refused.*` |
| Retired work refused | turn, observations, and turn-input return the same 409; scoped work is unchanged | | `03-retired-id-refused.json` |
| Rotation inherited nothing | empty transcript, triggers, and scoped work; retired identities absent | | `04-rotated-fresh-*` |
| Rotation is usable | assistant marker, fresh owned mail registration, one new bound process and notice, no old recipient, matching cancellation | | `04-rotated-live-*`, `04-rotated-session-usable.png` |
| Deterministic companions | two full-path laws executed and passed | | Kiln receipt |
| Restart preserves both rules | new PID, rotated session restored, retired id still refused | | `05-*` or not-run reason |

**Aggregate:** did deletion permanently retire the named id, did the Workbench rotate to
a genuinely fresh and controllable session, and did both facts survive process
replacement?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
