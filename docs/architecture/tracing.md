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
Only an executed body emits a live attempt observation. Loading a committed
result or reading a retained terminal grants neither permission.
Product process and language graphs reconstruct through a separate observer
on replay, without exporting another lifecycle observation.

Export is best effort. A crash after commit and before emission can lose an
observation. A body that exports before its result commits can execute again
and export a distinct attempt. There is no telemetry outbox or exactly-once
export guarantee. Trace delivery does not decide control flow or billing.

## Integration status

Production admission proposes an SDK candidate and retains the selected
anchor before it dispatches children. Turn, tool, wait, run and process
terminals emit from committed first-writer receipts, with their retained
times. Model attempts report the provider's actual responses.
`tests::otel_laws::golden_tree_survives_replay_and_redrive` checks the
exported SDK tree across replay, redrive, adapter recreation and a second
send.

Core tracing names no plugin, durable substrate, store backend or LLM
provider. Events, scopes and instruments use the engine's typed vocabulary.
For example, a protocol plugin reports compile/link evidence as
`program_step`, and every backend uses the neutral `lash.store.pool.*`
instruments through its `StoreObserver`.

A tool call runs in memory inside the admitted execution that makes it
durable (ADR 0132 §5), and nothing replays it. Its start and completion
are live observations under the call's tool trace scope: each execution
that reaches the call observes it once. No store receipt grants tool
transitions any more; the transition-class `lash.tool_intent.*` counters
have no first writer until the substrate's trace lane gives them one.

Process tool scopes retain their process parent.
Deferred `AwaitToolCompletions` uses the same durable wait request and resolution
receipts as other engine waits. The SQL commit wrapper owns the Live permit
for its physical budget validation and histogram observation.

Recorded model usage and provider responses remain result data. Lash has no
billing ledger, and tracing never resends a provider request.
