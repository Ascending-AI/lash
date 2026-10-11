# E2E Scenario: Composer and Mail-Trigger Notice Projection

> Read [../RULES.md](../RULES.md) and the
> [host-trigger evidence procedure](../host-trigger-evidence.md) first.

**Purpose.** Judge whether a composer input and a host process-end notice each
project once, with their committed identities and order preserved across the
API, observation replay, rendered timeline and reload. Keep code, reasoning,
and nested tool summaries attached to their canonical parent rows.

**Tier.** Economy, `deepseek/deepseek-v4-flash`, TypeScript, SQLite. Composer
and registration turns call the live provider; mail injection is host-driven.
Run this row exactly once.

## Golden rules

1. Canonical truth is `/api/state.transcript[]`. Preserve each `row_id`, typed
   provenance and suppression decision. A model can emit more than one code or
   reasoning row; reconcile actual identities rather than pinning those counts.
2. A composer input commits one user row and its run one sealed assistant
   reply. A process-end notice also commits a user row: identify it by
   `process-end:<pid>` and its input/run provenance. Do not classify its text
   as a composer action or discard it from the count.
3. Reconcile the ordered visible canonical rows with the DOM and observation
   replay after every settle. Record provisional drafts and ingress receipts
   separately. No duplicate, missing or reordered row earns credit.
4. Reload preserves every canonical row identity and content, including code
   and reasoning. Compare order as well as cardinality.
5. Compare nested tool summaries under `.code-tools > .tool`. An empty
   execution output cannot justify losing its reasoning or terminal reply.

## Deterministic companions

```sh
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --test_arg=tests::projection_suites_tests::canonical_rows_survive_every_registered_production_renderer \
  --test_arg=tests::host_primitives_tests::a_process_end_notice_retried_after_a_lost_acknowledgement_reaches_the_session_once
```

Expect **2 executed, 2 passed**. The renderer law executes the registered
production blocks against a durable transcript fixture. The host law resends
notices after a lost cursor acknowledgement and checks one committed notice,
with no second pending input. Shared laws already run for another row in this
shift are cited by their receipt, not executed twice. Neither companion
claims that a paid browser journey ran.

## Phase 0 — Boot and scope

Use the shared setup, create Work, and open one explicit session. Require an
empty canonical transcript and rendered timeline. Save `00-startup.json`,
`00-state.json`, and `00-scoped-empty.png`. Start a bounded observation replay
capture using the same session id and retain its cursor identities.

## Phase 1 — Composer

Send a prompt asking for a short calculation and the marker
`PROJECTION-COMPOSER-<run-id>` in the answer. Settle. Require one committed
composer input and one sealed reply containing that marker in the API and
DOM. Compare every actual canonical row and its order, including any reasoning
and code. Save `01-state.json`, `01-dom.json`, `01-replay.json`, a trace extract,
and `01-composer.png`.

## Phase 2 — Register, then inject one mail occurrence

Ask the agent to register **projection-watch** on the mail source, with a
short process returning the event title and no mail-sending effect. Ask it to
acknowledge a later process-end notice briefly through `chat.reply`. Settle,
require exactly one owned registration, and reconcile the new composer input,
registration code and sealed reply. Save `02-registration.json`,
`02-registered-state.json`, and `02-registered.png`.

Deliver `PROJECTION-MAIL-<run-id>` once through Accounts. Require one source
occurrence, one binding to the captured subscription, and one successful
process await result. Save `02-delivery.json` and `02-mail.png`.
Settle its notice: exactly one new user row beginning
`Triggered process <pid> ended:`, one admitted input under `process-end:<pid>`,
and one sealed answer for that run. The two prior composer answers survive.
Compare the full ordered canonical/API, replay and rendered rows; save
`02-notice-state.json`, `02-notice-dom.json`, `02-notice-replay.json`, the trace
extract and `02-notice.png`. The inbox injection itself is not a chat user row.

If an identity or projection gate fails, capture row ids, provenance, pending
inputs, host bindings and DOM identities before Abort. Never repair the
projection or continue to reload as though the preceding gate passed.

## Phase 3 — Reload

Save all canonical row ids and content, then reload with
`wait_until="domcontentloaded"`. Wait for snapshot recovery, settle, and
require the same ordered visible rows in state and DOM. Suppressed rows remain
suppressed; code tool summaries and reasoning retain their parent identity.
Save `03-state.json`, `03-dom.json`, `03-identity-comparison.json`, and
`03-reloaded.png`.

## Phase 4 — Stop and score

Use the shared teardown on success or Abort. Fill the scorecard and judge
whether the composer reply and host notice are presented clearly as distinct
inputs without concealing canonical rows or inventing duplicate ones.

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| Boot/scope | judged model/dialect/store, explicit session, empty state and timeline | | `00-*` |
| Composer projection | one input, one sealed marker reply; all canonical rows agree | | `01-*` |
| Registration | one owned mail subscription; registration turn projects once | | `02-registration.json`, `02-registered-*` |
| Mail delivery | one occurrence and process binding; successful title output | | `02-delivery.json`, `02-mail.png` |
| Notice projection | one committed notice input and sealed reply; prior answers retained | | `02-notice-*`, trace |
| Identity/order parity | canonical state, replay and rendered identities/content agree | | phase extracts |
| Reload | identical ordered visible canonical rows, reasoning and code summaries | | `03-*` |
| Companions | two named laws executed and passed, reusing shared receipt | | Kiln receipt |
| Teardown and cost | host stopped; recorded live model usage/cost | | run receipt |
