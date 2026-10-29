# Checkpoint components generalize to a keyed set

## Status

Accepted.

## Context

A session can retain far more execution state than one turn changes. Rewriting
all retained values makes a commit budget track session size. Attachment writes
do not have the checkpoint transaction's visibility and reachability contract.
ADR 0048 supplies content-addressed component identity; execution state uses
that contract at finer granularity.

## Decision

A session's protocol execution state is one descriptor page the session
manifest names as a typed child (`SessionRoot`, kind 1008), over the pages
of the kernel's persistent heap
([ADR 0145](0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)
§9). The descriptor names the session's dialect, number policy and kernel
version, its carried heap, binding and saved-function roots, and the names
it binds or does not carry. Plugin namespaces and the other manifest
components stay keyed components; execution state is not one of them.

A protocol captures `ExecutionGraphCapture::Clear` or
`Replace { root, nodes }`. `root` is the descriptor and `nodes` holds only
the pages no acknowledged capture installed: the pages a turn's cells
staged under it. A capture after a refused commit sends the same pages
again, and an acknowledged one settles them. A capture names its purpose:
`Working` writes the unfinished turn's working root after a cell ends,
`Final` the session manifest's root at the turn's commit; both write the
same root for the same state. Stores install the root and its new pages in
the commit's transaction and refuse a commit whose root reaches a page that
is neither staged nor installed. Restore hands the protocol a
`HydratedExecutionGraph`: the descriptor page, never the heap pages under
it. A binding's value is read on demand through the page exchange, under a
root lease, when a cell or a host reads it.

### Runtime roots and heap state

The code mode descriptor carries the carried bindings and the saved
function bundles as heap roots. Values keep their shared references,
exotics, property order and non-finite floats across capture and reload,
because the next cell adopts the same heap rather than a copy of it. ADR 0076 owns the runtime-root, host-view and
shared-graph boundary.

### Granularity and encoding

Nodes use the canonical binary grammar of
[`checkpoint_graph::node`](../../crates/lash-checkpoint-encoder/src/node.rs).
Heap pages are canonical trees of at most 8 KiB leaves, and immutable text,
bytes and integer digits over 256 bytes are 4 KiB payload leaves under an
index. A large value is therefore written once: a later capture whose value
is unchanged names the installed pages and stages no bytes for them, and a
point edit stages one leaf and its index path. Encoded size determines
granularity, regardless of value shape.

Node identity is the blob-domain BLAKE3 digest of the canonical node bytes; a node's edges are
exactly its child table, which is what collection walks. The commit budget
counts the encoded nodes the commit stages; installed nodes it names add no
bytes. Canonical structure and decode limits are checked at the boundary.
Shape edits during the pre-1.0 freeze change shapes in place, with no
version bump or upcaster.

JSON-text roots retain the exact stored string as their identity. In
`ModelCallFact` and the round/head environment roots, whitespace and member
order may produce different graph nodes for equivalent typed records. These
roots do not promise semantic one-spelling identity and do not reject an
otherwise valid record for its JSON text spelling. Typed JSON value-tree
payloads retain their decode/re-encode one-spelling check.


Reclamation remains host policy under ADR 0014 and ADR 0023. The root supplies
reachability; it does not choose a retention horizon.

## Why

One graph gives every backend the same checkpoint contract, and the store
can walk it without decoding the protocol. Size-based leaves keep a large
immutable value out of every later capture: the commit carries the changed
structure, not a rewrite of unchanged payloads.

A host projection can omit exotics and duplicate shared objects. Persisting
the runtime's own state preserves it. A restore-time fallback to null would
hide capture failures, so unsupported values fail at the writer.

## Consequences

- Newly installed nodes determine a capture's cost; the root still names the
  complete state.
- Shared heap identity, exotics, property order and non-finite values survive
  supported durable round trips.
- Canonical encoding is part of content identity and must match at both ends.
- A restore reads the descriptor only; heap pages load when a cell or a
  host reads them (ADR 0145 §3).

## Code evidence

- [Capture and purpose](../../crates/lash-core-store/src/execution_state.rs#L35).
- [Code mode capture](../../crates/lash-protocol-rlm/src/executor/session.rs#L607).
- [Leaf placement](../../crates/lash-checkpoint-encoder/src/encode.rs#L253).
- [Inline bound](../../crates/lash-checkpoint-encoder/src/node.rs#L28).
- [Session descriptor](../../crates/lash-protocol-rlm/src/executor/session/graph.rs#L10).
- [Manifest and execution](../../crates/lash-core-store/src/store/checkpoint.rs#L62).

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
