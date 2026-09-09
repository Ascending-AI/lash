# 0086: TypeScript aggregates evaluate runtime arrays

Status: Accepted

## Context

FIG-2766 requires `Promise.all` and `Promise.allSettled` to accept array-valued expressions, including synchronous maps and arrays saved in bindings. The previous lowering only recognized literal arrays and direct async maps, and rejected otherwise valid mixtures of calls and computed values.

## Decision

TypeScript tool calls in expression position create pending tool handles. Evaluating the argument array captures each call's receiver, arguments, and source site. The aggregate operand is lowered at the top await depth wherever the aggregate itself is written, so a `Promise.all` nested inside another call's argument still produces pending handles that the batch unwraps rather than result envelopes the outer call would forward as values. Aggregate await builds a batch from those handles and passes it through the existing settlement-order validation and rejection selection. Non-handle elements retain their values. Direct async maps retain their existing callback driver.

**Mixed process/tool aggregates settle in two phases.** Phase one settles every tool handle as one resource-operation batch with the host's recorded settlement order. Phase two awaits every process handle in array order through the existing process-await ability, the same durable seam a direct `await handle` uses. The rejection an unwrapping aggregate (`Promise.all`) reports is the first-settled tool rejection by recorded order; only when no tool leaf rejected does the first rejecting process in array order reject the aggregate. `Promise.allSettled` reports every outcome in array order. Lashlang's compiled aggregate (`await [h, module.op(...)]`) follows the same two phases, with its source-order rejection rule for unwrapped module leaves and a result record for each process leaf.

**A tool handle is an execution-scoped identity.** Each handle record carries the minting execution's nonce alongside its request index; `await` reaches a request slot only through a handle stamped with the current execution's nonce. A handle from an earlier execution or a hand-written handle record is refused with the typed pending-tool error, as is a handle awaited twice, a plain value awaited as a handle, and a pending handle passed inside a tool call's arguments (refused before dispatch, so the host never performs the call). Root bindings whose value carries a tool handle are omitted from the exported session globals, as closures already are. The nonce is derived deterministically from the session heap's allocation counter at execution start, so equal programs from equal state produce byte-identical continuations; it is continuation state, so a resumed execution recognises the handles it minted.

Pending tool requests and the execution nonce are VM roots and continuation state. Each pending request is consumed by await; an abandoned request at cell end produces a typed runtime error. The bytecode format advances from 10 to 11, continuation format from 8 to 9, VM ABI from v6 to v7, and semantic hash from v3 to v4; the execution nonce is part of that same continuation format 9 rather than a further increment.

Lashlang literal aggregates keep their source-order rejection rule and their existing language teaching. This decision does not implement FIG-2764.

## Consequences

Array shape is determined at runtime rather than by syntax. The host still receives one resource-operation batch for the tool leaves, and all of them settle before TypeScript reports the first-settled rejection; process leaves are awaited only after that batch succeeded. Existing continuation blobs require their original format and compiled program; this version does not reinterpret them.
