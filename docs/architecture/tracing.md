# Durable tracing

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

Hosts install `lash::tracing::OtelTelemetry` through
`LashCoreBuilder::telemetry`, over a `LashTracerProvider` built with
`LashTracerProvider::new(builder, sampler, id_fallback)` and a meter
provider. The adapter uses OpenTelemetry SDK 0.33. Hosts own resources,
export, provider flush and shutdown; their sampler decides once per trace,
at the producer, and every lash span below follows that decision
(`RecordOnly` and `Drop` both leave a trace unsampled). Lash accepts one
adapter per core, and it is both a record sink and the runtime's span
identity. Each `trace_sink` call adds a record consumer: every sink sees
every record the adapter sees, with the same W3C position, and a sink that
fails or panics affects only itself. The shared runtime also supplies every
plugin with its clock, scope, emitter and metric instruments.

The [instrumentation contract](../../crates/lash/docs/instrumentation-contract.md)
lists the registered spans, attributes and metrics. Every trace record,
span and operational metric lash declares has a production producer; an
entry without one is deleted, not left registered (FIG-5658).

## Telemetry content

One host setting, `LashCoreBuilder::telemetry_content`, states whether built-in
telemetry carries content: prompts and model responses, rendered instructions
and tool contracts, tool arguments and results, executed code and its output,
raw provider and protocol-step payloads, and diagnostic or provider text. It
defaults to
`TelemetryContent::Omitted`. The runtime applies it to every record before a
`TraceSink` (a tee, a host's own sink) or the adapter sees it,
so every path shares the choice.

| | `Omitted` (default) | `Captured` |
| --- | --- | --- |
| Record marker | `"content": "omitted"` | `"content": "captured"` |
| Identities, statuses, counts, hashes, durations, usage | kept | kept |
| Content fields | empty (`""`, `null`, `[]`); offered tools keep their names | original text within the trace limits, never scrubbed |
| Raw provider bodies and envelope diff values | absent, with the omission reason `content_policy` | present, or their size or parse reason |
| A failed or cancelled tool outcome | its class, code, source and origin | the whole failure record |
| Adapter span | `lash.content.omitted = true`, no `lash.payload.json` | `lash.payload.json`, cut at 4 KiB |

Omitted request, response, instruction, tool and code payloads are not built:
the runtime does not clone, render or serialize them. The setting is consent
and nothing else. `TraceDetailPolicy` still
chooses which records exist, sampling stays with the host's provider, and
`ContentLimits` and `TraceLimits` still bound what is captured. The same records exist under either value.

The setting governs telemetry only. Durable requests and results, session
history, product observations (the process and language graph) and the
responses a host's own calls return keep their contracts.
`DirectLlmClient` is its own telemetry path with the same default
(`with_telemetry_content`).

### Extension spans and host filtering

The policy covers extension spans too. A plugin records through the
capability lash binds for the callback it runs (`ExtensionTrace`; see
[the plugin guide](../plugins.md#tracing-an-extension)): spans and points
whose names, attributes and events are bounded by `TraceLimits`, and whose
content values go through `ExtensionSpan::content(key, || value)`. A content
builder runs only when the `extension` category is captured, the selector
kept it and the span records, so a plugin never inspects the policy itself;
`ExtensionTrace::content_policy()` reads it for content a plugin builds
elsewhere. Each span is one `extension_span` record, emitted whole when it
ends, attributed to the registered `plugin_id` and `lash.plugin.callback`
of the callback that opened it, and positioned under the span that callback
ran in. There is no free-form plugin payload record.

The host has the final say at its trace exporter. Wrap the export sink and
install the wrapper with `LashCoreBuilder::trace_sink(...)`. The wrapper can
drop extension records, or clone and redact them, before forwarding to the
inner sink. For example, a host that exports no spans of one plugin can use:

```rust
use std::sync::Arc;
use lash::tracing::{TraceEvent, TraceRecord, TraceSink, TraceSinkError};

struct DropPlugin {
    plugin_id: &'static str,
    inner: Arc<dyn TraceSink>,
}

impl TraceSink for DropPlugin {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        if matches!(
            &record.event,
            TraceEvent::ExtensionSpan(span) if span.plugin_id == self.plugin_id
        ) {
            return Ok(());
        }
        self.inner.append(record)
    }

    fn flush(&self) -> Result<(), TraceSinkError> {
        self.inner.flush()
    }
}

// `builder` and `exporter` are the host's core builder and export sink.
let builder = builder.trace_sink(Arc::new(DropPlugin {
    plugin_id: "my_plugin",
    inner: exporter,
}));
```

A dropped extension span orphans the spans opened under it, so a filter
that drops one should drop its descendants, or skip the kinds instead:
`TraceExportPolicy` skipping `Extension` (with `Point`, `Model` and
`ModelAttempt`, which it can parent) keeps every record in the sinks and
exports none of them, and a `CaptureSelector` that skips the same kinds for
a call builds none of them. Place the wrapper before any sink fan-out whose
exporters require this filtering. It governs the sinks it wraps; a
separately installed telemetry adapter or sink retains its own export
policy.

### Writing records to a file

Lash ships no trace file or console writer. `TraceSink` is the seam: a host
that wants records on disk or on stderr writes its own sink, choosing the format, rotation and
durability, and installs it with `LashCoreBuilder::trace_sink(...)`. A JSON
Lines sink is a few lines; write each record and its newline in one call so a
concurrent writer cannot split a line:

```rust
use std::{fs::File, io::Write, path::PathBuf, sync::Mutex};
use lash::tracing::{TraceRecord, TraceSink, TraceSinkError};

struct JsonlFile { path: PathBuf, file: Mutex<File> }

impl TraceSink for JsonlFile {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let mut file = self.file.lock().unwrap_or_else(|poison| poison.into_inner());
        file.write_all(line.as_bytes())
            .map_err(|source| TraceSinkError::Write { path: self.path.clone(), source })
    }
}
```

Override `TraceSink::flush` to `fsync` when the host needs records on disk
before exit. Tests read typed records from `lash::testing::RecordingTraceSink`.

The [logging and event practice](../agents/logging-and-events.md) defines diagnostic
fields, failure ownership, levels and correlation with domain observations.

The 1.0 export baseline includes conversation, durable run, process and model-call
identities as span attributes, even with payload export off (FIG-5529).
`gen_ai.conversation.id` and `lash.session.id` name the session. Scope ownership
supplies `lash.run.id` and `lash.process.id`; record context supplies
`lash.llm_call.id`, the id a provider decorator also reads from its request
scope. A physical turn's id does not imply its logical run's id. These
identities do not enter default span names or metric dimensions.

A code cell exports as `execute_tool {language}` (`gen_ai.tool.type =
function`, `lash.cell.id` its graph key, `lash.cell.language`,
`lash.cell.tool_calls` once it answered and `lash.outcome`). When a model
reply has text after its first complete cell, `lash.cell.trailing_discarded`
reports the discarded byte count even with content capture disabled. The
turn's RLM extraction diagnostic records the typed `discarded_after_cell`
note (`bytes`, `reason: predicted_continuation`); the tail never enters the
next model request. It is its turn's
child and the parent of the tool calls it makes, and it spans from its first
start, which the `Tools` phase that holds the cell retains, to the end of
the execution that answered it, across suspensions and re-runs. A failed
code cell exports Error status and its closed execution reason, or its cell
failure kind when the executor returned a cell failure; a cell its turn's
terminal discards exports `lash.outcome = discarded` with no error. A recovered
outer turn keeps its own successful status. Tool failures export the terminal
outcome's typed class, using the last attempt's class when the terminal payload
has no class. Turn failures export their stop reason as `error.type`.
Missing typed tool failure evidence exports `unknown`. Model failures retain
their provider code or normalized class. Human-readable failure detail remains
subject to the payload policy.

## Required durability contract

Admission retains a typed cause and the original start time beside
business data. The scope's span is derived from that cause and the scope's
durable identity (`derive_span`), so every owner names the same span. The first accepted write wins,
including an untraced admission. A retry under another context reads the
winner. Trace provenance does not change business submission hashes,
effect identities, tool-call identities or graph-node identities.

Every admission has a parent, or it is unobserved: no admission makes a
root. A host call that admits work opens a producer span first (`lash.send`,
`lash.send.batch`, `lash.process.start`, `lash.tool_intent.submit` or
`lash.plugin.invoke`), as the caller's child or, with
`TraceLinkMode::Link`, as a new root that links the caller, and the work it
admits retains that producer as its parent (FIG-6298). A turn joins its head
input's producer and links the other inputs of its batch. A child-session
turn joins its process, and a follow-on turn joins the turn that
switched to it. An input a running turn consumes at a checkpoint is linked
from the turn's completion and never becomes a second parent; the turn's
checkpoint retains the delivered input's cause, so an owner that resumes
the turn links it too. The receiving scope keeps its parent when a host
completion resolves a wait. A current execution attempt is an optional
link, supplied through the engine interface; it never replaces the retained
parent.

Lash mints every span id. A container (a turn, a tool call, a process, a
tool intent) takes its derived span; a leaf (a model call) takes a random id
from the identity's id generator when it opens; a producer takes ids from
the same generator and the host sampler's decision. Every record carries
its `TracePosition`: its span, parent, links, role and kind. A container's
children name it as their parent from its start, and the container is built
once, from its closing record, at its retained start and end. No SDK span or
event buffer stays open across suspension.

Only a newly committed lifecycle transition emits a logical completion.
Only an executed body emits a live attempt observation. Loading a committed
result or reading a retained terminal grants neither permission.
Product process and language graphs reconstruct through a separate observer
on replay, without exporting another lifecycle observation. The
[process observation guide](../observing-processes.md) explains the durable
facts, node execution telemetry and host persistence needed to replay a path.

A container closes exactly once: on the owner whose acknowledged commit
wrote its terminal, after that acknowledgement, from retained times (P1).
A turn closes on the owner of `turn.commit`, a cancel, a session close's
cancel or a refusal end. A tool call closes on the owner whose commit
retained the member's outcome, at the `ended_at_ms` that commit retained:
an owner that takes a round over closes only the members whose outcomes
its own commits write. A member a turn's terminal settles `Cancelled` (a
cancel or a refusal of a turn whose round no live owner settled) closes
on that terminal's owner, `cancelled`; one a session close's cancel leaves
with no outcome closes there, `discarded`. A code cell closes on the owner
whose commit consumes its answer (the next model call's start, the next
step's `model.done` or the turn's commit); a cell its turn's terminal stops
before it answered closes on that terminal's owner, `discarded`, with the
calls it admitted that have no outcome. A cell that suspends closes nothing
until an activation answers it. A process closes on the owner of
`process.terminal`, and a tool intent on the submission that first retains
its settlement. A successor that folds a committed terminal, a resend, a
redrive and a reader of a retained terminal close nothing. Tracing adds no
commit: there is no admission export to record.

### Publication, acceptance and delivery

Container publication is at most once, and the owner whose acknowledged
commit wrote the container's terminal owns it (P1). It is never repeated
and never deduplicated: an at-least-once export would retain an obligation
in the terminal commit and discharge it with one more commit per
container, and an owner that died between publishing and discharging would
export a second span with the same id, a second duration and a second
metric sample. Lash keeps loss rather than duplication, and tracing adds no
commit.

- **Publication** is the owner's one call into the emitter for the closing
  record. The engine's close path has no `.await` between the commit's
  acknowledgement and that call. Publication still runs host code
  synchronously: every sink's `append`, the flight recorder's snapshot
  callback, and under `OtelTelemetry` the host's `SpanEnricher` and
  the SDK's span processors. Host code has no wall-clock bound, so
  publication has none either.
- **Sink acceptance** is per sink. Each `append` runs under its own panic
  containment and counts its own failures, so a record one sink accepted
  may be missing from another.
- **Delivery** past a sink (an SDK batch processor, an exporter, a
  backend) is the host's pipeline. Lash observes none of it.

A container is lost in three cases:
- its owner loses the terminal commit's acknowledgement;
- its owner dies after the commit and before or during publication,
  including while a host callback runs;
- one consumer fails after publication: its own `append` fails or panics,
  or the host pipeline drops the span. Each consumer fails independently.

In the first two cases the next owner finds the terminal written and
closes nothing. The container's published children keep a parent id that no span carries, and
show as an orphan subtree. A crash after a commit and before an emission
can lose an observation in the same way. There is no telemetry outbox, and
trace delivery does not decide control flow or billing. A body that exports
before its result commits can execute again, and then exports a distinct
attempt.

Host callbacks (sinks, the flight recorder's callback and the enricher)
run with no engine lock held. They must not block on the engine or call
back into the session or core that called them: no `send`, no admin call
and no store read. A host that needs I/O hands the record to
its own bounded queue.

The tool crash laws (`crates/lash-durable-test/tests/tool_crash_laws.rs`,
`publication_loss.rs`) measure the window. They cut a traced turn at its
commits under every fault, resume it on the other node, and report per
terminal label what each cut lost (L1(f)). Only a cut that lands a terminal
commit and then loses its owner's acknowledgement (`ack-hidden`) or its
life (`commit-then-abort`) loses anything, and it loses only the container
that commit closes. A cut that fails before the commit, aborts before it,
or pauses the owner as a zombie loses nothing: the successor writes the
terminal and publishes. Measured on SQLite:

| Terminal label | Lossy cut | Lost | Orphaned published spans |
| --- | --- | --- | --- |
| `turn.commit` | `ack-hidden`, `commit-then-abort` | `invoke_agent` | the turn's two model calls and its tool call, and in the cell turn its cell too |
| `round.outcome+present+model.start` (a native call) | `ack-hidden`, `commit-then-abort` | the call's `execute_tool` | none |
| `round.outcome` (a cell's call) | `ack-hidden`, `commit-then-abort` | the call's `execute_tool` | none |

A code cell closes on the commit that consumes its answer (the next
`model.start`, `model.done` or `turn.commit`), so the matrix lets a lossy cut
of that commit lose the cell's `execute_tool {language}` span, orphaning the
calls the cell made, and nothing else; a cell that re-runs after any other
cut publishes its span once. `process.terminal` (a process) and a tool intent's
settlement follow the same rule, but this matrix does not cut them.

## Tracing policies

Every tracing knob is a sealed policy with a documented default and range.
A value outside its range is refused at build with
`TracePolicyError::Range { field, value, min, max }`; a value that breaks a
documented relation with another with `TracePolicyError::Relation`, naming
both operands and both values; a selection the model forbids with
`TracePolicyError::Structural`. A core reports what is in effect through
`LashCore::trace_policy()`.

- **What lash builds.** `TraceDetailPolicy` (`LashCoreBuilder::trace_detail`)
  sets one `TraceDetail` per `TraceSpanKind`. A kind that is `Off` is never
  built: its records are not constructed, for any sink. Containers cannot be
  off, and only model calls (provider and stream event records) and
  workflows (node spans) have `Extended` detail. A workflow's choice is
  retained on the process at registration (`trace_nodes`), so every owner
  that runs it makes the same one.
- **What the exporter exports.** `TraceExportPolicy`, on `OtelOptions`,
  skips kinds from the OpenTelemetry export only: their records still reach
  every sink. `TraceMetrics` chooses the runtime and GenAI metric families.
- **The off rule.** No kind is off or skipped while a kind in its
  `TraceSpanKind::children()` is on, so no exported span names a parent that
  was not exported. `extension_spans = 0` turns extension and point spans
  off, and so requires model calls and their attempts off.
- **Bounds.** `TraceLimits` holds the seam's admission and buffering
  bounds (held records, links, extension spans, attributes, events, bytes,
  references, the publication hold). `ContentLimits` bounds captured
  provider bodies. Execution evidence, diagnostics and composition memory
  are not telemetry and have their own policies: `EvidenceLimits`,
  `DiagnosticLimits`, `CompositionCacheLimits`. `TraceStateLimits` and
  `FlightRecorderSettings` are sealed the same way.

### The bounded writer and omissions

Every host or plugin contribution to a span passes one writer. It charges
one attribute and `key.len() + encoded_len(value)` bytes before it buffers,
against the span's one budget: `attributes_per_span` less the registry's
`attribute_reserve(kind)` for lash's own attributes, and
`span_contribution_bytes`. Lash's attributes always fit, so an SDK whose
`max_attributes_per_span` is at least a kind's reserve drops contributions
first and never a lash attribute. A refusal is a typed `TraceOmission`
(`Attributes`, `SpanBytes`, `ValueTooLarge`, `ReservedKey`, `UseReplace`,
...): the writer returns it, the span carries `lash.trace.omitted` and
`lash.trace.omitted.count`, and `LashCore::trace_omissions()` counts it by
site kind and reason. A string over `attribute_value_bytes` is cut at a
UTF-8 boundary; a JSON value over it is refused whole.

### The enricher

`OtelOptions::enricher` installs one `SpanEnricher`. Lash calls it once for
every span it exports, before the SDK builds the span, with a `SpanView`
holding the span's kind, stable ids (`TraceIds`, the same on every owner),
position, record, lash's attributes and the policies in effect. A
producer's enricher runs before the host sampler decides, so the sampler
sees what it derived; the sampler's result attributes are written through
the same writer and exported on the producer span only. Neither is ever
persisted. `SpanEnrichment::set` adds host keys (never `lash.` keys or keys
the registry declares), `rename` renames the span, and `replace` replaces
a registered semantic default such as `gen_ai.provider.name`
(`RefinableKey`); metric labels keep lash's values. A panicking enricher
leaves the span with lash's attributes, counted as `EnricherFailed`.

A `lash.model.attempt` span's view shares the logical call's closing record
as `record`; `SpanView::attempt` names the attempt it decorates (`None` on
every other span), and `SpanView::usage()` is the summed usage lash
exported as `gen_ai.usage.*` on the logical `chat` span, from the same
fold. Enrichers use `attempt` and `usage()` instead of re-deriving either
from `record`.

## Integration status

Production admission retains its scope, and with it its span, before it
dispatches children. Model attempts report the provider's actual responses. Each logical record is emitted by the owner whose commit
was acknowledged, which is the record's first-writer receipt:

| Record | Span or metric | Producer | Time |
| --- | --- | --- | --- |
| `turn_started` | opens `invoke_agent` (a live hint; the span is built from its close) | the session actor, after `turn.admit` | the scope's retained start |
| `turn_completed` | `invoke_agent` | the session actor, after the commit that writes the turn's terminal (`turn.commit`, a refusal, a cancel, a session close) | the owner's clock at the acknowledgement |
| `domain_completed` (process) | `lash.process {kind}` or `invoke_workflow {name}` | the process actor, after `process.terminal` | the terminal's retained `occurred_at_ms` |
| `domain_completed` (tool intent) | `lash.tool_intent`, `lash.tool_intent.executed`, `lash.tool_intent.refused` | the host ingress, after the ledger retains the settlement | the settlement's retained time |
| none | `lash.parked_work.parks` | the session or process actor, after the commit that parks it | not timed |

An owner that dies between its commit and the emission loses that record:
there is no telemetry outbox. The next owner finds the terminal already
written and emits nothing, so a resend, a redrive or a takeover never
exports a second logical completion. Every cause that ends an admitted
turn, an operator's cancel and a fork included, writes its `turn_completed`;
a deleted session's read answer and a command run end no admitted turn.

Lash emits no wait, timer, tool-receipt or journaled-effect record, and no
run or process-segment span. A wait is not the unit an operator acts on:
the tool call or process that waited carries the outcome, and a parked
actor is counted by `lash.parked_work.parks`. A host reads how many actors
are parked, and for how long, from the parked-work reads of `lash::admin`,
not from a gauge.

Core tracing names no plugin, durable substrate, store backend or LLM
provider. Events, scopes and instruments use the engine's typed vocabulary.
For example, a protocol plugin reports compile/link evidence as
`program_step`, and every backend uses the neutral `lash.store.pool.*`
instruments through its `StoreObserver`.

A tool call runs in memory inside the admitted execution that makes it
durable (ADR 0132 §5), and nothing replays it. Its trace scope is
retained by that admission, so the call is admitted once however many
attempts or owners run it. Its start and completion are live observations
under that scope: each execution that reaches the call observes its start,
and its `execute_tool` span closes once, on the owner whose commit retained
its outcome.

A host-submitted tool intent retains its scope on its ledger row, under
its submission's producer. The submission
that first retains the settlement exports `lash.tool_intent` and counts the
outcome; a redelivery of the key finds the settlement and exports nothing.

Process tool scopes retain their process parent. The SQL commit wrapper
owns the Live permit for its physical budget validation and histogram
observation.

Recorded model usage and provider responses remain result data. Lash has no
billing ledger, and tracing never resends a provider request.

## GenAI projection contract (FIG-6301)

The default projection follows
`open-telemetry/semantic-conventions-genai@07794021bfe46e1c241408b6f55cb4354bd28318`.
A turn is `invoke_agent`; each logical model operation is one `chat {model}`,
with `lash.model.attempt` children published only from its closing ledger.
The detail and export policies independently select those children. Attempt
spans carry diagnostic usage under `lash.usage.*`; only the logical chat carries
`gen_ai.usage.*`, summed over every attempt that reported usage. Input tokens
include fresh, cache-read and cache-write tokens. A sum that overflows exports
no `gen_ai.usage.*` counter; an input total that overflows omits
`gen_ai.usage.input_tokens`; either sets `lash.usage.overflow`. Reasoning tokens use
`gen_ai.usage.reasoning.output_tokens` and remain a subset of output tokens. A
call whose usage never arrived, because the attempt that ended it was cut short
after a response was observed, sets `lash.usage.unknown`: its missing
`gen_ai.usage.*` counters are unknown, not zero (ADR 0031).

Adapter defaults are `openai` for `openai`, `openai-compatible` and `codex`,
`anthropic` for `anthropic`, and `gcp.gen_ai` for the `google_oauth` adapter's
Code Assist endpoint. The enricher can replace `RefinableKey::GenAiProviderName`; runtime
retry labels keep the semantic provider default. Successful provider evidence
supplies the response model, response id and finish reasons. Failed calls keep
the request facts and billed usage as well.

Process engines explicitly declare `ProcessTraceClass`; the VM
engine declares `Workflow`. The first registration records that choice, and
every terminal reads it, including cancellation on a node without the engine.
Workflows use the recorded label (the VM's definition entry name); an unavailable
name is omitted. Internal processes have no model attributes or GenAI usage.

Hosts that store this: `ProcessRegistration` and `ProcessRecord` gain required
`trace_class: "internal" | "workflow"` beside the retained execution facts.
Trace-record schema 37 changes in place to add operational model facts,
trace-owned attempt metadata, typed process/workflow subjects and the turn's
admitted-input count;
it removes model/provider/usage fields from domain completions. The stored
execution attempt ledger keeps its existing shape. The generated
[instrumentation contract](../../crates/lash/docs/instrumentation-contract.md)
lists the exact exported attributes and span type ids.

## GenAI metrics (FIG-6306)

Installing `OtelTelemetry` records both families by default.
`OtelOptions::metrics` selects `Off`, `Runtime`, `GenAi` or `All`; each disabled
family uses no-op instruments. The generated instrumentation contract lists
the names, types and units. Hosts own histogram views, cardinality limits and
exporters through their meter provider.

GenAI durations, token counters and per-call token histograms come from the
same closing records as their spans. They retain the projection's default
labels, before host enrichment, and include no session, turn or call ids.
Sampling and span export selection do not suppress metrics. Removing a leaf's
record through detail selection also removes its metrics. First-chunk timing
observes streamed output or usage, excluding request evidence and retry notices.
`lash.trace.omissions{reason}` belongs to `Runtime` and records at the omission
ledger, including omissions with no published record.

The agent's closing record carries its admitted inference and tool counts,
including failed calls. Tool admissions update their counter with the admission
facts, so replay and takeover do not count a call again. The live owner's close
uses its loaded row plus acknowledged appends; cold terminals use the stored
row. A refusal reloads that row once on its exceptional path.

Hosts that store this: the baseline `turn_phases` table gains nonnegative
`tool_calls`, default zero (`INTEGER` on SQLite, `BIGINT` on PostgreSQL), beside
`model_calls`. The canonical admission writer stages `TurnWrite::AddToolCalls`
with each new turn or cell round. A cell belonging to a session operation has
no turn row to update. Trace-record schema 37 adds optional `inference_calls`
and `tool_calls` to `TurnCompleted` in place, without a version bump.
