# E2E Scenario: Session Isolation with Shared Mail-Source Fan-Out

> Read [../RULES.md](../RULES.md) and the
> [host-trigger evidence procedure](../host-trigger-evidence.md) first.

**Purpose.** Two sessions in one workbench keep their conversations and
registrations separate while a shared mail occurrence deliberately fans out
to both. Judge routing by subscription owner and process identity. A shared
source event is expected in both subscriptions; foreign composer text or a
foreign process-end notice is a leak.

**Tier.** Economy, `deepseek/deepseek-v4-pro`, TypeScript, SQLite. One judged
run, two independent contexts and one host.

## Golden rules

1. Create A and B through **new chat** (`#newChat`) and pin each explicit
   `session_id`. URL, `#sessionId`, and scoped state must agree; A differs from B.
2. Both registrations use the same name **shared-mail-watch**, source
   `{"kind":"mail"}` and event argument `event`. Their `id` values differ and
   each `owner_session` equals its registering session. Source equality is
   deliberate; a name cannot prove ownership.
3. Every mail occurrence selects both pinned subscriptions atomically. Expect
   two deliveries and two distinct process ids, including when the injection
   request is scoped to only A. A caller's query is not a recipient filter.
4. Each process-end notice reaches only its subscription owner. A's notice
   input names A's bound process id; B's names B's. Each commits once and
   receives one sealed answer. Shared event content is not a transcript leak.
5. Composer markers must occur only in their own transcript, with matching
   rendered/API canonical rows. Registration ids must never cross scoped lists.
6. Host-delivery bindings establish process ownership. Session-scoped work
   reads and drawers are observer views, not a guaranteed listing of host
   starts. Do not substitute the runtime-wide list as an isolation oracle.
7. Any foreign composer marker, foreign subscription or notice process id,
   UI/API disagreement, or collapsed session identity invokes Abort immediately.

## Deterministic companions

```sh
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --test_arg=tests::session_isolation_tests::concurrent_sessions_isolate_transcripts \
  --test_arg=tests::host_primitives_tests::an_occurrence_recorded_while_a_subscription_is_removed_delivers_to_the_others \
  --test_arg=tests::host_primitives_tests::a_process_end_notice_retried_after_a_lost_acknowledgement_reaches_the_session_once
```

Expect **3 executed, 3 passed**. The transcript law covers simultaneous
composer turns only. The recipient-selection law covers a shared occurrence
with one deleting and one pinned subscription. The notice law covers keyed
retry identity. Cross-session fan-out and owner routing are additionally
judged below, not claimed as properties of the transcript-only companion.
Cite any law already run for another row by its shared receipt.

## Phase 0 — Boot and pin A/B

Use the shared setup and create Work. Create two chats through the sidebar,
then open independent contexts at their explicit URLs. Require different
session ids on one origin, empty timelines, and empty scoped trigger lists.
Save `00-startup.json`, `00-sessions.json`, `00-tab-a.png`, and `00-tab-b.png`.

## Phase 1 — Register independently

Ask each agent to register **shared-mail-watch** on the mail source, returning
the event title from a short process without mail-sending effects. Ask it to
acknowledge its process-end notices briefly through `chat.reply`. Settle each
session independently. Require exactly one owned registration in each list,
same name/source/event argument, and distinct ids. On each Triggers page the
rendered `data-subscription-id` must equal its own id and exclude the other's.
Save `01-triggers-a.json`, `01-triggers-b.json`, and `01-trigger-a.png` /
`01-trigger-b.png`. Do not pin generated source or definition identity as a
condition on nondeterministic model authorship.

## Phase 2 — Concurrent composer turns

Submit from both browsers concurrently: request several Parallel search
lookups and repeat `ISOLATION-A-<run-id>` or `ISOLATION-B-<run-id>` respectively.
Poll both states until each simultaneously has one distinct active address;
save `02-active-a.json` / `02-active-b.json`. Settle independently and require
the sealed assistant marker locally, never remotely, with ordered canonical
DOM/API parity. The two registrations remain unchanged. Save
`02-state-a.json`, `02-state-b.json`, `02-dom-a.json`, `02-dom-b.json`, and
`02-transcript-a.png` / `02-transcript-b.png`.

## Phase 3 — One occurrence, two owners

Deliver `ISOLATION-MAIL-<run-id>` once into Work from A. Confirm the inbox
marker, then read the host tables. Require one occurrence with exactly two
bindings: registration A to process A, registration B to process B, with the
correct owners and distinct process ids. Await both ids to successful title
outputs. Save `03-fanout.json`, `03-await-a.json`, `03-await-b.json`, and
`03-mail.png`.

Settle each session's notice and answer. Require exactly one committed
`process-end:<process A>` input in A and none in B; the inverse for process B.
Each owner's sealed reply appears once in that owner's page/API. Save
`03-state-a.json`, `03-state-b.json`, both replay/trace extracts and
`03-notice-a.png` / `03-notice-b.png`.

## Phase 4 — Remove one recipient

Cancel A's registration. Require A's scoped list empty while B's original id
is unchanged. Deliver `ISOLATION-AFTER-REMOVE-<run-id>` once from B and complete
a composer fence in A. Require the new occurrence to bind only B's
subscription, one new B process, and one B notice; no A delivery or notice.
Save `04-triggers-a.json`, `04-triggers-b.json`, `04-fanout.json`,
`04-state-a.json`, `04-state-b.json`, and `04-final-a.png` / `04-final-b.png`.
Do not infer absence from the age-filtered unscoped work list.

Complete the bidirectional matrix, then use the shared teardown:

| Surface | A contains | A excludes | B contains | B excludes |
| --- | --- | --- | --- | --- |
| Composer transcript | marker A | marker B | marker B | marker A |
| Registration before removal | id A | id B | id B | id A |
| Notice before removal | process A | process B | process B | process A |
| Notice after removal | no new notice | new B process | new B process | new A process |

## Scorecard

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| Two sessions/one host | distinct explicit ids, empty initial scoped surfaces | | `00-*` |
| Registration ownership | same-name mail subscriptions, distinct ids, no cross-listing | | `01-*` |
| Concurrent composer isolation | overlapping active addresses; local sealed marker only | | `02-*` |
| Shared source fan-out | one occurrence binds two distinct owned processes | | `03-fanout.json`, await results |
| Notice owner routing | one notice per process only in its owner's API and DOM | | `03-state-*`, `03-notice-*`, replay/trace |
| Recipient removal | only B receives the next occurrence; A stays silent | | `04-*` |
| Final non-membership | every matrix row passes in both directions | | matrix extracts |
| Companions | three named laws executed and passed, shared receipts reused | | Kiln receipt |
| Teardown and cost | host stopped; model usage/cost recorded | | run receipt |

Judge whether shared-source delivery remains intelligible without exposing a
foreign conversation. Preserve any leak finding and mark later gates NOT RUN.
