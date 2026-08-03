# 0051. The facade is the host API; lash-core's public surface is its integrator seams

Date: 2026-08-03
Status: accepted

## Decision

The `lash` crate is the supported host API, in full. `lash-core` keeps a public
item exactly when a named integrator class needs it, and for no other reason.
Everything else in `lash-core` is crate-private or reachable only through
internal seams the facade consumes. Public surface is a promise we test and
keep; anything we are not prepared to promise is not public.

The named integrator classes, each seeded by the interfaces an implementor must
write against, are:

1. **Store and durable-substrate implementors** — the store traits
   (`RuntimePersistence`, `SessionStoreFactory`, `ProcessRegistry`,
   `TriggerStore`, `AttachmentStore`, `LiveReplayStore`,
   `ProcessExecutionEnvStore`, `ProcessContinuationStore`, and their sibling
   store contracts) and every type their signatures reach.
2. **Effect-host implementors** — `EffectHost`, `RuntimeEffectController`,
   `AwaitEventResolver`, and their signature closure.
3. **Protocol and process-engine implementors** — `ProtocolSessionPlugin`,
   `ProtocolDriverPlugin`, `CodeExecutorPlugin`, `ProcessEngine`, and the other
   engine extension points, with their closure.
4. **Conformance-suite embedders** — everything
   `lash::testing::conformance` exposes, closed over its signatures, so an
   integrator can hold a custom backend to the same executable contract the
   built-in backends answer to.

Membership is decided by **transitive signature closure**, not by direct-use
scanning. A type that appears only in a public trait method's parameters or
return type is integrator surface, whether or not any repository code names it
today: `ExecRequest` exists because `CodeExecutorPlugin::execute_code` exists.
The narrowing measurement found 683 items that a direct-use scan classified as
removable but that the closure proves are load-bearing for implementors; the
closure rule is therefore normative, and any future tooling that classifies
surface must apply it.

The package-version constants (`lash_core::VERSION`, `SANSIO_VERSION`) are not
integrator surface. Compatibility between an integrator and the runtime is
expressed through trait contracts, data shapes, and the schema-version
machinery — never by gating on a package version.

## Why

Three defects in one week were unused-public-surface defects: a drain API with
zero callers concealed a wake-mark over-claim; an append precondition with one
since-deleted caller was read in opposite ways by two capable reviewers; an
orphaned retention lever meant deleting an unsafe deletion left a table with no
reclamation path. Surface nobody consumes drifts until its first consumer
discovers what it actually does. The example-coverage rule (the inventory and
its CI contract) makes such drift visible, but visibility alone would have us
writing examples against 5,424 public core items — most of which exist only
because the facade's implementation happens to live in another crate. Narrowing
visibility deletes no functionality: hosts keep everything through the facade.
It deletes *promises we never meant to make*.

The measurement behind this decision classified every public `lash-core` item:
2,848 are reachable by no integrator class (942 of them by nothing at all,
including the facade); 2,574 are integrator surface under the closure; 2 were
contested and are resolved above. The facade's own 6,821 items produced zero
trim candidates — the facade is already the deliberate API this ADR makes
authoritative.

## Consequences

- Anything consuming `lash-core` directly that is not one of the four classes
  is unsupported. The measured exceptions — 79 items held only by direct
  example or downstream-host use — are treated as **facade gaps**: each gets a
  facade home (or the consumer moves to an existing one) rather than a
  perpetual carve-out.
- The example-coverage inventory shrinks with the surface, and the coverage
  end-state ("every public API used and tested through our examples") is now a
  statement about surface we deliberately promise. Conformance suites remain
  the integrator classes' executable contract, and integrator-facing surface
  is exercised by integrator-class examples rather than shoehorned into host
  examples.
- Internalization proceeds in waves: first the zero-consumer items, then the
  facade-gap moves, then the remaining facade-only items. Each wave is
  breaking for direct `lash-core` consumers and carries `Breaking:` release
  notes naming the facade or seam replacement.
- New public items in `lash-core` must name their integrator class in rustdoc.
  An item that cannot name one belongs behind the facade.
