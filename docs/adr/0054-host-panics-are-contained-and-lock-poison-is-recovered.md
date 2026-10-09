# Host panics are contained and standard-lock poison is recovered

## Status

Accepted.

## Context

Providers and tools are host-supplied code. A panic at their attempt boundary
must become a typed production failure, while a harness needs the same defect
to remain loud. Lock poisoning indicates an unwind while holding a guard; it
does not establish that the protected value is unusable.

## Decision

`Provider::send` and `ToolProvider::execute` contain attempt panics as
non-retryable typed host failures. A provider panic records
`lash:provider_panicked` and stops the turn with `TurnStop::ProviderError`.
A tool panic records `ToolFailureCause::Panicked`, naming the tool, call id
and panic text, and stops the turn with `TurnStop::ToolPanicked`. Outside
work may already have happened; the outcome does not assert it was undone.
Neither protocol repair nor a caught guest error resumes the panicked turn.
Cell and process tool calls preserve the same typed cause. Auxiliary
provider callback `close` contains panics as
non-retryable `ProviderPanicked` failures too.

Child and effect task joins distinguish a panic from cancellation, form the
typed panic outcome and then apply loudness. They do not reduce a panic to a
generic task-join failure.

Loudness is a process-scoped runtime flag. Production leaves it disabled;
harnesses, the simulator and confidence binaries enable it at startup. Cargo
features do not choose panic behavior. The failure mapping forms the typed
outcome before loudness can re-raise the panic. For a turn, both the call's
outcome and the terminal turn commit before the post-commit callback raises
it. A redrive reads the terminal and invokes neither the tool nor provider
again.

Standard-library `Mutex` and `RwLock` acquisitions recover poisoned guards with
`PoisonError::into_inner`. `lash_sansio::sync`, also exported through
`lash_core::sync` and `lash::sync`, supplies the shared acquisition traits.
Poison is not a typed error tier. The operation that owns a domain invariant
also owns its repair.

## Why

Selecting containment through features would let feature unification change
production semantics. Converting every poisoned lock into an error would turn
an unwind marker into a claim about domain validity. A process flag and uniform
guard recovery keep those decisions explicit.

## Consequences

- Quiet and loud runs use the same typed failure mapping.
- Containment does not repair the host object's mutable state. Hosts own repair
  or replacement before reusing it.
- Guard recovery is uniform; domain repair remains with the owning operation.

## Code evidence

- [Process flag](../../crates/lash-core-ids/src/panic_containment.rs#L6).
- [Provider attempt and callbacks](../../crates/lash-core-llm/src/provider/handle.rs#L350).
- [Tool attempt containment](../../crates/lash-core-execution/src/tool_dispatch/retry.rs#L85).
- [Child join containment](../../crates/lash-core/src/runtime/session_manager/session_init.rs#L1034).
- [Poison recovery traits](../../crates/lash-sansio/src/sync.rs#L15).
