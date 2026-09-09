# 0086: TypeScript aggregates evaluate runtime arrays

Status: Accepted

## Context

FIG-2766 requires `Promise.all` and `Promise.allSettled` to accept array-valued expressions, including synchronous maps and arrays saved in bindings. The previous lowering only recognized literal arrays and direct async maps, and rejected otherwise valid mixtures of calls and computed values.

## Decision

TypeScript tool calls in expression position create pending tool handles. Evaluating the argument array captures each call's receiver, arguments, and source site. Aggregate await builds a batch from those handles and passes it through the existing settlement-order validation and rejection selection. Non-handle elements retain their values. Direct async maps retain their existing callback driver. Process promises retain their explicit separate-await requirement: their host operation is not a resource-operation batch, which cannot currently express settlement order across a mixed process/tool aggregate.

Pending tool requests are VM roots and continuation state. Each pending request is consumed by await; an abandoned request at cell end and an await of a settled non-handle produce a typed runtime error. The bytecode format advances from 10 to 11, continuation format from 8 to 9, VM ABI from v6 to v7, and semantic hash from v3 to v4.

Lashlang aggregates keep their source-order rejection rule.

### FIG-2764: Lashlang comprehension aggregates

An outer `await` over a comprehension of operation calls captures each iteration's receivers and arguments, then submits one host batch. The existing aggregate shape and settlement path apply per-leaf `?` after every call settles, reporting the first rejection in written order. An `await` inside the element remains sequential. Empty call comprehensions return an empty list without host work.

Awaiting settled scalars or containers raises `AwaitedSettledValue`, including the value kind and supported call/handle forms; it never substitutes error records into ordinary fields. Statically visible settled expressions are rejected by the linker. Literal aggregates containing calls still preserve their pure fields.

This extension advances bytecode 11 to 12, VM ABI v7 to v8, and semantic hash v4 to v5 on top of #1175 (FIG-2766). #1175 is unreleased, so this stack retains one continuation-format bump from 8 to 9. The v9 wire includes `AwaitedSettledValue` and `InvalidResourceComprehensionElement` in `RuntimeError`, reachable through a suspended finally’s `VmPendingErrorOriginContinuation.error`. The structural-validation test pins the complete serialized error-variant vocabulary, and the version guard covers the error shapes; artifact and cache identities change with the bytecode/ABI/semantic versions, and continuation callers must supply the same content-addressed compiled program.

## Consequences

Array shape is determined at runtime rather than by syntax. The host still receives one resource-operation batch, and all leaves settle before TypeScript reports the first-settled rejection. Existing continuation blobs require their original format and compiled program; this version does not reinterpret them.
