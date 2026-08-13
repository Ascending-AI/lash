# E2E Scenario: Workbench Trigger Lifecycle Beyond First Fire

> **Read [../RULES.md](../RULES.md) first** — especially browser automation, polling,
> named-checkpoint screenshots, real-token use, port-derived stacks, Abort/RCA, and
> teardown ownership. This scenario extends the inbox world from
> [../workbench-inbox-world/runbook.md](../workbench-inbox-world/runbook.md).

**Purpose.** Prove that a mail-triggered forwarding concierge remains safe and operable
after its first delivery: it fires repeatedly without looping, can be disabled and
re-enabled, can be deleted, and handles a fire during a foreground turn without taking
over that turn's ingress.

**Mid-turn contract.** Trigger occurrence dispatch and its durable process may run while
the session has a foreground turn. Any resulting session wake is durable queued work; it
must not submit a competing turn while that foreground turn owns ingress. After the turn
terminalizes and releases ingress, the queued wake is claimed as the next turn. The
implementation seam is `WorkbenchQueuedWorkSubmitter::run_queued_work` in
[`state.rs`](../../examples/agent-workbench/src/main_sections/state.rs), with the release
and re-claim in `terminalize_turn_execution` in
[`restate.rs`](../../examples/agent-workbench/src/restate.rs). The deterministic companion
gate is
`tests::button_trigger_lifecycle_stays_visible_and_queues_wakes_during_active_turn` in
[`trigger_lifecycle.rs`](../../examples/agent-workbench/src/main_sections/tests/trigger_lifecycle.rs).

**Real tokens.** OpenRouter authors and runs the trigger process. No exact model prose or
exact generated Lashlang is an answer key; the trigger API, inbox API, active-turn API,
and work registry are.

## Scenario-specific golden rules

1. **Capture the registration reference key.** After registration, save
   `GET /api/triggers`. Every lifecycle mutation must affect that same
   `subscription_key`; a replacement registration is not evidence of re-enable.
2. **Silence means no delivery and no work.** For disabled and deleted probes, require
   both no personal-inbox copy and no new process id. Use the completed causal-fence turn
   described below; never use a blind sleep as evidence of absence.
3. **Count copies, not prose.** Each enabled work-inbox marker must produce exactly one
   personal-inbox copy. The concierge may also run once for its own personal-inbox
   emission and no-op on its account filter. That bounded extra run is the loop-breaker
   working; a second copy or an unbounded-growing work rail is a failure.
4. **Mid-turn work may start, ingress may not fork.** During the overlap gate, the
   original turn address must remain the only entry in `active_turns` while `/api/work`
   gains the trigger process. A second active address is an ingress-seam violation.
5. **UI and API lifecycle state must agree.** The registration rail's action and enabled
   styling must match `GET /api/triggers`. After deletion, the subscription key must be
   absent from both surfaces.

## Working material

- Require `OPENROUTER_API_KEY`. Boot an empty, port-isolated stack with
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. Teardown, including Restate, is
  `just agent-workbench-down <port>` on success or Abort.
- UI affordances: chat/accounts tabs, account cards and compose forms, transcript,
  running/idle pill, right-hand work rail, and left-hand **registrations** rail with
  **disable**, **re-enable**, and **delete**.
- Backend truth: `GET /api/state`, `GET /api/triggers`,
  `PUT /api/triggers/{subscription_key}/enabled` with `{ "enabled": false|true }`,
  `DELETE /api/triggers/{subscription_key}`, `GET /api/accounts/{slug}/inbox`,
  `GET /api/work`, and `GET /api/work/{process_id}/await`.
- The account compose form posts mail to `/api/accounts/{slug}/messages` as
  `{"title":"<title>","text":"<text>"}`; both `title` and `text` are required.
- Before judged execution, the deterministic companion should be green:
  `cargo test -p agent-workbench button_trigger_lifecycle_stays_visible_and_queues_wakes_during_active_turn`.

Save every API response named below under the run's artifact directory.

## Phase 0 — Boot and build the inbox world

Boot, gate `/healthz`, open the browser, and require the chat pane, trigger buttons, an
empty registrations rail, and an empty work rail. Screenshot `00-fresh.png`.

In **accounts**, add `Work` and `Personal`. Poll `GET /api/accounts` until the `work` and
`personal` slugs exist and both cards render. Save the response and screenshot
`01-inbox-world.png`.

## Phase 1 — Register one forwarding concierge

In chat, ask for the outcome, not Lashlang: register one trigger named
`lifecycle-forwarder` that copies each message received by `work` into `personal`, with
an account filter so the personal emission is a no-op. Wait for the turn to settle.

Poll `GET /api/triggers` until it returns exactly one enabled registration named
`lifecycle-forwarder`. Save `02-registration.json`, record its `subscription_key`,
`subscription_id`, source type, and source configuration, and require the registrations
rail to show the same name with a **disable** action. Screenshot `02-registered.png`.

## Phase 2 — Fire repeatedly and gate the loop-breaker

Record the baseline personal inbox and process-id set. From the `work` compose form,
deliver two messages sequentially with unique titles
`FIG425-LIFE-N1-<run-id>` and `FIG425-LIFE-N2-<run-id>`. Do not send a chat turn between
delivery and forwarding.

For each marker, poll the work inbox for the original and the personal inbox for exactly
one traceable copy. Await each newly observed process through
`GET /api/work/{process_id}/await` and require a terminal success. At the end require:

- both markers occur exactly once in the personal inbox;
- the work rail and `GET /api/work` agree on the new process ids;
- the number of new runs is bounded to one or two per source delivery; and
- the process-id set stops growing once every observed run is terminal.

Save `03-repeat-work.json` and both inbox responses. Screenshot both inbox cards as
`03-repeat-inboxes.png` and the work rail as `04-repeat-work-rail.png`.

## Phase 3 — Disable, provoke, and prove silence

Click **disable** on the captured registration. Poll until `GET /api/triggers` shows the
same subscription key with `enabled: false` and the rail changes to **re-enable**. Save
`05-disabled-registration.json` and screenshot `05-disabled.png`.

Record the process-id set, then deliver `FIG425-LIFE-DISABLED-<run-id>` into `work`.
Require the original in the work inbox. Establish a causal fence by sending a short chat
turn containing `FIG425-LIFE-DISABLED-FENCE-<run-id>` and waiting until that user row and
its assistant reply are committed, the UI is idle, and `/api/state.active_turns` is
empty. Now require the disabled marker to be absent from `personal` and the process-id
set to be unchanged. Save `06-disabled-state.json`, the two inbox responses, and the work
response; screenshot `06-disabled-silent.png` with the original visible and no copy.

## Phase 4 — Re-enable the same registration and fire again

Click **re-enable**. Poll until the same captured subscription key is `enabled: true`; no
new subscription key may appear. Save `07-reenabled-registration.json`.

Deliver `FIG425-LIFE-REENABLED-<run-id>` into `work`. Poll for exactly one copy in
`personal`, await the new process run(s), and require the work rail/API to agree. Save
the inbox and work responses; screenshot `07-reenabled-fired.png`.

## Phase 5 — Fire during a foreground turn

Record the process-id set. Submit a foreground prompt containing
`FIG425-LIFE-MIDTURN-CHAT-<run-id>` that requires `web.search`, making the overlap
observable. Poll until `/api/state.active_turns` contains exactly one address and save it
as `08-active-before-trigger.json`. The busy pill should remain running because trigger
dispatch no longer emits a session `Done` while a foreground turn is active, but it is
corroborating UI only: `active_turns` and `/api/work` are the authoritative overlap gate.

While `/api/state.active_turns` still contains that exact address, deliver
`FIG425-LIFE-MIDTURN-MAIL-<run-id>` into `work`. Poll until `/api/work` gains the
concierge process **while** `/api/state.active_turns` still contains exactly the original
address. Save both responses at that instant. Require no second active address. This is
the first half of the ingress contract: occurrence/process work can land immediately,
but it cannot take over foreground ingress. Screenshot `08-midturn-overlap.png` with the
running pill and new work item visible.

Then poll until the original turn commits and `active_turns` empties, the new process is
terminal, exactly one personal copy exists, and any resulting queued wake has drained as
the next turn. Save `09-midturn-settled-state.json` and `09-midturn-work.json`; screenshot
the newest transcript and work rail as `09-midturn-settled.png`. A process that appears
only after the foreground turn is not a failure, but it does not satisfy the overlap
gate; the deterministic companion remains the authoritative scheduler gate.

## Phase 6 — Delete, provoke, and prove permanent silence

Click **delete**, accept the confirmation, and poll until the captured subscription key
is absent from `GET /api/triggers` and the rail reads `none in this session`. Save
`10-deleted-registration.json`; screenshot `10-deleted.png`.

Record the process-id set and deliver `FIG425-LIFE-DELETED-<run-id>` into `work`. Use a
new completed chat fence exactly as in Phase 3. Require the original in `work`, no copy
in `personal`, and no new process id. Save the state, work, and inbox responses;
screenshot `11-deleted-silent.png`.

## Phase 7 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench and its port-derived
Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot/world | `/healthz` 200; `work` and `personal` agree in UI/API | | `00-fresh.png`, `01-inbox-world.png` |
| Registration identity | one enabled registration has the same `subscription_id` in the rail and `/api/triggers` | | `02-registered.png`, `02-registration.json` |
| Repeated fires | two originals yield exactly two copies; bounded terminal runs | | `03-repeat-inboxes.png`, `04-repeat-work-rail.png` |
| Disable silence | same reference key disabled; fenced probe creates no copy or process | | `05-disabled.png`, `06-disabled-silent.png` |
| Re-enable | same reference key enabled and next probe forwards exactly once | | `07-reenabled-fired.png`, API artifacts |
| Mid-turn ingress | process observed with the one original active address; queued wake drains after settle | | `08-midturn-overlap.png`, `09-midturn-settled.png`, state JSON |
| Delete silence | reference key absent; fenced probe creates no copy or process | | `10-deleted.png`, `11-deleted-silent.png` |
| UI/API agreement | registration, inbox, active-turn, and work surfaces agree throughout | | screenshots + saved API responses |

**Aggregate:** did one durable concierge survive repeated fires, stop atomically when
disabled or deleted, resume under the same identity, break its own feedback loop, and
respect foreground-turn ingress when it fired mid-turn?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
