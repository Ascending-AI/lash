# 0086: TypeScript aggregates evaluate runtime arrays

Status: Accepted

## Context

FIG-2766 requires `Promise.all` and `Promise.allSettled` to accept array-valued expressions, including synchronous maps and arrays saved in bindings. The previous lowering only recognized literal arrays and direct async maps, and rejected otherwise valid mixtures of calls and computed values.

## Decision

TypeScript tool calls in expression position create pending tool handles. Evaluating the argument array captures each call's receiver, arguments, and source site. Aggregate await builds a batch from those handles and passes it through the existing settlement-order validation and rejection selection. Non-handle elements retain their values. Direct async maps retain their existing callback driver. Process promises retain their explicit separate-await requirement: their host operation is not a resource-operation batch, which cannot currently express settlement order across a mixed process/tool aggregate.

Pending tool requests are VM roots and continuation state. Each pending request is consumed by await; an abandoned request at cell end and an await of a settled non-handle produce a typed runtime error. The bytecode format advances from 10 to 11, continuation format from 8 to 9, VM ABI from v6 to v7, and semantic hash from v3 to v4.

Lashlang literal aggregates keep their source-order rejection rule and their existing language teaching. This decision does not implement FIG-2764.

## Consequences

Array shape is determined at runtime rather than by syntax. The host still receives one resource-operation batch, and all leaves settle before TypeScript reports the first-settled rejection. Existing continuation blobs require their original format and compiled program; this version does not reinterpret them.
