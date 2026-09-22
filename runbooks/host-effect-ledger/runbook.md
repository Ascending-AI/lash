# Host effect ledger

> **Read [../RULES.md](../RULES.md) first.** This runbook documents a
> deterministic, operator-only rehearsal — not a judged browser journey. Its
> executable half is the `host_effect_ledger` witness module in
> `crates/lash-core-execution::tool_dispatch::tests`, which drives the real
> dispatch path against in-memory providers and makes no model or network
> call.

**Purpose.** A host that owns an external system — a ticket tracker, a
payments API, anything whose writes the runtime cannot journal — can build a
durable undo ledger for tool effects with no new core primitive. This
runbook demonstrates the pattern and pins its limits.

## The pattern

Two seams, one identity:

- **The write-ahead seam is inside the attempt.** The tool body claims a
  ledger row keyed by `AttemptContext::tool_call_id` *before* touching the
  world, records the effect it intends, applies the write, and marks the
  row applied. The row is the difference between "died before the write"
  and "died after it" — the crash window is exactly the `pending` stage.
- **The correlator is the after-tool hook.** `ToolResultHookContext::call_id`
  carries the same durable call id the executed-call record carries, so the
  hook can note the call's outcome on the row. The hook *observes*; it never
  decides whether the effect landed.
- **Reconciliation and compensation are host passes**, not runtime
  machinery: on restart, pending rows whose calls are over are reconciled
  against the world — the recorded effect either landed or it did not — and
  reverse compensation walks applied rows at or after a retained history
  point, newest first, each exactly once.

## Deterministic companion

Choose a warm fork and an evidence directory owned by this run.

```bash
set -o pipefail
: "${LASH_EFFECT_LEDGER_FORK:?set this to the caller-owned warm fork name}"
: "${LASH_EFFECT_LEDGER_EVIDENCE_DIR:?set this to a fresh evidence directory}"
mkdir -p "$LASH_EFFECT_LEDGER_EVIDENCE_DIR"
```

`LASH_EFFECT_LEDGER_EVIDENCE_DIR` is a path on the **caller's** side of `kiln
gate`: the `tee` runs outside the gate body, so set it to an absolute path the
caller owns.

```bash
kiln gate lash "$LASH_EFFECT_LEDGER_FORK" -- \
  kiln test //crates/lash-core-execution:lash-core-execution__unit_test --test_output=all \
  --test_arg=host_effect_ledger \
  | tee "$LASH_EFFECT_LEDGER_EVIDENCE_DIR/host-effect-ledger.log"
```

Expect exactly seven tests and `7 passed; 0 failed`. What each proves:

- `host_effect_ledger_hook_call_id_matches_the_executed_call_record` — the
  hook context's `call_id`, the executed-call record's `call_id`, and the id
  the attempt body saw through `AttemptContext::tool_call_id` are one value.
  This is the ticket's contract: the correlator and the durable record join.
- `host_effect_ledger_deduplicates_retried_attempts_on_the_call_id` — a
  retried attempt whose predecessor applied the effect before reporting a
  lost acknowledgement claims the same row and touches the world once. The
  hook observed both attempts under the one call id.
- `host_effect_ledger_replay_reexecutes_neither_effect_nor_hook` — a journaled
  redrive returns the recorded attempt outcome: the tool body does not
  re-run and the hook is not re-invoked, so the ledger sees exactly one
  observation per call.
- `host_effect_ledger_reconciles_a_pending_row_against_the_world` — a call
  that dies inside the crash window leaves a `pending` row the failed
  outcome cannot settle; the host's restart pass proves the effect landed
  and marks it `applied`.
- `host_effect_ledger_compensates_only_what_reconciliation_proves` — a call
  that failed before the write reconciles to `no_effect`; the compensation
  pass undoes only the row the world confirms.
- `host_effect_ledger_reverse_compensation_resumes_at_a_retained_point` —
  undo walks applied rows at or after the retained point, newest first; a
  pass killed partway resumes skipping already-compensated rows, and a
  further pass is a no-op.
- `host_effect_ledger_observes_a_settled_pending_call_under_its_call_id` —
  a deferred call reports nothing while parked and is observed once, under
  its durable call id, when its completion settles.

Score by the passed count, not the exit code: a filter that matches nothing
exits `0` having proved nothing, so require `7 passed` in the log.

## Abort and score

Abort on a test count other than `7 passed; 0 failed`, a provider or
network call in the log, or a test that passes while the ledger rows it
prints contradict the claim its name makes. This rehearsal drives in-memory
providers only; a run that needed a real token is a harness defect, not a
funding decision.

## Scorecard

| Item | Objective gate | Evidence |
|---|---|---|
| Correlator joins the record | hook `call_id` == `ToolCallRecord::call_id` == `AttemptContext::tool_call_id` | `host_effect_ledger_hook_call_id_matches_the_executed_call_record` |
| Retry deduplication | one world-write across retried attempts of one call | `host_effect_ledger_deduplicates_retried_attempts_on_the_call_id` |
| Replay deduplication | journaled redrive runs neither the effect nor the hook | `host_effect_ledger_replay_reexecutes_neither_effect_nor_hook` |
| Crash reconciliation | a `pending` row settles from the world, not the hook | `host_effect_ledger_reconciles_a_pending_row_against_the_world` |
| Partial failure | compensation touches only what reconciliation proved | `host_effect_ledger_compensates_only_what_reconciliation_proves` |
| Restartable undo | reverse compensation resumes at a retained point, then is a no-op | `host_effect_ledger_reverse_compensation_resumes_at_a_retained_point` |
| Deferred settlement | a parked call is observed once, under its call id, at settle | `host_effect_ledger_observes_a_settled_pending_call_under_its_call_id` |

**Aggregate:** could a host engineer following only this runbook build a
durable compensation ledger that survives retry, replay, crash, and partial
failure — and correctly refuse to automate the tools it cannot instrument?

## Limits — read before adopting

- **The hook is a correlator, not a receipt.** Retry and hook-result
  reinspection invoke it more than once for one call, so observations
  deduplicate on `call_id`; an append-only log keyed by anything else will
  double-count.
- **Replay skips the hook.** A journaled redrive serves the recorded outcome
  without re-running the attempt or the hook. A ledger whose only write seam
  is the hook cannot tell replay from silence — the claim must live inside
  the attempt, where `AttemptContext::tool_call_id` supplies the same key.
- **A parked call observes at settlement, not at park.** Nothing reaches the
  hook between the park and the resolution, so a row claimed by a call that
  then defers stays `pending` until the completion settles — that is correct
  bookkeeping, not a stuck row.
- **There is no atomicity between the ledger write and the world write.** The
  row claim and the external effect are two writes to two systems; the
  `pending` stage plus reconciliation is the whole answer to a crash between
  them. Hosts needing stronger must put both writes under one external
  transaction where the system allows it.
- **Tools the host cannot instrument claim no row.** A tool whose body the
  host does not own — a third-party provider, a sealed runtime tool — never
  executes the claim seam, so the ledger can hold only the hook's
  observation: that a call ran and what it reported. The observation cannot
  prove an external effect landed, and compensation for such a tool is a
  host policy decision the ledger cannot automate.
