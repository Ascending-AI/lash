# Child-turn and driver stack growth have canonical seams

## Context

Polling nested turns or heavy effects recursively can grow the driver's poll
stack even when individual futures are boxed. Task boundaries are the runtime's
explicit way to bound that growth.

## Decision

A managed child turn runs on its own runtime task. Owned heavy local effects
also select the runtime task boundary; cheap effects stay inline. The runner
declares that choice rather than callers adding boxes where a stack budget
happens to fail.

The child runtime mutex stays held for the complete turn. Post-turn state is
published while it remains guarded. This is the single-writer boundary, not a
recursion guard. Child activity uses the ordinary event channel.

Spawned child and effect tasks have abort-on-drop guards. Normal completion
disarms them. A task panic crosses the runtime's panic-containment boundary:
production maps it to a typed failure, while configured loudness can resume
the panic. It is neither ignored nor a blanket promise to unwind the parent.

Heavy turn effects own their driver state and return the changes needed by
the outer driver. The effect task commits nothing: phase commits stay with the
session actor's owner under its epoch fence
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3), so a
task boundary never moves commit authority.

## Consequences

Recursion grows at named task boundaries. Dropping the owning future aborts
its spawned local work. Shortening the child mutex would change publication
ordering; committing from an effect task would split commit authority from
the actor's owner. Opportunistic future boxing is rejected because it does not
establish a consistent recursion boundary.

## Implementation

- [Managed child turn and abort guard](../../crates/lash-core/src/runtime/session_manager/session_init.rs).
- [Heavy turn effect selection](../../crates/lash-core/src/runtime/turn_driver/local_effects.rs) and [local task executor](../../crates/lash-core-execution/src/runtime/effect/executor.rs).
