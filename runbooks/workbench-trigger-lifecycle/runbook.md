# E2E Scenario: Workbench Host-Trigger Lifecycle

> Read [../RULES.md](../RULES.md) and the
> [host-trigger evidence procedure](../host-trigger-evidence.md) first.

**Purpose.** Judge repeated mail delivery, a notice during a foreground turn,
restart recovery, and removal of a host subscription. The workbench owns mail
and cron sources; Lash supplies process starts, pinned definitions and one
session ingress. [ADR 0139](../../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md)
defines the kernel executing the authored definitions.

**Tier.** Economy, `deepseek/deepseek-v4-pro`, TypeScript, SQLite.
One judged run; count process bindings and committed inputs rather than model prose.

## Golden rules

1. Capture the list record's `id` and `owner_session`; every fire and removal
   must concern that subscription. An unrelated turn cannot replace it.
2. One occurrence and subscription bind exactly one process; one process end
   admits exactly one notice input under `process-end:<pid>`.
3. A foreground turn retains one ingress owner while process work starts.
   Inspect pending notice admission and its eventual committed row; no client
   may manufacture a competing foreground turn.
4. Only pinned rows dispatch. Removal stops dispatch before releasing the pin.
5. A normal restart is continuity evidence. The deterministic companion owns
   the precise crash between start and bind; a graceful restart cannot prove it.
6. No delivery to the removed id after deletion. Use host-table evidence and
   the causal fence, not an unscoped work-list count.

## Deterministic companions

Run each full selector once on the workbench's SQLite unit target:

```sh
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --test_arg=tests::host_primitives_tests::a_delivery_that_crashed_between_start_and_bind_starts_one_process_and_binds_at_boot \
  --test_arg=tests::host_primitives_tests::a_process_end_notice_retried_after_a_lost_acknowledgement_reaches_the_session_once \
  --test_arg=tests::host_primitives_tests::a_session_tombstoned_before_its_registrations_were_removed_loses_them_at_boot \
  --test_arg=tests::host_primitives_tests::a_delivery_that_cannot_start_is_recorded_failed_and_not_retried \
  --test_arg=tests::host_primitives_tests::a_registration_left_pending_dispatches_nothing_and_is_pinned_at_boot \
  --test_arg=tests::host_primitives_tests::a_removal_that_crashed_before_its_release_dispatches_nothing_and_finishes_at_boot \
  --test_arg=tests::host_primitives_tests::a_cron_tick_is_recorded_once_across_a_restart_after_its_occurrence_was_pruned
```

Expect **7 executed, 7 passed**. These laws in
[host_primitives.rs](../../examples/agent-workbench/src/main_sections/tests/host_primitives.rs)
pin recovery, notice deduplication, tombstone cleanup, bounded start failure,
pin admission, removal recovery and cron tick deduplication. Cron crash safety
is companion evidence; the paid browser arm below judges mail, not cron timing.
A failing law is a product finding, never grounds to weaken it.

## Phase 0 — Boot and register

Use the shared setup. Save `00-startup.json`, `00-state.json` and
`00-ready.png`. Create Work, then ask the agent to register **lifecycle-watch**
on the mail source, returning the event title from a short process with no
mail-sending effect. Ask it to acknowledge each process-end notice briefly and
finish replies through `chat.reply`. Require one subscription owned by `<S>`
with `source.kind = mail`; save `01-registration.json` and `01-registered.png`.
Send an unrelated calculation turn, settle, and require the captured
registration unchanged. Save `01-unrelated-state.json` and `01-unrelated.png`.

## Phase 1 — Fire twice

Deliver titles `LIFE-ONE-<run-id>` and `LIFE-TWO-<run-id>` sequentially from
Accounts, settling the first before the second. Save each source occurrence,
subscription binding and process await result as `02-delivery-*.json`.
Require two distinct process ids, one per occurrence, each completing with its
title. Each end must reach `<S>` once, with one notice input and one sealed
reply. Save `02-state.json`, the replay/trace extract, `02-mail.png`, and
`02-notices.png`. A second occurrence is new work, not a redelivery probe.

## Phase 2 — Fire during a turn

Start a foreground request using the Parallel search tool; ask for several
lookups so the running boundary is observable. Save its one active address.
While it remains active, deliver `LIFE-MID-<run-id>` and record the new host
binding and pending notice. Require the active foreground address not to be
replaced or duplicated. Settle the foreground and notice, then require its
composer answer preserved and the notice input consumed once. Save
`03-overlap.json`, `03-settled.json`, `03-running.png`, and `03-settled.png`.
If no overlap is observed within the bound, mark that gate FAIL, not PASS.

## Phase 3 — Replace the host

Save subscription, occurrence bindings and canonical transcript identities.
With the same data and run directories run
`bash scripts/agent-workbench-dev.sh restart --port <port>`.
Require a new PID, `/healthz`, the same subscription id, unchanged existing
bindings and no duplicated or lost committed notice. The mock Accounts world
is process-local: reopen Accounts and reconnect Work after replacement if its
list is empty. This restores the test source without changing the persisted
subscription. Deliver
`LIFE-AFTER-RESTART-<run-id>`; require one new process and one new notice.
Save `04-before.json`, `04-after.json`, `04-delivery.json`, and
`04-restarted.png`. Cite the crash companion separately for start/bind recovery.

## Phase 4 — Remove and fence

Cancel the captured subscription on the Triggers page. Require its id absent
from the API and DOM. Deliver `LIFE-DELETED-<run-id>`, confirm the mail exists,
then complete a short composer fence turn. Its occurrence must have no delivery
for the deleted subscription and create no process for it. Save
`05-deleted.json`, `05-fenced.json`, `05-deleted.png`, and `05-silent.png`.
Stop the stack with the shared teardown, also on Abort.

## Scorecard

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| Boot and register | judged geometry, model, owner and source; stable id after unrelated turn | | `00-*`, `01-*` |
| Repeat fires | two occurrences, two distinct process bindings, successful title outputs | | `02-delivery-*`, `02-mail.png` |
| Notice identity | one committed input and sealed reply per process; replay agrees with DOM | | `02-state.json`, `02-notices.png`, replay/trace |
| Mid-turn delivery | original foreground address retained; notice consumed once; prior answer preserved | | `03-*` |
| Restart continuity | new PID, same subscription and bindings, one subsequent delivery | | `04-*` |
| Removal silence | id absent, post-delete occurrence has zero deliveries to that id | | `05-*` |
| Crash and cron companions | all seven named laws executed and passed | | Kiln receipt |
| Teardown and cost | host stopped; model usage/cost and any unavailable cost explained | | run receipt |

Judge whether the registration, its deliveries and the resulting notices stay
understandable through these transitions. On Abort fill later items NOT RUN
and preserve the failure; do not repeat the paid run.
