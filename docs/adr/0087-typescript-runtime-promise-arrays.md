# 0087: TypeScript aggregates evaluate runtime arrays

Status: Superseded by [ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md) (FIG-2990, 2026-09-13); the replacement landed 2026-09-14 (FIG-2996)

## Context

FIG-2766 requires `Promise.all` and `Promise.allSettled` to accept array-valued expressions, including synchronous maps and arrays saved in bindings. The previous lowering only recognized literal arrays and direct async maps, and rejected otherwise valid mixtures of calls and computed values.

## Decision

TypeScript tool calls in expression position create pending tool handles. Evaluating the argument array captures each call's receiver, arguments, and source site. The aggregate operand is lowered at the top await depth wherever the aggregate itself is written, so a `Promise.all` nested inside another call's argument still produces pending handles that the batch unwraps rather than result envelopes the outer call would forward as values. Aggregate await builds a batch from those handles and passes it through the existing settlement-order validation and rejection selection. Non-handle elements retain their values. Direct async maps retain their existing callback driver.

**Mixed process/tool aggregates settle in two phases.** Phase one settles every tool handle as one resource-operation batch with the host's recorded settlement order. Phase two awaits every process handle in array order through the existing process-await ability, the same durable seam a direct `await handle` uses. The rejection an unwrapping aggregate (`Promise.all`) reports is the first-settled tool rejection by recorded order; only when no tool leaf rejected does the first rejecting process in array order reject the aggregate. `Promise.allSettled` reports every outcome in array order. Lashlang's compiled aggregate (`await [h, module.op(...)]`) follows the same two phases, with its source-order rejection rule for unwrapped module leaves and a result record for each process leaf.

**A tool handle is an execution-scoped identity.** Each handle record carries the minting execution's nonce alongside its request index; `await` reaches a request slot only through a handle stamped with the current execution's nonce. A handle from an earlier execution or a hand-written handle record is refused with the typed pending-tool error, as is a handle awaited twice, a plain value awaited as a handle, and a pending handle passed inside a tool call's arguments (refused before dispatch, so the host never performs the call). Root bindings whose value carries a tool handle are omitted from the exported session globals, as closures already are. The nonce is derived deterministically from the session heap's allocation counter at execution start, so equal programs from equal state produce byte-identical continuations; it is continuation state, so a resumed execution recognises the handles it minted.

Pending tool requests and the execution nonce are VM roots and continuation state. Each pending request is consumed by await; an abandoned request at cell end produces a typed runtime error. The bytecode format advances to 12 (FIG-2764 took 11 on main for the comprehension batch instruction), continuation format from 8 to 9, VM ABI from v6 to v7, and semantic hash to v5 (FIG-2764 took v4); the execution nonce is part of that same continuation format 9 rather than a further increment.

Lashlang literal aggregates keep their source-order rejection rule and their existing language teaching. FIG-2764 direct comprehension batching is owned by ADR 0086; nested composition shares this decision's two-phase settlement.

Nested Lashlang comprehensions in tuples, lists, records, or another comprehension capture their receiver and argument values recursively before expanding to one batch. Runtime-bound Lashlang lists, tuples, and records are also walked recursively during the process phase, settling only process-handle leaves and retaining pure leaves unchanged; TypeScript Promise elements remain deliberately shallow. Main's direct-call comprehension instruction remains in place. The expanded batch uses the same two-phase process/tool settlement as literal aggregates. Visibly settled awaits fail during linking; dynamically settled leaves retain `AwaitExpectsHandle` and identify their type and nested path. The continuation guard also covers the serialized `RuntimeError` vocabulary, independently pinned by a serde variant-list test and a suspended-finally round trip.

## Consequences

Array shape is determined at runtime rather than by syntax. The host still receives one resource-operation batch for the tool leaves, and all of them settle before TypeScript reports the first-settled rejection; process leaves are awaited only after that batch succeeded. Existing continuation blobs require their original format and compiled program; this version does not reinterpret them.

## Superseded (2026-09-13, FIG-2990); replaced (2026-09-14, FIG-2996)

[ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md) makes `processes.await` a Durable Wait on the work-driver seam and
the VM keeps one handle kind, so a mixed aggregate is one resource-operation
batch settling on one recorded order. The two-phase tool-then-process rule
above, the process-leaf settlement walk, and the execution-nonce-stamped
second handle encoding are deleted; the runtime-array evaluation rule and the
journaled-order selection rule survive in that ADR.

That replacement has landed. What is true now, in place of the two-phase rule:

- **One batch, one recorded order.** An awaited aggregate settles as a single
  resource-operation batch. The host's recorded settlement order over its
  leaves is authoritative and decides which rejection an unwrapping aggregate
  reports, whichever leaf it came from. There is no phase ordering left for a
  tool rejection to win by: `Promise.all([tools.x.op(), processes.await(h)])`
  reports the failure the batch recorded first.
- **A durable wait is a leaf.** `processes.await` is a leaf tool that parks on
  a Durable Wait, and a parked leaf takes its place in the recorded order at
  the moment its completion arrives, not at the position it was launched in.
  It is not a batch child with a cancel grace, which is why the wait may last
  days without the aggregate losing its ordering.
- **A process handle is not an aggregate leaf.** Writing a raw handle at an
  element position was how a process reached the retired second phase. It is
  now refused with a repair naming `processes.await(handle)`. A handle *inside*
  a value bound to a name is still carried through untouched: settlement is
  shallow over element positions (ADR 0096), a rule this ADR's recursive
  process-leaf walk had already lost.
- **One handle encoding.** `{__handle__: "lash", id}` with the execution nonce
  folded into the id, and `AwaitedValue` is `{Leaf, Plain}`: a value handed to
  `await` is a handle record or it is not, and what the id names decides which
  repair a handle that names no live request gets.

Every law this ADR asserted has a replacement asserted under the single batch
order; the replacement table is on the FIG-2996 part-4 pull request.
