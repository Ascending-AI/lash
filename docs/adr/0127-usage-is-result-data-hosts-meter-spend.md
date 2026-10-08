# 0127: Usage is result data; hosts meter spend

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
from successes and failed partial responses. It reads turn `TokenUsage`
summaries and does not consume the ADR 0125 ledger. That ledger adds a SQL
admission and a recorded delivery per model call, duplicates host metering,
and would freeze unnecessary storage and host APIs at 1.0.

## Decision

Usage is provider-reported data. Each model call's committed result carries
its response and `LlmCallRecord`, including the observed usage and failure of
each attempt
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §4).
Resume loads that result. Absence stays absent and
explicit zero stays zero, as ADRs 0031 and 0032 require. Lash uses this data
for context-window and compaction decisions, configured token budgets,
turn-result summaries (`TokenUsage`) and trace attributes.

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
complete(request):
    reservation = spend_gate.reserve(operation_class)
    attempt = next_ordinal(request.request_id)
    result = inner.complete(request)
    receipt = extract_allowlisted_receipt(result)
    ledger.upsert(request.request_id, attempt, receipt, result.failure)
    reservation.settle()
    return result
```

Reservation precedes `complete()`. Settlement runs after every attempt,
including failure and partial response, before returning its result. The
host makes settlement idempotent on the deterministic request id and attempt
ordinal. Provider request/response ids can strengthen its receipt identity.
The ordinal distinguishes retries of one logical call; repeating delivery of
one receipt must reuse that attempt's identity.

Figments' extractors read OpenRouter `/usage/cost` and Opper `/cost`,
`/usage/opper/cost/total` and `x-opper-cost`, with declared precedence and
validated decimal amounts. Missing or malformed receipts remain unpriced,
never inferred zero. Its provider template clones carry the attempt sequence;
the template must remain unused so a fresh call starts its own sequence.
Figments ADRs 0048 and 0050 own receipt settlement and operation-class
in-flight liability. Those policies are examples, not Lash policy.

### What Lash provides

Lash provides no usage ledger, usage-fact store, accounting delivery,
reconciliation, unknown-liability tracking or deletion drain. SQL stores
retain runtime state and execution evidence for their existing purposes.
Session deletion and process pruning have no accounting dependency.

The pre-1.0 cutover removes accounting shapes in place, without a version
bump, migration or compatibility reader. ADR 0106 owns the version freeze.

## Consequences

A model call needs only its ordinary recorded result. Billing cannot delay
Lash deletion or impose accounting transactions on runtime execution. Hosts
choose how to retain and meter their provider receipts, and can enforce caps
before dispatch. Usage summaries and traces describe reported tokens; a host
requiring billing evidence collects receipts at the provider seam.
