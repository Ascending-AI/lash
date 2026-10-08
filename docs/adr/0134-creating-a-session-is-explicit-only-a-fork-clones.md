# 0134: Creating a session is explicit; only a fork clones

## Status

Accepted. The rules are on main (FIG-5296). `scripts/check-no-subagent.py`
keeps the core crates, `lash-sansio`, `lash-durable` and the facade free of
any subagent concept.

## Context

A session can be created as a root, as a child that names a parent, or as a
fork of a committed frame. When a child silently copied its parent's policy,
reasoning, model profile and every plugin's configuration, a child's
behaviour depended on which session created it rather than on what its
creator asked for. The same request then produced different sessions, a
parent's later state could leak into a child it had already spawned, and core
had to carry a delegation concept (a capability, a depth limit, a parent
initialization capture) that only one plugin read.

## Decision

### 1. Creation is explicit

A non-fork session starts from exactly what its create request states and the
defaults of each config owner. `ConfigOwner::create` sees only its creator's
input. A request that states no policy is refused with
`CoreConfigRefusal::PolicyUnstated`: a policy has no neutral turn budget or
tool-call limit to default to. An unstated reasoning selection is the default
selection. No owner reads another session to fill in what a request omits.

### 2. Only a fork clones

A catalog fork (`fork_session`, `fork_at`) copies everything committed at the
fork point: config, plugin state and retained frame content. It is the one
operation that makes a session from another session's state.

### 3. The parent link is weak lineage

`SessionRelation::Child { parent_session_id, caused_by }` is recorded at
creation for display and audit. It has no behavioural effect: no config,
prompt, tool surface or protocol default reads it, and deleting, cancelling
or closing the parent does nothing to the child session. A linked child
behaves like an unlinked session created with the same input. Ending a
child's work with its parent's is process scope's job (ADR 0108), never the
session link's.

Process lineage, causal links and the session capability and authority
scopes from an admitted start context stay. They describe the process that
creates a session, not the session it creates.

### 4. Core has no subagent concept

Core knows sessions, an optional parent link and forks. What a delegated
child is, what it may do and what context it carries belong to the host's own
plugin, in that plugin's own config namespace. Lash ships no subagent
implementation. `examples/delegation` shows a host-written delegation tool on
the facade alone: it creates the child with explicit input, runs it as a
`SessionTurn` start and routes the child's result back to the calling turn.

## Consequences

- The same create request yields the same session, whoever creates it.
- A host that wants a child to resemble its parent states that configuration
  in the request, or forks.
- Stored session config and authority carry no subagent field.
- A host cancels delegated work through its process or scope, not through the
  parent link.

## Alternatives considered

Inheriting by default with an opt-out keeps the creator-dependent behaviour
for every host that does not opt out, and keeps core reading the parent.
Keeping a neutral delegation context in core (a depth and a label) gives core
a concept no core rule reads; a plugin records what it needs in its own
namespace.
