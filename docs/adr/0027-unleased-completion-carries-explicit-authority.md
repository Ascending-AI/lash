# 0027: Process completion carries explicit authority

## Status

Accepted.

Amended 2026-09-27 (FIG-3863): D21 removes process leases and the separate
lease-fenced completion path. Every process execution write is invocation-bound;
terminal writes continue to carry an explicit completion authority. Process
observations no longer expose a lease holder or expiry. The execution attempt
remains part of invocation authority so a stale engine invocation cannot write
after its successor.

[ADR 0110](0110-the-engine-owns-process-recovery.md) defines the current
recovery contract. Lash never re-runs started work outside its engine journal.

## Decision

`ProcessRegistry::complete_process` requires a `ProcessCompletionAuthority`.
The authority names the single-writer discipline under which a caller may write
a terminal event, and each backend validates it against the process input class
inside the terminal transaction. The event records the validated authority as
audit evidence.

There are three authorities:

- **`ExternalOwner`** closes an externally-owned (`ProcessInput::External`)
  process. The session manager verifies the caller's observer edge before the
  authority reaches the registry.
- **`WorkflowKey`** closes a process the workflow substrate executed. The key
  records the engine's per-process serialization discipline.
- **`WorkflowKeyRecovery`** closes an executed process whose current segment's
  journal was lost. Its segment ordinal is checked in the same transaction as
  the terminal append; a later handover returns `ProcessHandedOver` instead.

`ExternalOwner` is rejected for engine-executed processes. Both workflow
variants are rejected for externally-owned processes. There is no default
authority and no process-lease completion path.

Process execution writes use `ProcessExecutionWriteAuthority`, reconstructed
from the engine invocation identity and bound to the admitted attempt. The
registry checks it against the retained started fact before accepting a start,
wait, effect, or terminal write. A superseded invocation is rejected with
`ProcessExecutionSuperseded`.

## Why

A bare completion method left authority in caller convention. The required
argument makes the writer's discipline explicit, and validation inside each
backend gives the rule one enforcement point. In-process Rust cannot make the
token unforgeable; explicitness, uniform validation, and durable evidence are
the contract.

The former process lease protocol duplicated the engine's execution authority
and created a second takeover mechanism. D21 removed that machinery. Invocation
identity and the engine journal now own execution and replay, while completion
authority distinguishes workflow writes from external-owner writes.

## Consequences

- A caller cannot compile a completion without naming its authority.
- SQLite, PostgreSQL, in-memory stores, decorators, and test doubles enforce
  the same input-class rule within their terminal write.
- Terminal event type, replay key, and payload construction share the
  `terminal_append_request` helpers. The completion-authority evidence field
  remains part of the event payload.
- The process registry has no lease claim, renewal, takeover, or lease-fenced
  completion API. The `ProcessLease` model and its serde tests are deleted under
  D21.
- This cutover does not alter the completion-authority payload or its versioned
  process-event identity.
