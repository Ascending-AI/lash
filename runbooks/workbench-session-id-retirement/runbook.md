# E2E Scenario: Workbench Session ID Retirement — Delete, Refuse, Rotate

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the session-id retirement scenario.

**Purpose.** Prove that a durable session id is single-use. Populate one named session,
delete it through the Workbench API, and require every attempt to reopen that exact id to
fail with the typed HTTP 409 retirement response and its explanatory message on the
rendered surface. Then prove the Workbench's rotated id is a fresh, fully usable session
that inherited no transcript, trigger registration, or scoped work, and that replacing
the web process does not resurrect the retired id.

**Why this matters.** Deletion is a permanent identity fence, not a request to clear a
name for reuse. Effect journals, revocation ledgers, queued input, and Restate state can
only remain unambiguous if a tombstoned id is never admitted again. The host must make
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

## Working material

- Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Create one fresh
  data directory, choose `fig754-retired-<run-id>` as `<retired-id>`, and write exactly
  that value to `<data-dir>/session-id` before boot. Start one port-isolated stack:
  `AGENT_WORKBENCH_DATA_DIR=<data-dir> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. Teardown on success or Abort:
  `just agent-workbench-down <port>`.
- Browser affordances: the rendered session id, chat composer and timeline, running/idle
  pill, **stop turn**, Red/Blue trigger buttons, registrations rail, and work rail. Two
  rendered literals are gates: the empty timeline reads
  `no turns yet. ask the agent something below, or click red or blue to fire a trigger
  occurrence.` and an empty registrations rail reads `none in this session`.
- Backend truth for id `<S>`: `GET /api/state?session_id=<S>`,
  `GET /api/observations?session_id=<S>`, `POST /api/turn?session_id=<S>`,
  `POST /api/turn/input?session_id=<S>`, `GET /api/triggers?session_id=<S>`,
  `POST /api/button-trigger?session_id=<S>`, `GET /api/work?session_id=<S>`,
  `GET /api/work/{process_id}/await`, `POST /api/turn/cancel?session_id=<S>`, and
  `DELETE /api/session?session_id=<S>`.
- The button-trigger request body is exactly `{"button":"Red"}` or
  `{"button":"Blue"}`; the `button` value is case-sensitive, so lowercase `red` or
  `blue` returns HTTP 422.
- Durable truth on the default SQLite stack:
  `<data-dir>/lash-sessions/durable-core.db`. The live id has a `session_meta` row; the
  retired id has a `deleted_sessions` row and no live session metadata. Save query
  results as JSON artifacts rather than treating terminal output as the record.
- Trigger truth: `/api/triggers?session_id=<S>` records expose
  `subscription_id`, `subscription_key`, `incarnation`, `revision`, and registrant
  scope. Save complete records.
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
2. Ask the agent to register a trigger named `retirement-watch` for the Blue host button
   that starts a durable process labelled `retirement_job`. Poll to idle and require
   exactly one enabled registration in `/api/triggers?session_id=<retired-id>` and the
   registrations rail. Record the complete registration as registration A.
3. Activate Blue. Poll `GET /api/work?session_id=<retired-id>` until exactly one new
   `retirement_job` process appears; record its id and await
   `/api/work/{process_id}/await` until terminal.
4. Start one more turn long enough to observe running, then press **stop turn**. Require
   rendered `turn stopped · request <id>` and a cancel receipt whose committed terminal
   is cancelled and whose request id matches.

Save `01-retired-state.json`, `01-retired-triggers.json`,
`01-retired-work.json`, and `01-retired-cancel.json`. Scroll the timeline and work rail
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

Do not use the delete response's empty message list as proof of fresh state: it describes
the rotated target before the browser has independently opened and read it. Screenshot
the post-delete page as `02-delete-returned-rotation.png`. The still-rendered retired id
on that bare-fetch page is expected, not a contract violation; Phase 4 opens a page
scoped to the rotated id.

## Phase 3 — Prove the retired id is refused

Reload a browser page at `/?session_id=<retired-id>`. Require its initial
`GET /api/state?session_id=<retired-id>` to return HTTP 409 with JSON
`error` containing `<retired-id>` and the full single-use explanation from golden rule
2. Require the page to render that explanatory refusal in an error row; a blank
timeline, the ordinary empty-state literal, or only `internal server error` fails this
gate.

Issue one second `GET /api/state?session_id=<retired-id>` and require the same status and
message so the refusal is stable rather than a transient race. Then gate every accepting
or observing surface changed by the retirement fence:

1. Record `GET /api/work?session_id=<retired-id>` and require its process-id set to equal
   the saved Phase-1 retired work set. Submit
   `POST /api/turn?session_id=<retired-id>` with a non-empty marker prompt. Require HTTP
   409 and the exact same canonical `error`; then read scoped work again and require the
   complete process-id set to remain identical. Any accepted response, new work row, or
   changed existing row means work escaped the fence → Abort/RCA.
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
- the timeline renders the empty-state literal and `/api/state.messages` is empty;
- `/api/triggers?session_id=<rotated-id>` is empty and the registrations rail renders
  `none in this session`;
- `GET /api/work?session_id=<rotated-id>` is empty;
- `FIG754-RETIRED-<run-id>`, registration A's identity fields, and the retired process id
  are absent from every rendered and scoped API surface.

Save `04-rotated-fresh-state.json`, `04-rotated-fresh-triggers.json`, and
`04-rotated-fresh-work.json`; screenshot `04-rotated-session-empty.png`.

Then prove the rotated id is alive:

1. Submit a turn containing `FIG754-ROTATED-<run-id>` and require the ordered pair in the
   page and `/api/state`. The **assistant** row in both surfaces must contain the exact
   marker, and the turn must settle.
2. Register `retirement-watch` for the same Blue source and `retirement_job` target.
   Require exactly one enabled registration whose registrant is scoped to
   `<rotated-id>`. Its derived display name and `subscription_key` must equal
   registration A's, while its `subscription_id` must differ. Same-name, same-key
   registration is correct across the retired and rotated owner scopes; the distinct
   global id is the isolation gate.
3. Activate Blue. Require exactly one new `retirement_job` process from
   `GET /api/work?session_id=<rotated-id>`, require its id to differ from the retired
   session's process id, and await it to terminal.
4. Start another long turn and press **stop turn**. Require rendered
   `turn stopped · request <id>` and a matching committed cancellation receipt.

Save `04-rotated-live-state.json`, `04-rotated-live-triggers.json`,
`04-rotated-live-work.json`, and `04-rotated-live-cancel.json`. Scroll to the settled
assistant row and fresh work card; screenshot `04-rotated-session-usable.png`.

## Phase 5 — Restart preserves rotation and retirement

Run this phase only if Phase 4 completed. If any Phase-4 gate aborts, follow the shared
Abort/RCA rule, tear down, and mark every Phase-5 score item
**not run because Phase 4 aborted**. Do not restart merely to produce a second verdict
from an invalid prerequisite.

Run
`AGENT_WORKBENCH_DATA_DIR=<same-data-dir> just agent-workbench-restart <port>` and poll
`/healthz`. Require:

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

Run `just agent-workbench-down <port>` and confirm the Workbench process and its
port-derived Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Named session | URL, rendered, API, disk, and live metadata ids agree | | `00-retired-id-before-delete.json`, `00-named-session-ready.png` |
| Populated before delete | assistant marker, registration, terminal process, and cancellation | | `01-*` |
| Deleted and rotated | 200 response returns a different id; disk rotates; tombstone persists | | `02-*` |
| Retired id refused | repeated state HTTP 409 and rendered single-use explanation | | `03-retired-id-refused.*` |
| Retired work refused | turn, observations, and turn-input return the same 409; scoped work is unchanged | | `03-retired-id-refused.json` |
| Rotation inherited nothing | empty transcript, triggers, and scoped work; retired identities absent | | `04-rotated-fresh-*` |
| Rotation is usable | assistant marker, fresh registration/process, and matching cancellation | | `04-rotated-live-*`, `04-rotated-session-usable.png` |
| Restart preserves both rules | new PID, rotated session restored, retired id still refused | | `05-*` or not-run reason |

**Aggregate:** did deletion permanently retire the named id, did the Workbench rotate to
a genuinely fresh and controllable session, and did both facts survive process
replacement?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
