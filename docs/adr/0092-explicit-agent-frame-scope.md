# 0092: Agent frame scope is explicit and resolvable

Status: Accepted

## Context

Session graph reads and read-tail rewrites had three scope encodings: separate
scoped and unscoped methods, an optional internal parameter, and an empty frame
identity treated as whole-history access. A requested frame absent from the
active path also had two outcomes. Reads returned an empty projection, while
rewrites silently retained the graph.

Those outcomes erased the caller's intent. An empty result could mean an empty
frame or a missing frame, and a successful rewrite could mean either a rewrite
or no operation. The empty identity also let serialized data express unscoped
access through a value that otherwise claimed to identify a frame.

## Decision

Session graph history projection and read-tail replacement each have one
operation. Both take `Option<&FrameNodeId>`: `None` deliberately selects the
whole active history, and `Some` selects one Agent Frame on the active path.
The requested node must be a `FrameOpen` node on that path. If it cannot be
resolved, both operations return `SessionGraphScopeError::FrameNotFound`.

A rewrite resolves its scope before changing graph state. A failed scoped
rewrite therefore preserves the nodes, leaf, and derived projections exactly.
Selecting the root Agent Frame remains equivalent to an unscoped read when no
later frame boundary occurs.

`FrameNodeId` is a non-empty value type. Construction and deserialization
reject the empty string, and optional frame fields use `None` as their sole
absence representation. Runtime paths that require an initialized Agent Frame
return an error when it is absent instead of fabricating an empty identity.
There is no legacy empty-ID decoder, adapter, or migration.

The serialized representation remains a transparent string, so valid stored
identities retain their bytes and format generations do not advance. Previously
accepted empty identities fail at decode rather than being translated.

## Consequences

- A bad requested frame cannot look like empty history or a successful no-op.
- Read and rewrite callers make whole-history access visible with `None`.
- Serialized scope-bearing records cannot carry two encodings for absence.
- Hosts with legacy empty frame identities must discard or recreate that data.
