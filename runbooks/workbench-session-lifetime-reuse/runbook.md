# E2E Scenario: Workbench Session Lifetime Reuse — A Recreated Name Is a New Session

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the session-lifetime scenario.

**Purpose.** Prove that a session's durable identity is its store-minted lifetime, not its
host-facing name. Delete a named session and recreate it under the **same name**: the new
lifetime must inherit no transcript and no trigger registrations, and it must be fully
usable — turns admitted, durable work started and settled, and the Stop control able to
cancel the right turn. Then prove the converse: replacing the web process reopens the
*same* lifetime rather than minting a new one.

**Why this matters.** Everything keyed to a session — effect-journal rows, durable
await-event promises, turn-control gates — now addresses a lifetime rather than a name.
Nothing that belonged to the retired lifetime may be visible to, or block, the new one.
The browser-visible consequence is simple and unforgiving: after recreation the session
must behave exactly like a brand-new one, including its control surfaces.

**Real tokens.** Turns use OpenRouter. Gate on the operator's literal markers,
registration records, process identity, cancellation receipts, and the stored lifetime id
— never on model prose.

## Scenario-specific golden rules

1. **One explicit name, everywhere.** Every request in this scenario carries
   `?session_id=<name>` and the browser tab is opened at `/?session_id=<name>`. A delete
   without that parameter rotates the workbench onto a *new* name and does not exercise
   reuse at all; a run that drops the parameter anywhere is void.
2. **The lifetime change is proved by the stored identity, not by an empty screen.** Read
   the session's `incarnation_id` before and after. Delete-then-recreate must change it;
   a web-process restart must not. An empty transcript alone is equally consistent with a
   view that simply failed to load.
3. **The recreated lifetime must be usable, not merely empty.** A turn submitted after
   recreation must be admitted and settle, a fresh trigger must deliver, and Stop must
   cancel. A turn that is refused, never starts, or reports an unknown-or-revoked control
   outcome is a contract violation at the effect host → Abort/RCA. There is no "retry the
   phase" path for this: it is the scenario's point.
4. **A trigger's stable key is not its lifetime.** Re-registering the same process/source
   deliberately reproduces the same `subscription_key`. Record each registration's
   `incarnation`; delete-then-recreate must change that field while keeping the stable key.
5. **Process survival is not browser-observable in the scoped scenario.** Explicit
   `GET /api/work?session_id=<name>` uses `snapshot_for_session` and must return no
   lifetime-A card after deletion. The shared golden rules forbid an unscoped request, so
   this runbook does not claim whether the process survived. A store/process-registry
   conformance test must prove that session deletion does not terminate an independent
   Runtime Process.
6. **Host-side view caches are not session state.** In the explicit-delete path the
   workbench keeps some process-wide UI material (the execution explorer's graphs, the
   mock mail world). Only `/api/state`, `/api/triggers`, scoped `/api/work`, and the
   durable store are evidence; stale explorer entries are not a leak.
7. **Prove markers through assistant output.** The user's own prompt contains each marker
   and cannot satisfy a recall gate. Require the corresponding assistant row in both the
   rendered timeline and `/api/state`.

## Working material

- Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot one fresh,
  port-isolated stack:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  The dev helper explicitly forwards `AGENT_WORKBENCH_DATA_DIR` to the workbench process.
  Gate `GET /healthz` → 200. Teardown on success or Abort:
  `just agent-workbench-down <port>`.
- Browser affordances: the rendered session id, the chat composer and timeline, the
  running/idle pill, **stop turn**, the Red/Blue trigger buttons, the registrations rail,
  and the work rail. Two rendered literals are used as gates: the empty timeline reads
  `no turns yet. ask the agent something below, or click red or blue to fire a trigger
  occurrence.` and an empty registrations rail reads `none in this session`.
- Backend truth for name `<S>`: `GET /api/state?session_id=<S>`,
  `POST /api/turn?session_id=<S>`, `GET /api/triggers?session_id=<S>`,
  `POST /api/button-trigger?session_id=<S>`, `GET /api/work?session_id=<S>`,
  `GET /api/work/{process_id}/await`, `POST /api/turn/cancel?session_id=<S>`, and
  `DELETE /api/session?session_id=<S>`.
- Durable truth: the `session_meta` row for `<S>` in
  `<data-dir>/lash-sessions/durable-core.db` (column `incarnation_id`); in a PostgreSQL
  boot it is the `incarnation_id` field of `lash_session_meta.meta_json`. Save extracted
  values as JSON artifacts rather than treating a terminal printout as the record.
- Trigger truth: entries from `/api/triggers?session_id=<S>` expose
  `subscription_key`, `incarnation`, and `revision`. Save the complete records.
- The workbench's own **reset** button is not the affordance under test: it deletes the
  session and rotates onto a *new* name. This scenario deletes the named session through
  the scoped endpoint above precisely so the name is reused.

## Phase 0 — Boot and pin one explicit name

Choose `fig636-lifetime-<run-id>` as the name. Boot, gate `/healthz`, and open the browser
at `/?session_id=<name>`. Require the rendered session id, the URL parameter, and
`/api/state?session_id=<name>.settings.session_id` to agree. Read the stored
`incarnation_id` for that name and save it as lifetime **A** in `00-lifetime-a.json`.
Screenshot `00-lifetime-a.png`.

## Phase 1 — Give lifetime A history, a registration, durable work, and a cancellation

Perform all four, polling each to a settled state before starting the next.

1. Submit a turn containing the literal `FIG636-LIFE-A-<run-id>` and ask that the marker
   be repeated verbatim. Poll to idle. Require the ordered user/assistant pair in both
   surfaces, and require the **assistant** row — not merely the user row — to contain the
   exact marker.
2. Ask the agent to register a trigger named `lifetime-watch` for the Blue host button
   that starts a durable process labelled `lifetime_job`. Poll to idle and require exactly
   one enabled registration in `/api/triggers?session_id=<name>` and in the registrations
   rail. Record its `subscription_key` and `incarnation` as registration A.
3. Activate the Blue button. Poll `/api/work?session_id=<name>` until exactly one new
   `lifetime_job` process appears; record its id and await it with
   `/api/work/{process_id}/await` until terminal.
4. Start one more turn long enough to observe running, then press **stop turn**. Require
   the rendered `turn stopped · request <id>` and a cancel receipt whose terminal is
   cancelled and whose request id matches.

Save `01-a-state.json`, `01-a-triggers.json`, `01-a-work.json`, and `01-a-cancel.json`;
screenshot the populated surface as `01-lifetime-a.png`. Lifetime A now owns committed
history, a registration, a durable process, journal state, and a resolved turn-control
gate — everything the new lifetime must not inherit or be blocked by.

## Phase 2 — Delete the named session and observe a new lifetime

Call `DELETE /api/session?session_id=<name>` and require a success response. Then poll
until both hold at once: `/api/state?session_id=<name>` returns an empty message list, and
the stored `incarnation_id` for the name differs from A. Record that value as lifetime
**B**, then read it a second time after one further `/api/state` round trip and require the
same value.

An id that keeps changing across those reads means deletion and recreation are racing for
the same name; that is a contract violation at store persistence → Abort/RCA. A value that
never changes from A means the delete did not retire the lifetime → Abort/RCA at store
persistence. Save `02-lifetime-b.json`.

## Phase 3 — Prove the new lifetime inherited nothing observable

Reload `/?session_id=<name>` and gate:

- the timeline renders the empty-state literal and `/api/state.messages` is empty;
- `/api/triggers?session_id=<name>` is empty and the registrations rail renders
  `none in this session`;
- the marker `FIG636-LIFE-A-<run-id>` and registration A's key/incarnation are absent from
  every rendered and scoped API surface;
- `/api/work?session_id=<name>` is empty.

The scoped empty work result proves lifetime B inherited no process visibility; it does
**not** prove that lifetime A's independent process survived. Do not issue an unscoped
request to turn that non-observable property into a browser gate. Save
`03-fresh-state.json`, `03-fresh-triggers.json`, and `03-fresh-work.json`; screenshot
`03-fresh-lifetime.png`.

## Phase 4 — Prove the new lifetime is fully alive

This is the load-bearing phase: an empty session that cannot work is a worse failure than
one that leaked.

1. Submit a turn containing `FIG636-LIFE-B-<run-id>` and require the ordered pair to render
   and to appear in `/api/state`. Require the **assistant** row in both surfaces to contain
   the exact marker. The turn must be admitted and settle within the phase timeout;
   refusal or a turn that never starts is the Abort case named in golden rule 3.
2. Ask the agent to register `lifetime-watch` for the same Blue source and
   `lifetime_job` process again. Require exactly one enabled registration whose
   `subscription_key` equals registration A's key and whose `incarnation` differs from
   registration A's incarnation.
3. Activate the Blue button. Require exactly one new `lifetime_job` process whose id
   differs from lifetime A's, and await it to a terminal outcome.
4. Start another long turn and press **stop turn**. Require the rendered
   `turn stopped · request <id>` and a cancel receipt whose terminal is cancelled — proof
   that turn control addresses the live lifetime and was not consumed or blocked by the
   retired one.

Save `04-b-state.json`, `04-b-triggers.json`, `04-b-work.json`, and `04-b-cancel.json`;
screenshot `04-lifetime-b.png` with the settled work rail visible.

## Phase 5 — A restart reopens the same lifetime

Run this phase only if Phase 4 completed. If Phase 4 aborts, follow the shared Abort/RCA
rule, tear down, and mark every Phase-5 score item **not run because Phase 4 aborted**;
do not turn a prerequisite failure into a second restart verdict.

Run
`AGENT_WORKBENCH_DATA_DIR=<same-tmp> just agent-workbench-restart <port>`, poll
`/healthz`, and reload `/?session_id=<name>`. Require:

- a new workbench PID;
- the stored `incarnation_id` for the name is **still B** — a restart must not mint a new
  lifetime;
- the transcript renders exactly the lifetime-B rows, in order, with no lifetime-A row
  reappearing;
- one further long turn can be started and stopped, rendering `turn stopped · request <id>`
  with a matching receipt.

Save `05-restart-state.json`, `05-lifetime.json`, and `05-cancel.json`; screenshot
`05-after-restart.png`.

## Phase 6 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench process and its
port-derived Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Named session | URL, rendered, and API session ids agree; lifetime A recorded | | `00-lifetime-a.*` |
| Populated lifetime A | assistant marker, one registration, one terminal process, one cancellation | | `01-*` |
| Lifetime retired | stored incarnation changes to a stable B after the scoped delete | | `02-lifetime-b.json` |
| Nothing inherited | empty transcript, registrations, and scoped work; A values absent | | `03-*` |
| Recreated name is usable | assistant marker, changed registration incarnation, fresh process id, Stop cancels | | `04-*` |
| Restart keeps the lifetime | new PID, unchanged incarnation B, lifetime-B rows only | | `05-*` or not-run reason |
| Process survival | not browser-observable through the required scoped API; conformance test required | | not scored |

**Aggregate:** did reusing a deleted session's name produce a genuinely new, fully
controllable durable session — and, if that succeeded, did replacing the process leave
that same session alone?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
