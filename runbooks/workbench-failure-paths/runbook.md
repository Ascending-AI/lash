# E2E Scenario: Workbench Failure Paths

> **Read [../RULES.md](../RULES.md) first.** Its browser, polling, screenshot,
> Abort/RCA, and teardown rules apply. This runbook names a deliberate exception to the
> real-token rule: the Workbench's opt-in development failure provider.

**Purpose.** Judge what a user sees when provider authentication fails mid-turn, a
pre-output rate limit causes an attempt reset, paid partial output makes regeneration unsafe,
and a durable Runtime Process fails. The rate-limit phase also judges the persisted execution
scorecard: an evidence-less failed attempt must remain visible before its successful retry,
including after cursor replay and snapshot hydration. Each fault is deterministic so the
visible wording and final state are objective gates.

**Deterministic companion.** `just agent-workbench-restate-e2e` asserts the auth terminal,
same-session recovery, retry attempt reset, and single-copy live/replay observations.
`cargo test -p lash-internal-core --lib retryable_mid_stream_failure_preserves_paid_output_without_retry`
asserts the paid-output refusal and single provider call. `cargo test -p agent-workbench
process_work_tests` asserts the failed-process `/api/work` projection and UI error rendering.
The browser run judges the actual transcript and work rail; it does not reproduce those
internal assertions.

## Scenario-specific golden rules

1. Set `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO` to exactly the phase's documented value.
   Require the startup warning and rendered model id `dev/failure-paths`; an OpenRouter
   request or missing warning invalidates the run.
2. Use a fresh data directory and a fresh port-derived stack for each scenario. Never
   carry provider call counts, sessions, or processes between phases.
3. A provider error is a stopped `ProviderError`, never `Cancelled`. The public transcript
   must settle to `turn could not be completed`, while the typed scorecard and trace retain
   the provider classification; the active route clears and the same session remains usable.
4. Retry output is replace-not-append. The final rendered assistant response and
   `/api/state.messages` contain `retry observer single-copy marker` exactly once.
5. **Every provider attempt is a scorecard row.** A pre-response failure has no provider
   response metadata by definition; that absence must leave its provider columns empty, not
   remove the row. Reconcile scorecard rows to the `/api/state` `model_call_recorded`
   product-event records by call id and attempt ordinal, and require the same order and facts
   after replay and reload.
6. Paid partial output is preview evidence only. It must stop with a non-retryable
   `unsafe_retry_after_output_started`, make exactly one provider attempt, and never purchase
   or render the fixture's second-generation sentinel.
7. Process failure remains process failure. The work rail must show `failed` plus the
   durable error; a successful parent turn must not make the process look successful.

## Working material

- For each phase choose `<port>` and `<fresh-data-dir>`, then boot:
  `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=<scenario> AGENT_WORKBENCH_DATA_DIR=<fresh-data-dir> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  No provider key is required. Gate `GET /healthz` → 200 and retain the startup log.
- Browser affordances: model selector, session id, composer, running/idle pill,
  transcript, the **execution evidence** scorecard, and **work** rail.
- Backend truth: `GET /api/state`, `POST /api/turn`, `GET /api/work`, and the observation
  stream used by the page. Disk truth: `<fresh-data-dir>/trace.jsonl` and
  `active-turns.json`.
- End every phase with `just agent-workbench-down <port>` and confirm its managed Restate
  container is gone before reusing the port.

## Phase 0 — Common pre-flight

For the selected scenario, poll `/healthz`, open the page, and require the rendered model
id to be `dev/failure-paths`. Require the startup log to name
`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO` and the exact scenario. Require a fresh transcript,
no active turn, and the idle pill. Screenshot `00-<scenario>-ready.png`.

## Phase 1 — Authentication failure, honest terminal, recovery

Boot with `auth-failure-once`. Submit `trigger deterministic auth failure`. Poll until the
page is idle and Stop is hidden, then gate all of:

- the transcript visibly renders the public event `turn could not be completed`, with no
  assistant success bubble for that turn; the execution scorecard retains `dev_auth_rejected`
  and HTTP 401 without leaking the provider message into the product transcript;
- `/api/state.active_turns` is empty and `active-turns.json` contains no route;
- the trace's completed turn has `outcome` stopped with `provider_error`, contains issue
  code `dev_auth_rejected`, and has no cancellation evidence or cancelled outcome.

Save state and matching trace rows as `01-auth-failed-state.json` and
`01-auth-failed-trace.json`; screenshot `01-auth-failed.png`.

Without resetting or restarting, record the session id, submit `prove recovery`, and poll
for the exact assistant text `session recovered after provider auth failure`. Require the
session id to be unchanged, the page idle, and no active route. Screenshot
`02-auth-session-recovered.png` and save `/api/state` as
`02-auth-session-recovered-state.json`.

Teardown this stack before Phase 2.

## Phase 2 — Retryable rate limit, one-copy convergence

Boot fresh with `rate-limit-once`. Start browser network/event capture before submitting
`trigger deterministic rate limit retry`; this captures transient observations without
using a fixed sleep. Poll until the exact terminal assistant text
`provider retry succeeded` is rendered and the page is idle.

Require the final rendered assistant response and `/api/state.messages` each contain
`retry observer single-copy marker` exactly once. In captured observations or trace
evidence require one retry caused by `dev_rate_limited` and one
`model_attempt_reset`; after applying that reset by its correlation ids, one marker
remains. Require no error terminal, no cancellation evidence, and no active route.

From `/api/state.product_events.events`, select the `model_call_recorded` call whose attempts
carry `dev_rate_limited` and require exactly two attempts in ordinal order. Attempt 1 is
`failed` at `response_observed`,
has error code `dev_rate_limited` and HTTP 429, records a scheduled zero-delay retry, and has
no provider response identity/model evidence. Attempt 2 is a successful protocol-boundary
`interrupted` attempt at `output_started` and comes second. In the rendered
**execution evidence** scorecard require two rows for that same call id in the same ordinal
order. The first row must visibly name attempt 1, `failed`, `response_observed`,
`dev_rate_limited`, HTTP 429, and `retry scheduled`; its empty provider facts must not hide
it. The second must visibly name attempt 2, `interrupted`, and `output_started`.

Force a cursor replay by reconnecting the observation stream with its last acknowledged
cursor and require the scorecard to stay at those same two row identities and facts: no
duplicate row and no dropped failed attempt. Then reload the page so `/api/state` hydrates a
fresh shell, and require the identical ordered two-row scorecard before teardown. Save the
normalized API attempts and the scorecard text before replay, after replay, and after reload.

Screenshot `03-rate-limit-recovered.png`; save state, observations, and trace rows as
`03-rate-limit-state.json`, `03-rate-limit-observations.jsonl`, and
`03-rate-limit-trace.json`. Save the new scorecard evidence as
`03-rate-limit-attempts.json` and `03-rate-limit-scorecard-{live,replay,reload}.txt`, and
capture `03-rate-limit-scorecard-reload.png`. Teardown before Phase 3.

## Phase 3 — Paid partial output refuses regeneration

Boot fresh with `partial-output-failure`. Start browser network/event capture before
submitting `trigger deterministic paid partial failure`. Poll until the page is idle and Stop
is hidden.

Require the rendered turn to show the public event `turn could not be completed`, with no
assistant success bubble. In the captured observations require the safe-regeneration error
text, and in the trace require the typed issue `unsafe_retry_after_output_started`. Also
require in captured observations
`paid partial output marker` as preview activity, but require that marker to be absent from
`/api/state.messages` and the settled transcript. Require `UNSAFE second generation was
purchased` to be absent everywhere.

In the matching trace require exactly one LLM call attempt, `protocol_position` equal to
`output_started`, retry decision `scheduled: false` with reason
`output_started_without_retry_guarantee`, observed output usage equal to 4, and terminal issue
code `unsafe_retry_after_output_started` with `retryable: false`. Require no attempt reset or
retry-status event and no active route.

Screenshot `04-paid-output-refused.png`; save state, observations, and trace rows as
`04-paid-output-state.json`, `04-paid-output-observations.jsonl`, and
`04-paid-output-trace.json`. Teardown before Phase 4.

## Phase 4 — Failed durable process in the work rail

Boot fresh with `failed-process`. Submit `start deterministic failing process`; the parent
turn may finish successfully. Poll `GET /api/work` until the row labelled
`FIG425_deterministic_failure` is terminal with status `failed` and error exactly
`deterministic durable process failure`.

Open the **work** rail and poll until that same row visibly shows both `failed` and
`error: deterministic durable process failure`. The UI and `/api/work` must identify the
same process id. Screenshot `05-failed-process-work-rail.png`; save the API row as
`05-failed-process.json`.

## Phase 5 — Teardown and score

Tear down the final stack and confirm all four managed Restate containers were removed.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Dev-only fixture | exact scenario warning + `dev/failure-paths`; no network provider | | `00-*-ready.png`, startup logs |
| Auth failure | public terminal; typed auth scorecard/ProviderError trace; no Cancelled evidence | | `01-auth-failed.*` |
| Session recovery | same session commits the exact recovery response | | `02-auth-session-recovered.*` |
| Retry convergence | one reset and one surviving marker in UI/API/observations | | `03-rate-limit-*` |
| Retry scorecard | failed evidence-less attempt then successful protocol-boundary retry, exact row identity/order/facts through replay and reload | | `03-rate-limit-attempts.json`, `03-rate-limit-scorecard-*` |
| Paid-output refusal | one output-started attempt; typed non-retryable error; preview not committed | | `04-paid-output-*` |
| Durable failure | work rail and `/api/work` agree on failed + exact error | | `05-failed-process-*` |
| Route hygiene | every settled phase has no active route | | saved state and `active-turns.json` |
| Teardown | each port-derived stack is gone | | command log |

**Aggregate:** did every failure class settle honestly, stay observable at the user
surface, avoid duplicate retry output, and preserve a usable session?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
