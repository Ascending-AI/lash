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
those observations. Arbitrary context metadata is off by default.

## Telemetry content

One host setting, `LashCoreBuilder::telemetry_content`, states whether built-in
telemetry carries content: prompts and model responses, rendered instructions
and tool contracts, tool arguments and results, executed code and its output,
raw provider and protocol-step payloads, and diagnostic or provider text. It
defaults to
`TelemetryContent::Omitted`. The runtime applies it to every record before a
`TraceSink` (JSONL, stderr, a tee, a host's own sink) or the adapter sees it,
so every path shares the choice.

| | `Omitted` (default) | `Captured` |
| --- | --- | --- |
| Record marker | `"content": "omitted"` | `"content": "captured"` |
| Identities, statuses, counts, hashes, durations, usage | kept | kept |
| Content fields | empty (`""`, `null`, `[]`); offered tools keep their names | original text within the trace limits, never scrubbed |
| Raw provider bodies and envelope diff values | absent, with the omission reason `content_policy` | present, or their size or parse reason |
| A failed or cancelled tool outcome | its class, code, source and origin | the whole failure record |
| Adapter span | `lash.content.omitted = true`, no `lash.payload.json` | `lash.payload.json`, cut at `OtelOptions::max_payload_bytes` |

Omitted request, response, instruction, tool and code payloads are not built:
the runtime does not clone, render or serialize them. The setting is consent
and nothing else. `TraceLevel` still
chooses which records exist, sampling stays with the host's provider, and
`TraceLimits` and `OtelOptions::max_payload_bytes` still bound what is
captured. The same records exist under either value.

The setting governs telemetry only. Durable requests and results, session
history, product observations (the process and language graph) and the
responses a host's own calls return keep their contracts.
`DirectLlmClient` is its own telemetry path with the same default
(`with_telemetry_content`).

### Plugin custom payloads and host filtering

The policy covers built-in telemetry. A `custom` payload is plugin-authored
(or host-authored) output: Lash does not inspect or classify its fields. A
plugin must honour the policy for content it puts in custom payloads. Its
factory reads `PluginSessionContext::telemetry_content()` before building
content, for example with `ctx.telemetry_content().capture(|| text.to_owned())`.
The accessor reads the receiving host's current `TraceRuntime` policy. This
deployment privacy setting is not recorded session configuration: reopening a
session under a host that turned content off must expose `Omitted`, even if
the session was created with capture on.

The host has the final say at its trace exporter. Wrap the export sink and
install the wrapper with `LashCoreBuilder::trace_sink(...)`. The wrapper can
drop custom records or clone and redact their payloads before forwarding to
the inner sink. For example, a host that exports no custom records can use:

```rust
use std::sync::Arc;
use lash::tracing::{TraceEvent, TraceRecord, TraceSink, TraceSinkError};

struct FilterCustom {
    inner: Arc<dyn TraceSink>,
}

impl TraceSink for FilterCustom {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        if matches!(&record.event, TraceEvent::Custom { .. }) {
            return Ok(());
        }
        self.inner.append(record)
    }

    fn flush(&self) -> Result<(), TraceSinkError> {
        self.inner.flush()
    }
}

// `builder` and `exporter` are the host's core builder and export sink.
let builder = builder.trace_sink(Arc::new(FilterCustom { inner: exporter }));
```

Place the wrapper before any sink fan-out whose exporters require this
filtering. It governs the sinks it wraps; a separately installed telemetry
adapter or sink retains its own export policy. Lash ships no custom-payload
filter or presentation policy.

The [logging and event practice](../agents/logging-and-events.md) defines diagnostic
fields, failure ownership, levels and correlation with domain observations.

The 1.0 export baseline includes conversation, durable run, process and model-call
identities as span attributes, even with payload export off (FIG-5529).
`gen_ai.conversation.id` and `lash.session.id` name the session. Scope ownership
supplies `lash.run.id` and `lash.process.id`; record context supplies
`lash.llm_call.id`. A physical turn's id does not imply its logical run's id.
The free host metadata in `TraceContext::run_id` exports separately as
`lash.context.run.id`. These identities do not enter default span names or
metric dimensions.

A failed code cell exports Error status and its closed execution reason, or
its cell failure kind when the executor returned a cell failure. A recovered
outer turn keeps its own successful status. Tool failures export the terminal
outcome's typed class, using the last attempt's class when the terminal payload
has no class. Turn failures export their stop reason, and failed waits and
timers their resolution/status tag as `error.type`. Tool receipts export their
closed denied or aborted terminal. Missing typed tool failure evidence exports
`unknown`. Model failures retain their provider code or normalized class and,
when present, `http.response.status_code`. Human-readable failure detail remains
subject to the payload policy.

## Required durability contract

Admission retains a typed cause, its selected SDK-created anchor and the
original start time beside business data. The first accepted write wins,
including an untraced admission. A retry under another context reads the
winner. Trace provenance does not change business submission hashes,
effect identities, tool-call identities or graph-node identities.

Awaited work uses a parent edge. Independently admitted work starts a trace
with links to its producers. The receiving scope keeps its parent when a
host completion resolves a wait or a checkpoint consumes another input. A current
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
on replay, without exporting another lifecycle observation. The
[process observation guide](../observing-processes.md) explains the durable
facts, node execution telemetry and host persistence needed to replay a path.

An admission's export is an obligation of the durable admission that
retains its scope (FIG-5395): a turn's `turn.admit`, a round's `model.done`,
a cell's admission of its call. Its candidate is selected once that commit
lands, and deferred to the adapter when the commit's acknowledgement was
lost. An owner that dies or loses the acknowledgement first leaves the
export owed, and the next reader of the retained admission reconciles it
through `TraceScopeFactory::export_admitted`: the owner that holds a turn
still admitted, a round or a cell's admission of its calls, whose exports
are not recorded. The adapter dedupes the
admission's identity, its anchor, and a reconcile selects a deferred
candidate of that anchor. The SDK mints every span id, so an adapter that no
longer holds the candidate exports the admission as a span under the
anchor, which names its identity. Each admission is therefore exported once
per adapter, and at least once under one identity across owners.

An adapter's dedupe lives in its process, so an obligation is discharged
durably by the owner that exported it (FIG-5452, FIG-5457):

- A turn's owner records `turn.traced` on the turn's phase row once its
  admission's candidate is selected, before the turn starts. An owner that
  finds the turn still admitted without that record reconciles the export
  and then records it.
- A round's owner records the decision `round.traced` once its calls'
  candidates are selected, before any body runs. An owner that resumes the
  round without that record reconciles the exports and then records it.
- A cell's owner exports its admission's calls (selecting the candidates
  it proposed, reconciling another owner's) and records the same
  `round.traced` decision on the admission's run before any of their
  bodies runs.

A node that takes a turn, a round or a cell over after its exports are
recorded exports none of those admissions again. Only a node lost between
the export and its record leaves an admission to be exported twice.

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
