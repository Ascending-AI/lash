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

A session's protocol execution state is one checkpoint graph (FIG-5887): a
SessionExecution root (node kind 29) the session manifest names as a typed
child, over content-addressed nodes. Plugin namespaces and the other
manifest components stay keyed components; execution state is not one of
them.

A protocol captures `ExecutionGraphCapture::Clear` or
`Replace { root, nodes }`. `nodes` holds only the nodes no acknowledged
capture installed: a capture after a refused commit sends the same nodes
again, and an acknowledged one drops them from the next. A capture names its
purpose: `Working` writes the unfinished turn's working root after a cell
ends, `Final` the session manifest's root at the turn's commit; both write
the same root for the same state. Stores install the root and its new nodes
in the commit's transaction and refuse a commit whose root reaches a node
that is neither staged nor installed. Restore hands the protocol a
`HydratedExecutionGraph`: the root and every node it reaches.

### Runtime roots and heap state

The code mode root carries the engine ID, the globals, deferred tool and
deferred-call resolutions and the saved function bundles. Values keep their
shared references, exotics, property order and non-finite floats across
capture and reload. ADR 0076 owns the runtime-root, host-view and
shared-graph boundary.

### Granularity and encoding

Nodes use the canonical binary grammar of
[`checkpoint_graph::node`](../../crates/lash-core-store/src/checkpoint_graph/node.rs)
(FIG-5887). A text or byte token
over `checkpoint_graph::INLINE_TOKEN_MAX` (256 bytes, tag and length
included) becomes its own leaf node, chunked under an index when it exceeds
one leaf. A large value is therefore written once: a later capture whose
value is unchanged names the installed leaf and stages no bytes for it.
Encoded size determines granularity, regardless of value shape.

Node identity is BLAKE3 over the canonical node bytes; a node's edges are
exactly its child table, which is what collection walks. The commit budget
counts the encoded nodes the commit stages; installed nodes it names add no
bytes. Canonical structure and decode limits are checked at the boundary.
Shape edits during the pre-1.0 freeze change shapes in place, with no
version bump or upcaster.

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
- Lazy hydration is a separate decision; the graph contract does not require
  it.

## Code evidence

- [Capture and purpose](../../crates/lash-core-store/src/execution_state.rs#L35).
- [Code mode capture](../../crates/lash-protocol-rlm/src/executor/session.rs#L919).
- [Leaf placement](../../crates/lash-core-store/src/checkpoint_graph/encode.rs#L253).
- [Inline bound](../../crates/lash-core-store/src/checkpoint_graph/node.rs#L28).
- [Manifest and execution](../../crates/lash-core-store/src/store/checkpoint.rs#L62).

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
