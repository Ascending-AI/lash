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

Checkpoint components form a keyed set. An execution-state root names the
complete current set, with inline bodies or leaf descriptors. A logical key
belongs to the root; a content ref identifies logical bytes.

A keyed component has a changed body, an unchanged reference, or disappears
from the root's complete set. Stores hydrate unchanged refs and fail with a
typed component error when a required ref cannot be resolved. Root and changed
leaves commit in the same transaction. A backend does not replace the keyed
contract with one opaque execution-state body.

### Runtime roots and heap fragments

The RLM root contains the engine ID, a durable state header, globals, deferred
tool resolutions and deferred trigger resolutions. Each global body is a
durable fragment containing its value and its assigned heap objects. The
header carries format and heap-accounting information. Guest scratch files are
outside this root.

Roots are walked in name order and objects in member order. The first root to
reach an object carries it; other fragments refer to its heap ID. Every live
object is carried once. Shared references and TypeScript exotic state therefore
survive capture and reload without conversion to independent host trees.

Heap mutation stamps identify changed fragments, including writes through
aliases. A capture re-encodes a fragment when its value, assigned objects or
object contents change. Warm and cold captures preserve the same logical state.
ADR 0076 owns the runtime-root, host-view and shared-graph boundary.

### Granularity and encoding

`lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES` is the one 512-byte
inline/leaf threshold. A fragment below it stays inline; a fragment at or above
it becomes a leaf. Encoded size determines granularity, regardless of value
shape. Backend compression policy cannot select the threshold, because it
answers how to store bytes rather than which values deserve components.

Roots and fragments use canonical typed MessagePack:

- non-finite floats round-trip, with one encoded NaN pattern;
- unsupported durable values fail during capture with a named location;
- dynamic maps sort their keys, while record fields retain observable property
  order;
- typed structures use declared named-field order; and
- byte fields use MessagePack bytes through `serde_bytes`.

Canonical structure and depth checks run at the boundary. Restore checks the
complete leaf set and admits component and embedded format versions through
the fleet's registered read windows. The current versions live in
`lash::formats`; ADR 0115 governs upgrade compatibility. Shape edits during the
pre-1.0 freeze do not add version bumps or upcasters.

Component identity is BLAKE3 over uncompressed logical bytes in the
`lash-blob/v2` domain. Compression follows hashing. The commit budget counts
the root manifest and changed bodies; unchanged body references add no body
bytes. Atomic root/leaf storage avoids a write-before-reachability window.

Reclamation remains host policy under ADR 0014 and ADR 0023. The root supplies
reachability; it does not choose a retention horizon.

## Why

A reusable root-and-leaf representation gives every backend the same
checkpoint contract. Shape-based granularity would inline large scalar strings
and allocate leaves for tiny composite values. Size-based granularity directly
compares the encoded body with component overhead.

A host projection can omit exotics and duplicate shared objects. Persisting
runtime fragments preserves the actual durable state. A restore-time fallback
to null would hide capture failures, so unsupported values fail at the writer.

## Consequences

- Changed leaf bodies determine incremental body cost, while the root still
  carries the complete key set.
- Shared heap identity, exotics, property order and non-finite values survive
  supported durable round trips.
- Canonical encoding is part of content identity and must match at both ends.
- Chunking within an oversized leaf and lazy hydration are separate decisions;
  the keyed contract does not require either.

## Code evidence

- [Root and persisted-value shapes](../../crates/lash-protocol-rlm/src/executor/state.rs#L22).
- [Capture and leaf assembly](../../crates/lash-protocol-rlm/src/executor/state.rs#L818).
- [Heap partition](../../crates/lashlang/src/runtime/heap/partition.rs#L1).
- [Threshold](../../crates/lash-core-execution/src/plugin/protocol.rs#L239).
- [Component descriptors and hydration](../../crates/lash-core-store/src/store/checkpoint.rs#L131).
- [Budget measurement](../../crates/lash-core-store/src/store/commit_budget.rs#L267).
- [Content hash](../../crates/lash-core-store/src/store/mod.rs#L329).
