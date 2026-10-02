# Durable tracing

Hosts install `lash::tracing::OtelTelemetry` through
`LashCoreBuilder::telemetry`, supplying their tracer and meter providers.
The adapter uses OpenTelemetry API 0.33. Hosts own sampling, resources,
export, provider flush and shutdown. Lash accepts one adapter per core;
`trace_sink` adds passive record consumers. The shared runtime also supplies
every plugin with its clock, scope, emitter and metric instruments.

The [instrumentation contract](../../crates/lash/docs/instrumentation-contract.md)
lists the registered spans, attributes and metrics. It describes what the
adapter can project, rather than proving that every engine producer emits
those observations. Arbitrary metadata and payload export are off by default.
An explicit payload option bounds exported bytes and events.

## Required durability contract

Admission retains a typed cause, its selected SDK-created anchor and the
original start time beside business data. The first accepted write wins,
including an untraced admission. A retry under another context reads the
winner. Trace provenance does not change business submission hashes,
effect identities, tool-call identities or graph-node identities.

Awaited work uses a parent edge. Independently admitted work starts a trace
with links to its producers. The receiving scope keeps its parent when a
signal resolves a wait or a checkpoint consumes another input. A current
execution attempt is an optional link, supplied through the engine interface;
it never replaces the retained parent.

A durable scope has a short admission span. Its logical completion creates
a new span under that retained anchor, using retained start and terminal
times. Children and the completion span are siblings under the admission
anchor. No SDK span or event buffer stays open across suspension.

Only a newly committed lifecycle transition emits a logical completion.
Only an executed journaled body emits a live attempt observation. Serving a
journal result or reading a retained terminal grants neither permission.
Product process and language graphs reconstruct through a separate observer
on replay, without exporting another lifecycle observation.

Export is best effort. A crash after commit and before emission can lose an
observation. A body that exports before its result commits can execute again
and export a distinct attempt. There is no telemetry outbox or exactly-once
export guarantee. Trace delivery does not decide control flow or billing.

## Integration status

At the FIG-4835 review of `f9dfed0c61`, the adapter, retained admission
provenance, transport attempt links and shared plugin runtime are present.
Whole-arc acceptance remains open pending the FIG-4830 integration follow-up:

- Production admission does not call the scope factory. Run admission
  explicitly offers `Untraced`, and the projector skips untraced scopes.
  The engine must persist the selected candidate before dispatching children.
- Production sites do not yet emit `DomainCompleted` or
  `LlmAttemptCompleted`. Adapter fixtures alone do not prove run, process,
  send, intent or provider-attempt spans are emitted by the engine.
- Turn, tool and process observations still construct temporary untraced
  scopes and use attempt observations. They need the retained scopes and
  terminal receipts for logical completion identities and durations.
- Replay-only wait resolutions and process conclusion need committed
  ownership. Several transition metric callers still pass no permit and
  consequently publish no counter.
- `emit_unscoped` still uses the obsolete random-ID record constructor.
- Shift observations construct their records and read the clock before the
  replay frontier grants permission. The replay cost law needs to cover that
  producer path, as well as the emitter's early return.
- The existing P3 law checks replayed record labels and a body counter. It
  does not yet prove the full exported parent/link tree, retained timestamps,
  adapter recreation or a second independent send into the same session.

The six tracing rulings and laws P1-P8 define acceptance. Passing a registry
or compile check cannot close these producer gaps.

The review also found backend-specific physical pool metric names in the
shared registry and instruments; FIG-4843 renamed them to the neutral
`lash.store.pool.*` contract this document links.
