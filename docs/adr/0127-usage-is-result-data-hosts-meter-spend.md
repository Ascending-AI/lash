# 0127: Usage is result data; hosts meter spend

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

## Status

Accepted (FIG-4852). This decision replaces
[ADR 0125](0125-model-usage-is-engine-owned-accounting-delivered-per-call.md).

## Context

Lash reports model usage and needs token counters for context windows,
compaction and configured token budgets. A token counter does not establish
what a provider charged. Routing, cache pricing, reasoning and gateway fees
can make catalogue estimates differ from the account debit.

Figments, Lash's production host, already meters at the `Provider` boundary.
Its `CostTrackingProvider` reserves spend before dispatch and settles receipts
from successes and failed partial responses. It reads turn `LlmUsage`
summaries and does not consume the ADR 0125 ledger. That ledger adds a SQL
admission and a recorded delivery per model call, duplicates host metering,
and would freeze unnecessary storage and host APIs at 1.0.

## Decision

Usage is provider-reported data. Each model call's existing settling transaction retains a
`model_call_records` row naming its `ModelCallFact` and sealed `LlmCallRecord`,
including observed usage and failures of every attempt. An answer receipt
names the logical call id and may be consumed without discarding that record
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §4).
Resume loads that result. Absence stays absent and
explicit zero stays zero, as ADRs 0031 and 0032 require. Lash uses this data
for context-window and compaction decisions, configured token budgets,
turn-result summaries (`LlmUsage`) and trace attributes.

Spend metering, caps, pricing and billing belong to the host at the `Provider`
seam. Lash owns no authorization policy, rate limiting or cost accounting.
A host can refuse a call before it spends, and can settle the actual receipt
rather than deriving a debit from token counts and a mutable price catalogue.

Lash guarantees hosts:

- Deterministic request ids remain stable across retries, re-sends and resume.
- Resume or redrive of a model call whose result committed makes no provider
  call.
- Allowlisted response headers and JSON pointers are captured on buffered,
  streaming and failed partial responses, so receipts reach the decorator.
- Provider failures remain typed. A host spend-cap refusal can reach the
  caller as a quota failure rather than an unclassified string.

These guarantees do not make an external attempt exactly once: a model call is
`Repeatable`, so a crash before its result commits re-sends the pinned request
while its `model_total` deadline allows.
The engine can retry after dispatch but before recording the result. The host
owns receipt retention and idempotent settlement for that window.

### A host decorator

Figments installs `CostTrackingProvider` around its resolved provider through
`install_cost_tracking` in `apps/lash-runtime/src/costs.rs`. A short rendering
of that pattern is:

```text
send(body, context):
    key = (context.scope.request_id, context.scope.attempt)
    spend_gate.reserve(key, operation_class)
    result = inner.send(body, context)
    ledger.upsert(key, extract_allowlisted_receipt(result), result.failure)
    spend_gate.settle(key)
    return result

attempt_ended(attempt, terminal):
    key = (attempt.call_id, attempt.ordinal)
    ledger.upsert(key, extract_allowlisted_receipt(terminal.partial_response), terminal.reason)
    spend_gate.settle(key)
    await inner.attempt_ended(attempt, terminal)
```

Reservation precedes `send()`, is keyed by the attempt, and stays available
if the send future ends without returning. Settlement runs from either the
returned result or `attempt_ended`, including failure and partial response.
The host makes settlement idempotent on the deterministic call id and
Lash-assigned attempt ordinal. Provider request/response ids can strengthen
its receipt identity. The ordinal distinguishes retries of one logical call;
repeating delivery of one receipt must reuse that attempt's identity.

Figments' extractors read OpenRouter `/usage/cost` and Opper `/cost`,
`/usage/opper/cost/total` and `x-opper-cost`, with declared precedence and
validated decimal amounts. Missing or malformed receipts remain unpriced,
never inferred zero. Lash assigns each send its attempt ordinal; decorators do not count retries.
Figments ADRs 0048 and 0050 own receipt settlement and operation-class
in-flight liability. Those policies are examples, not Lash policy.

### The settle point

The provider seam is the one settle point for each dispatched attempt:
`Provider::send` returns `Ok(LlmResponse)` or `Err(LlmTransportError)`, or Lash
invokes `Provider::attempt_ended` with `AttemptTerminal::Abandoned` after the
send ended without returning. Returned attempts never receive that callback.
Decorators forward `attempt_ended` to their inner provider and settle from
these terminals, including every failed attempt before a retry.

Lash signals `ResponseContext::attempt_control` at a model deadline,
immediate cancel, tail-drain drop or superseded retry. A contained provider
panic delivers Abandoned with ProviderPanicked; a stream consumer or
assistant-stream hook failure uses StreamConsumerFailed.

Hosts configure `ProviderComponents::with_terminal_policy` with a typed
`ProviderTerminalPolicy`. Its `drain_timeout` and `delivery_timeout` each
default to 250 ms. Lash clips each to half the execution budget's stop grace,
so together they cannot extend that grace. Lash continues polling a cancelled
send for the drain interval. A cooperative provider can return its partial
response then. Otherwise Lash drops the send and invokes `attempt_ended` on
the same provider instance. The terminal carries the call id, Lash-assigned
ordinal, request scope with the call span, and immutable `drain_timed_out`
evidence. Its partial response retains allowlisted metadata, served model,
provider response id and usage received before abandonment. Missing usage
stays unknown, including when headers arrived without usage.

Lash awaits the callback for the delivery interval, then proceeds even if a
host ledger write has not completed. A bounded invocation is not a promise
that the host's settlement completed. Delivery timeout is Lash's observation
of the callback: the call and attempt spans carry
`lash.provider.terminal_delivery_timed_out`, and the runtime increments
`lash.provider.terminal_delivery.timeouts`. It is separate from the immutable
attempt terminal. These are process-local observations, not durable delivery:
a process crash delivers nothing and the host reconciles that window from
its own receipt store. Dropping the turn's owner signals its independently
owned provider task, which drains and delivers the terminal under these bounds.

Every Lash-driven attempt gets exactly one terminal. A host that itself drives
and drops a borrowed `ProviderHandle::complete`, `Provider::complete` or
`Provider::send` future owns that call's cancellation and terminal itself.

`TurnEvent::ModelCallRecorded` and `TurnFinalized` are observation only.
Neither is a billing delivery channel. Committed call records retain execution facts without host receipts.
Followers read them by turn and call id on any node. A turn's usage is
computed from its rows; the head's usage remains the lifetime projection.

### What Lash provides

Lash retains one committed record per logical model call, written beside its
answer or in its existing cancellation, deadline or tail-settling commit.
Pending dispatch identities survive early receipt consumption. On recovery,
lost observations become `Interrupted`/`Unobserved` attempts, never zero usage.
`ModelCallRecorded` carries only `call_id`; hosts read rows or the head
instead of `TurnEvent::Usage` or `SessionStreamEvent::LlmUsage`.
This is no accounting ledger: it has no admission policy, delivery,
reconciliation, money, host receipts or deletion drain.
Session deletion and process pruning have no accounting dependency.

The pre-1.0 cutover removes accounting shapes in place, without a version
bump, migration or compatibility reader. ADR 0106 owns the version freeze.

## Consequences

A model call's sealed record outlives its consumed answer receipt. Its row
and graph node share the session's ledger lifetime, with no separate host setting. Billing cannot delay
Lash deletion or impose accounting transactions on runtime execution. Hosts
choose how to retain and meter their provider receipts, and can enforce caps
before dispatch. Usage summaries and traces describe reported tokens; a host
requiring billing evidence collects receipts at the provider seam.
