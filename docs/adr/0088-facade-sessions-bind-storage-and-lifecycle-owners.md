# Facade sessions bind storage and lifecycle owners

## Status

Accepted. Ratified on FIG-2872.

## Context

The facade previously treated session persistence as optional after open. A
session could use an explicit store while cancellation, resume, observation,
or administration later consulted the core's factory and effect host. Those
independent lookups could address the same session id in different persistence
or control deployments. The storeless branch also skipped the acceptance,
lease, cancellation-evidence, and recovery rules required by ADR 0069.

## Decision

Every successfully opened facade session owns one immutable, privately
constructed binding to its exact `RuntimePersistence` handle and lifecycle
services. Per-session reads, queued-input operations, cancellation, park, and
resume use that binding. Resume carries the binding forward while taking live
provider, plugin, prompt, tracing, and policy configuration from the receiving
core. It does not substitute the receiving core's lifecycle services.

An explicit store on `SessionBuilder` wins for that root session. Otherwise the
core catalog creates or opens the store. With neither, open returns
`MissingSessionStore` before admission or execution. Related sessions use the
same admission and binding model; their relation may inform catalog selection,
but never permits reusing a parent's exact store or publishing a storeless
executable runtime. [ADR 0089](0089-parent-relationships-do-not-define-a-second-session-model.md)
records that broader session-model invariant. An explicitly chosen in-memory
factory is the ephemeral facade configuration; there is no hidden storeless
facade mode.

Exact-session work drivers accept one session id and one store handle.
Arbitrary-session drivers accept a catalog and resolve the addressed store once
after the cancellation gate is known to exist, retaining that handle through
recording and receipt readback. Unknown or revoked gates return without a
catalog lookup or cancellation row. An address outside an exact binding is
refused before touching storage or controls.

Catalog and administration capabilities are separate from an opened session.
Catalog operations without a catalog return `SessionCatalogUnavailable`.
Deletion consumes an owner-issued `SessionDeleteContext` containing the
selected administration services and a controller scoped by the same trusted
backend adapter. Ordinary callers cannot combine an arbitrary controller with
another core's catalog. Native and store-replay administration scopes through
its retained effect host. Restate installs one administration adapter and mints
the context from the handler-borrowed controller. Restate's process deletion
keeps its existing direct process-command behavior; this decision does not add
a new journal command.

`SessionDeleteExecution` is a trusted backend extension boundary. Rust keeps
the administration and controller paired after issuance, but it cannot verify
that an external SDK context is routed to the same physical deployment. The
host must attest that installation and routing composition. No deployment id,
global registry, or wire discriminator is added.

This supersedes ADR 0049's storeless-session paragraph. Its single-use session
id and tombstone rules continue to apply to every explicitly selected store.
ADR 0069's unconditional durable acceptance remains the facade rule. ADR
0011's self-contained process reconstruction remains intact: internal process
runtime paths may still carry optional store capabilities where their substrate
contract requires it.

## Consequences

- A facade session cannot execute without a real store, including in memory.
- Session relation may influence catalog selection, but every facade session
  passes through the same admission and binding contract.
- Parked sessions retain attachment, process-environment, trigger, process,
  queue, effect, and exact-store ownership across resume.
- Deletion retries can finish effect-journal retirement after the catalog has
  already committed the permanent session tombstone.
- Hosts that compose third-party backends remain responsible for truthful
  physical pairing of their catalog, lifecycle services, and engine context.
