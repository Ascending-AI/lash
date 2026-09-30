# 0027: Process completion carries explicit authority

## Status

Accepted.

## Decision

`ProcessRegistry::complete_process` requires `ProcessCompletionAuthority`. The backend validates it against the process input class inside the terminal transaction and records the accepted authority in event evidence.

- `ExternalOwner` completes `ProcessInput::External`. Public session-scoped completion checks the caller's observer relationship before reaching the registry.
- `WorkflowKey` completes engine-executed work and records its serialized workflow key.
- `WorkflowKeyRecovery` ends engine work whose segment cannot resume. It includes a segment ordinal, checked transactionally against the retained carrier; a later handover returns `ProcessHandedOver`.

External authority is refused for engine work and both workflow authorities are refused for external work. There is no default authority. Authority validation precedes terminal replay. Valid repetition returns the retained outcome and original authority without applying a prelude, adding events or rearming publication. An equal proposal is `AlreadyApplied`; a different proposal reports that the retained terminal already owns the result. A different valid workflow key does not replace terminal evidence.

Process execution writes also carry `ProcessExecutionWriteAuthority`, bound to invocation identity and the admitted attempt. A stale invocation is refused with `ProcessExecutionSuperseded`. The engine journal owns replay under ADR 0110; the registry has no execution lease, renewal or takeover API.

## Why and consequences

Caller convention alone cannot establish who may write a terminal. The required argument, uniform transactional validation and retained evidence make that discipline inspectable. In-process tokens are not an auth boundary; the embedding host owns security policy. SQLite file, SQLite memory and PostgreSQL enforce the same rule.

Terminal construction shares `terminal_append_request` helpers. [Completion authority](../../crates/lash-core-execution/src/runtime/process/events.rs), [invocation authority](../../crates/lash-core-execution/src/runtime/process/model/execution.rs) and [registry transitions](../../crates/lash-core-execution/src/runtime/process/registry_transitions.rs) define the contract.
