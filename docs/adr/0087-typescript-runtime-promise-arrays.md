# 0087: TypeScript aggregates evaluate runtime arrays

## Context

Promise aggregate arguments can be arrays stored in bindings, synchronous map
results, or mixtures of pending operations and plain values. Their meaning must
follow the evaluated array rather than a restricted literal syntax.

## Decision

Evaluating tool calls in aggregate operand position creates pending-operation
handles with captured receivers, arguments, and source sites. The lowerer keeps
aggregate operands at the applicable await depth, including when an aggregate
appears inside another call's argument. The VM evaluates the resulting array
once in source order and builds its host batch from live pending handles.

Array settlement is shallow. Plain elements retain their values for
`Promise.all`; `Promise.allSettled` produces successful result envelopes for
them. An object containing a handle is a plain element rather than a request
to walk and settle its fields. A repeated pending handle executes once while
all of its array positions retain the outcome.

### One batch and one recorded order

Tools, timers, and explicit `processes.await(handle)` calls join one operation
batch. The host's durable settlement order determines which rejection
`Promise.all` reports. `Promise.allSettled` reports outcomes in array order.
A durable wait enters settlement order when its completion arrives, rather than
when it is launched. Process waiting is an explicit tool under ADR 0095.
Race and any use the same evaluated-array and recorded-order machinery under
ADR 0099.

A raw process handle at an aggregate element position is refused with a repair
naming `processes.await(handle)`. It does not create another settlement phase.

### Pending identity and durable execution

A handle uses `{__handle__: "lash", id}`. The pending id includes the execution
identity; only a live request in that execution can be settled. Unknown,
expired, or consumed requests receive typed repairs. Pending handles in tool
arguments are refused before dispatch. A tool call discarded as an expression
statement, including `void tools.x(...)`, and a `const` or `let` tool handle
whose binding is never read in the cell are refused at lowering with the
spanned `TS_UNAWAITED_TOOL` diagnostic. Its repair names direct `await` and
collection into an awaited `Promise.all` or `Promise.allSettled`. Reads resolve
to binding identities, including captures and session-global property reads.
Lowering remains conservative about possible consumers: later awaits, literal
aggregate arrays and handles pushed into arrays remain legal.

Abandoned handles that cannot be proven unused at lowering produce a typed
`PendingTool` error at execution completion. Each pending entry retains its
call path and source span from the recorded instruction position. Source-aware
feedback renders each call's line as well as the count. This inspects only VM
state and changes neither dispatch nor replay re-execution.

Pending requests and their execution identity are continuation state. Suspension
preserves them under the declared continuation and compiled-program contract.
Bindings containing pending handles stay out of detached host globals under
ADR 0076. Current format identities live in `lash::formats` and the format
registry, rather than in this decision's prose.

## Alternatives considered

Restricting aggregate operands to literal arrays rejects ordinary evaluated
arrays. Separate tool and process settlement phases make rejection priority a
property of leaf kind rather than recorded completion. Recursively walking
plain values changes JavaScript array-element semantics. A single shallow batch
keeps values, positions, and settlement order explicit.

## Consequences

Computed arrays and mixed operation leaves share one durable aggregate contract.
Plain values remain plain, duplicate positions survive deduplication, and
rejection selection follows recorded order. Host waits do not acquire a separate
phase or cancellation grace from being used inside an aggregate.

## Code references

- `crates/lash-typescript/src/lower/constructs.rs:164` defines aggregate operand await depth.
- `crates/lashlang/src/runtime/vm/pending_tools.rs:17-40,168-260` classifies values and builds one deduplicated batch.
- `crates/lash-typescript/tests/runtime_promises.rs` covers runtime arrays and invalid handle repairs.
- `crates/lash-typescript/tests/agent_surface.rs` and `crates/lash-core-execution/tests/store_backed/kernel/session/settlement_latency_tests.rs` cover mixed settlement and durable waits.
