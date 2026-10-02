# 0108: A process lives until a scope its start could reach

## Status

Accepted.

## Context

A process's provenance says where it comes from. Its lifetime says which
scope requests its cancellation. The lifetime must be reachable from the
admitted start, and a logical turn must keep its children across a frame
switch. Session deletion needs durable evidence that also closes the
session's children.

## Decision

### 1. Lifetime is `Until(scope)` or `Detached`

A start chooses `Lifetime::Until(ScopeRef)` or `Lifetime::Detached`. A
`ScopeId` names either an effect opener (a turn root, session operation or
process) or a session. A `ScopeRef` pairs it with one grant:

- `Ancestor` names a scope in the start's recorded ancestry.
- `HostSessionLookup` names a session a host root start looks up.

`ScopeRef` has no deserializer and its public construction path is
`host_session_lookup`. Runtime starts obtain ancestor references through
`StartCx`. The journaled representation is `LifetimeDecision`, which carries
the chosen scope and grant. `Detached` names no cancelling scope.

Evidence:
`crates/lash-core-execution/src/runtime/process/model/scope_lifetime.rs:34`.

### 2. Every start records its ancestry

The runtime materializes `StartCx` from the admitted scope and enclosing
`ProcessLineage`. Ancestry lists the starter first, then its ancestors,
nearest first. A root start has empty ancestry.

A turn or drain lists its opener and session. If a process owns the session,
its recorded lineage follows. Session metadata records that owner as
`owning_process_id`. A process body's context comes from that process's
recorded lineage, including its session capability when it has one.

When live context lacks the enclosing process's lineage, the runtime reads
its row and refuses if it cannot obtain the required lineage. A host-driven
turn of an owned session reads the owner from session metadata and then the
owner's row. If retention has pruned that owner, the context contains the
turn and session without the owner's lineage.

Evidence:
`crates/lash-core-execution/src/runtime/process/model/scope_lifetime.rs:309`
and `crates/lash-core/src/runtime/session_manager/process_runners/control.rs:1177`.

### 3. Registration checks reachability, in every build

Registration refuses an ancestor-granted `Until` scope outside the recorded
ancestry. A host-lookup grant is valid only for a session on a root start.
A session capability must be in the ancestry or be the session granted to
that root by its `HostSessionLookup` lifetime. These checks return typed
registration refusals and run in production builds.

Evidence: `crates/lash-core-execution/src/runtime/process/validation.rs:1155`.

### 4. A policy decides, and the decision is journaled

A start-declaring plugin takes a required `LifetimePolicy`, a function from
`StartCx` to `Lifetime`. Declaration runs the policy and records its decision;
replay reads that decision without consulting the policy again.

`lifetime::session_or_starter` chooses the session when available and the
starter otherwise. `lifetime::starter` chooses the starter, and
`lifetime::detached` chooses no scope. A declared start follows these same
rules. A parked call's cancellation or deadline can owe its child a cancel
independently of the child's lifetime. Consumer holds follow ADR 0116 §3.6.

Evidence:
`crates/lash-core-execution/src/runtime/process/model/scope_lifetime.rs:126`
and `crates/lash-core-execution/src/runtime/process/model/scope_lifetime.rs:399`.

### 5. A scope closes once, after its end is durable

The scope-close ledger, `parent_end_plans`, records one row per closed scope.

- A process's terminal transaction records its process scope's end.
- A logical turn root closes after its terminal evidence is durable. Its
  terminal transaction arms `ScopeClose` on the root row. The recorded
  `CloseRootScope` step or the obligation relay delivers it. A frame switch
  does not close the logical root, and a parked root has no terminal close.
- A session closes through the engine half of its `CloseSession` intent.
  That half closes the open roots and session scope. Physical storage
  deletion writes no replacement close row.

The next root's admission does not wait for the terminal root's scope close.
The close remains owed through its durable obligation, and affects the
terminal root's `Until` children. Its transaction settles the obligation;
a repeat finds it settled. Recovery reads due obligations instead of
rescanning terminal roots.

Applying a close plan owes cancellation to processes living `Until` that
scope. `ParentEnd` delivery retains the plan's owed work until each cancel
is delivered or refused. [ADR 0109](0109-store-to-engine-delivery-is-an-outbox-of-obligations.md)
§3 owns delivery and retry policy.

Evidence: `crates/lash-sqlite-store/src/session_roots.rs:169`,
`crates/lash-core-execution/src/runtime/process/scope_close.rs:139`, and
`crates/lash-sqlite-store/src/process_registry/parent_end.rs:55`.

### 5a. The `CloseSession` intent is the deletion tombstone

Deletion records `CloseSession` before removing storage. The intent stays
pending while its obligation is owed and remains recoverable; a refusal
records `Refused { cause }`. Its acknowledgement arms the
physical-delete obligation. The acknowledged intent stays as permanent
deletion evidence, and `root_terminal` can answer roots of the deleted
session as `Cancelled` with cause `SessionDeleted`.

Session deletion and evidence reclamation preserve this tombstone. The
session's other control intents can be removed. A retried deletion completes
the recorded close rather than inventing a second intent.

Evidence: `crates/lash-sqlite-store/src/session_roots.rs:945`,
`crates/lash-sqlite-store/src/session_delete_ledger.rs:40`, and
`crates/lash-store-sql/src/session_roots/control_intents.rs:75`.

### 6. A closed scope starts nothing

A new registration checks its closing scopes in the registration
transaction. A closed starter or `Until` scope refuses the start, including
a detached start from a closed starter. The checks also cover the session
containing those scopes. A start racing a close either commits before the
close or sees the close row and refuses.

Evidence:
`crates/lash-sqlite-store/src/process_registry/registration.rs:41`.

## Consequences

Lifetime, ancestry and session capability are durable registration facts.
A start that must outlive its reachable scopes chooses `Detached` explicitly.
Provenance and wake routing grant no lifetime authority. Grant validation
keeps scope reachability explicit; it is not an authentication policy.

Caller-supplied parent data would admit scopes the start never reaches.
Cancelling at every physical turn boundary would end children during a
frame switch. A recorded grant and a logical-root close avoid both problems.
