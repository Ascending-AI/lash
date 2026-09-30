# 0092: Agent frame scope is explicit and resolvable

Status: Accepted

## Context

A frame identity identifies a real frame-open node. An empty identity makes
absence ambiguous and lets malformed records cross the identity boundary.

## Decision

`FrameNodeId` is non-empty. Construction and deserialization reject the empty
string. Optional frame fields use `None` for absence. Its valid serialized
representation is a transparent string.

Session graph reads project the current frame. `SessionGraph::read_model`
reads the active model, and `rewrite_active_read_tail` replaces the readable
tail from the nearest `FrameOpen` ancestor of the leaf. The rewrite retains
historical branches and excludes transient replacement messages. Its result is
a read projection and must not be committed against an existing durable head.
These operations do not take an optional historical frame selector.

Store-backed residency and historical frame reads follow
[ADR 0112](0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md).
The durable head's frame pointer is checked against graph ancestry. A caller
supplying a frame carrier constructs the checked core identity; an empty string
cannot express whole-history access.

Evidence: `crates/lash-core-store/src/session_identity.rs:30`, `:65`, `:85`,
`crates/lash-core-store/src/session_graph.rs:1292`, `:1504`, `:1628`, and
`crates/lash-core-store/src/store/runtime_commit_plan.rs:426`.

## Alternatives considered

An empty-ID sentinel conflates absence and identity. A validated type plus
explicit optionality gives stored and runtime records one absence encoding.
Arbitrary historical selection on a resident read obscures its bounded
current-frame contract; historical store reads name their frame explicitly.

## Consequences

- Serialized frame fields have one encoding for absence.
- Current-frame read rewrites change a projection without durable mutation.
- Invalid empty identities fail at construction or decode.
