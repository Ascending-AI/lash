# Child-turn and driver stack growth have canonical seams

## Context

Polling nested turns or heavy effects recursively can grow the driver's poll
stack even when individual futures are boxed. Task boundaries are the runtime's
explicit way to bound that growth.

## Decision

A managed child turn with an owned, shareable effect controller runs on its own
runtime task. A handler-scoped controller stays on its invocation task.
Owned heavy local effects also select the runtime task boundary; cheap effects
stay inline. The runner declares that choice rather than callers adding boxes
where a stack budget happens to fail.

The child runtime mutex stays held for the complete turn. Post-turn state is
published while it remains guarded. This is the single-writer boundary, not a
recursion guard. Child activity uses the ordinary event channel.

Spawned child and effect tasks have abort-on-drop guards. Normal completion
disarms them. A task panic crosses the runtime's panic-containment boundary:
production maps it to a typed failure, while configured loudness can resume
the panic. It is neither ignored nor a blanket promise to unwind the parent.

Heavy turn effects own their driver state and return the changes needed by
the outer driver. Controller proxies send owned requests back to the
handler-scoped controller while local executors remain on the effect task.
The controller task keeps heap-owned in-flight requests and polls each at most
once per task poll. It polls all pending requests so a suspended request
cannot bury the request that releases a lock it needs. The handler's journal
controller remains authoritative, and replay may skip local execution.

## Consequences

Recursion grows at named task boundaries. Dropping the owning future aborts
its spawned local work. Shortening the child mutex would change publication
ordering; replacing a handler-scoped controller with an out-of-band one would
change journal authority. Opportunistic future boxing is rejected because it
does not establish a consistent recursion boundary.

## Implementation

- [Managed child turn and abort guard](../../crates/lash-core/src/runtime/session_manager/session_init.rs).
- [Heavy turn effect selection](../../crates/lash-core/src/runtime/turn_driver/local_effects.rs) and [local task executor](../../crates/lash-core-execution/src/runtime/effect/executor.rs).
- [Handler controller proxy and driver](../../crates/lash-core-execution/src/runtime/effect/executor/control/task.rs).
