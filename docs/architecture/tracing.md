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

An admission's export is an obligation of the durable admission that
retains its scope (FIG-5395): a turn's `turn.admit`, a round's `model.done`,
a cell's admission of its call. Its candidate is selected once that commit
lands, and deferred to the adapter when the commit's acknowledgement was
lost. An owner that dies or loses the acknowledgement first leaves the
export owed, and the next reader of the retained admission reconciles it
through `TraceScopeFactory::export_admitted`: the owner that starts a turn
still admitted, the owner that resumes a round whose exports are not
recorded, the owner that builds a cell call's body. The adapter dedupes the
admission's identity, its anchor, and a reconcile selects a deferred
candidate of that anchor. The SDK mints every span id, so an adapter that no
longer holds the candidate exports the admission as a span under the
anchor, which names its identity. Each admission is therefore exported once
per adapter, and at least once under one identity across owners.

An adapter's dedupe lives in its process, so an obligation is discharged
durably by the owner that exported it (FIG-5452). A round's owner records
the decision `round.traced` once its calls' candidates are selected, before
any body runs, and an owner that resumes the round without that record
reconciles the exports and then records it. A node that takes a round over
after its exports are recorded exports none of its calls' admissions again;
only a node lost between selecting them and recording so leaves them to be
exported twice. A turn's admission is discharged by its first phase commit.

Every other export is best effort. A crash after commit and before emission
can lose an observation. A body that exports before its result commits can
execute again and export a distinct attempt. There is no telemetry outbox.
Trace delivery does not decide control flow or billing.

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
durable (ADR 0132 §5), and nothing replays it. Its trace scope is
retained by that admission, so the call is admitted once however many
attempts or owners run it. Its start and completion are live observations
under that scope: each execution that reaches the call observes it once. No store receipt grants tool
transitions any more; the transition-class `lash.tool_intent.*` counters
have no first writer until the substrate's trace lane gives them one.

Process tool scopes retain their process parent.
Deferred `AwaitToolCompletions` uses the same durable wait request and resolution
receipts as other engine waits. The SQL commit wrapper owns the Live permit
for its physical budget validation and histogram observation.

Recorded model usage and provider responses remain result data. Lash has no
billing ledger, and tracing never resends a provider request.
