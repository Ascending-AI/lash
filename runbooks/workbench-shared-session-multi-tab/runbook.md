# E2E Scenario: Shared-Session Queue, Notice and Reload Across Two Tabs

> Read [../RULES.md](../RULES.md) and the
> [host-trigger evidence procedure](../host-trigger-evidence.md) first.

**Purpose.** Two independent browser contexts attached to one session must
converge on its ordered committed conversation. Judge a queued composer send,
a mail-trigger process-end notice, and a reload during a running turn. Each
client has its own observation subscription through `subscribe_and_recover`;
neither client may consume the other's stream or manufacture a second turn.

**Tier.** Economy, `deepseek/deepseek-v4-pro`, TypeScript, SQLite. Exactly one
judged row with two contexts on the same origin and explicit session id.

## Golden rules

1. After each settle require A's and B's ordered canonical DOM rows to equal
   each other and the scoped `/api/state.transcript`, by identity and content.
   Retain code/reasoning/tool summaries; record client drafts and ingress
   receipts separately from committed rows.
2. Both URL parameters, rendered `#sessionId` values and API session ids
   equal `<S>`. Independent browser contexts share only the server.
3. A busy composer action through **queue next** (`#queueNext`) obtains a
   `next_turn` receipt. Its pending text is visible in both clients; it cannot
   appear as a second committed user row before admission.
4. The foreground has one active address. The queued send starts once after
   the foreground settles, with one committed input and sealed marker answer.
5. A host notice is an ordinary user transcript input identified by
   `process-end:<pid>`; its row and sealed reply fan out once to both clients.
6. Reload B during a live turn. Recovery through `recoverFromSnapshot` must
   preserve earlier committed rows and converge with A after settlement.
   Gate observable busy/idle state and controls, not removed internal helpers
   or a guessed status subtitle.
7. Compare actual canonical identities, not fixed total code or reasoning
   counts. Any tab disagreement is a finding; do not reload to repair it.

## Deterministic companions

```sh
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --test_arg=tests::concurrent_send_tests::a_send_to_a_busy_session_is_admitted_as_a_queued_next_turn_input \
  --test_arg=tests::concurrent_send_tests::concurrent_host_appends_settle_through_the_command_lane_exactly_once \
  --test_arg=tests::projection_suites_tests::workbench_browser_recovery_projection_preserves_rows_and_scopes_session_cursors \
  --test_arg=tests::host_primitives_tests::a_process_end_notice_retried_after_a_lost_acknowledgement_reaches_the_session_once
```

Expect **4 executed, 4 passed**. The surviving
[concurrent_send.rs](../../examples/agent-workbench/src/main_sections/tests/concurrent_send.rs)
laws pin durable busy-send admission and exactly-once command-lane settlement;
the browser projection law pins recovery and scoped cursors. The host law pins
notice retry identity. Cite a shared law's earlier receipt rather than rerun it.

## Phase 0 — Attach two clients

Use the shared setup, create Work, and attach separate contexts A and B at
`/?session_id=<S>`. Require both timelines empty and all ids equal. Save
`00-startup.json`, `00-sessions.json`, `00-tab-a.png`, and `00-tab-b.png`.
Capture both clients' observation streams and cursor identities throughout.

## Phase 1 — One composer input fans out

Send `TABS-FIRST-<run-id>` from A, asking for a short reply containing that
marker. Settle both clients, then require one committed input and one sealed
marker reply in each. Compare every canonical rendered row in order against
state. Save `01-state.json`, `01-dom-a.json`, `01-dom-b.json`, and
`01-tab-a.png` / `01-tab-b.png`.

## Phase 2 — Queue from B while A runs

From A submit a request using several Parallel search lookups and marker
`TABS-RUNNING-<run-id>`. Poll until state has one active address and both
clients show running. In B type `TABS-QUEUED-<run-id>` with a request to repeat
it and click **queue next**. Save the receipt and pending input, require
`ingress = next_turn`, the original single active address, and one pending
queued text. Both tabs must render the pending receipt, with no premature
committed input row. Save `02-queued-state.json`, `02-queue-receipt.json`, and
`02-queued-a.png` / `02-queued-b.png`.

Settle the foreground and queued send. Require one committed user row for
the queued marker and one sealed marker reply, one new admitting run, no
pending queued input, and A = B = canonical state in order. Save
`02-settled-state.json`, both DOM/replay extracts, the trace and
`02-settled-a.png` / `02-settled-b.png`. Missing overlap or an unavailable
queue control within the timeout fails this arm.

## Phase 3 — Host notice fans out

From A ask the agent to register **tabs-watch** on the mail source, returning
the event title without sending mail. Ask it to acknowledge process-end
notices through `chat.reply`. Settle and require the same subscription id
owned by `<S>` on both Triggers pages and in the scoped API. Save
`03-registration.json` and `03-registered-a.png` / `03-registered-b.png`.

Deliver `TABS-MAIL-<run-id>` once from B's Accounts page. Save the host
occurrence, subscription binding and successful process await result as
`03-delivery.json`. Settle its notice and answer. Require one committed input
under `process-end:<pid>` and one sealed reply, rendering once in both tabs
at the same place after the prior rows. Save `03-state.json`, both DOM/replay
extracts and `03-notice-a.png` / `03-notice-b.png`.

## Phase 4 — Reload B mid-turn

From A start another search request with marker `TABS-RELOAD-<run-id>`.
Capture both busy states while it runs and B's ordered committed row ids.
Reload only B with `wait_until="domcontentloaded"`; gate its restored session
and running state before the live turn ends. A stays attached throughout.
Save `04-running.json`, `04-recovered.json`, and
`04-reloaded-b.png` / `04-running-a.png`.

After the turn settles require one new sealed marker answer, all earlier
canonical rows preserved, no duplicate ids, and identical ordered A/B/state
rows. Save `04-settled-state.json`, `04-dom-a.json`, `04-dom-b.json`,
`04-comparison.json`, and `04-settled-a.png` / `04-settled-b.png`.
Use the shared teardown on success or Abort.

## Scorecard

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| Two clients/one session | separate contexts, same explicit ids and empty timelines | | `00-*` |
| Composer fan-out | one input and sealed marker answer; A = B = state in order | | `01-*` |
| Busy queue admission | B's next-turn receipt visible in both, one active address, no early committed row | | `02-queued-*`, receipt |
| Queued send settles once | one admitted input and sealed marker reply, queue empty, ordered convergence | | `02-settled-*`, trace |
| Registration fan-out | captured subscription id visible in both scoped trigger views | | `03-registration.json`, `03-registered-*` |
| Notice fan-out | one process, one notice input and sealed reply in both tabs in order | | `03-delivery.json`, `03-state.json`, `03-notice-*` |
| Mid-turn reload | B recovers while A stays attached; prior canonical identities survive | | `04-running.json`, `04-recovered.json`, screenshots |
| Final convergence | all ordered identities/content equal, one new sealed marker answer | | `04-settled-*`, `04-comparison.json` |
| Companions | four named laws executed and passed, shared receipt reused | | Kiln receipt |
| Teardown and cost | host stopped; live model usage/cost recorded | | run receipt |

Judge whether the queued receipt, host notice and restored conversation remain
clear to a user in either tab. Fill later gates NOT RUN after any Abort.
