# Parent relationships do not define a second session model

## Status

Accepted.

## Context

A related session needs a parent relationship, inherited context, tool access,
and usage attribution. Those facts describe its inputs; they do not define a
different storage identity or lifecycle.

## Decision

Every executing facade session passes through canonical store admission and
owns one immutable Session Binding, as defined by
[ADR 0088](0088-facade-sessions-bind-storage-and-lifecycle-owners.md).
Root sessions, related sessions and forks use this boundary.

The facade records parentage only at creation.
`create(SessionCreation { parent: Some(parent), .. })` writes the relation
with the catalog row and initial config head, then returns a Durable Session.
Creation constructs no runtime. An existing id is refused with
`SessionAlreadyExists`, even when the request repeats its parent and config;
a deleted id is refused with `SessionDeleted`. Opening resolves an existing
session, reads its recorded state and parent relation, and constructs its bound
runtime. An open cannot state or change parentage.

Each related session has its own exact session store binding. Usage remains data on its model results (ADR 0127).
Parentage cannot exempt it from admission or substitute the parent's binding.
A handle keeps its session identity and binding for its lifetime.

The process-origin initialisation path materializes an ordinary session from
`SessionCreateRequest`, carrying relation, policy, tool access, initial nodes,
observer intents, and plugin configuration. The request states everything the
child starts from; the parent relation is weak lineage that nothing reads
([ADR 0134](0134-creating-a-session-is-explicit-only-a-fork-clones.md)). The runtime
that executes a `ProcessInput::SessionTurn` is local to that process run. A
redelivery reopens its durable session row; no live child-runtime registry
supplies continuity.

Initialisation and catalog `fork_session` are separate operations. A fork
materializes durable lineage and retained-frame content under its own failure
contract, and it is the only creation that copies another session's state.
[ADR 0011](0011-self-contained-processes.md) governs reconstruction of a Runtime
Process from its captured Execution Environment. Creating and executing a real
child session still requires ordinary admission.

Evidence: `crates/lash/src/session.rs:52`, `:164`, `:247`, `:402`, and
`crates/lash-core/src/runtime/session_manager/session_init.rs:1`, `:52`, `:85`.

## Alternatives considered

A separate child runtime registry would make parentage select another storage
and lifecycle model. Durable session rows already provide continuity.
Rebinding an existing handle would change its identity beneath callers; moving
foreground work requires a different handle.

## Consequences

- A delegated child differs only through the configuration its creator states; delegation itself is a host plugin's concern (ADR 0134).
- Admission, control, resume, and failure contracts apply to related sessions.
- Rolling related sessions' usage together is host policy.
- Process-run residency ends with the run; durable session identity survives it.
