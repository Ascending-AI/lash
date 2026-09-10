# Parent relationships do not define a second session model

## Status

Accepted. Ratified on FIG-2872.

## Context

Lash has described sessions created from another session as children, managed
sessions, and subagent sessions. Those names leaked into construction and
lifecycle: an opened facade session could create a runtime through the managed
session registry without passing through the facade's canonical store
admission, and a facade administration operation could replace the runtime
inside an existing session handle with one of those managed runtimes. The
result admitted two execution models. In one, the facade bound an exact session
identity, store, and lifecycle owner. In the other, parentage selected different
storage and identity rules.

A subagent needs inherited context, restricted tools, usage attribution, and a
parent relationship. None of those facts changes what a session is or what must
be true before it executes.

## Decision

Every executing facade session is an ordinary session. It passes through the
same canonical admission boundary and receives one immutable Session Binding
as defined by
[ADR 0088](0088-facade-sessions-bind-storage-and-lifecycle-owners.md), whether
it is opened by a host, forked, created from another session, or created to run
a subagent. Session relation, subagent context, tool access, policy, initial
state, and usage attribution are request data applied at that boundary. They
do not select another storage, control, lifecycle, or identity model.

Creating a related session therefore must select and admit its exact store
before publishing an executable handle. A parent relationship may influence
catalog selection and host policy, but it cannot reuse the parent's exact
store handle or exempt the new session from admission. If the selected catalog
cannot provide a store, creation fails before the session becomes executable.
The resulting handle owns the new session's binding; an existing handle never
changes its session id or binding to point at the result.

The managed-session registry remains an implementation detail for resident
runtimes, not an admission boundary or a second kind of session. APIs may
create, address, or close related sessions, but they must not swap a different
runtime identity underneath an opened facade handle.

ADR 0011 remains orthogonal. A runtime instantiated solely to execute an
already-admitted Runtime Process is reconstructed from that process's captured
Execution Environment and is identified by its process Execution Scope. It is
not a host-addressable facade session merely because lower-level code reuses
runtime machinery. A `ProcessInput::SessionTurn` that creates and executes a
real session is covered by this decision and receives no process-only
exemption.

This decision supersedes ADR 0088's wording that gave managed children a
separate storage-selection rule. Catalog choice may use the parent relation as
input, but canonical admission and binding are shared by every facade session.

## Consequences

- Subagents differ through relationship and configuration metadata, not
  lifecycle machinery.
- A related-session creation route cannot publish a storeless executable
  runtime when facade admission requires a store.
- Session handles keep one identity and one Session Binding for their entire
  lifetime; moving foreground work to another session requires another handle.
- Tests and examples must exercise related sessions through the same admission,
  control, resume, and failure contracts as root sessions.
- Process reconstruction remains governed by ADR 0011 only where no
  host-addressable facade session is being created.
