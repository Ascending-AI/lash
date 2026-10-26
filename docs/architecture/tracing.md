# Durable tracing

Lash emits one `TraceRecord` model through one `TraceSink` seam. Every sink,
including `OtelTelemetry`, receives the same records, W3C positions and stable
ids. Sinks are additive: each `LashCoreBuilder::trace_sink` installs another
consumer. `LashCoreBuilder::trace_product_observer` separately installs a
product observer that also sees reconstruction, using the core's clock and
runtime. `LashCoreBuilder::telemetry` installs the OTel adapter as the identity,
ambient context, metrics and one sink together. A sink-only host still gets
lash-minted W3C ids through `LashIdentity`.

The [instrumentation contract](../../crates/lash/docs/instrumentation-contract.md)
is generated from the span, attribute and metric registry. The
[Configuration](#configuration) reference below is generated from policy rustdoc
and the numeric field registry. [What survives a crash](../../README.md#what-survives-a-crash)
is the execution durability contract.

## The host seam

Lash supplies mechanism and identity; hosts supply meaning and policy. Lash
carries no user, tenant, experiment or other host identity. Hosts map
`TraceIds` to their own data in an enricher, selector or sink. Resource,
sampling policy, processors, exporters, metric views, histogram buckets,
cardinality, flush and shutdown belong to the host.

Lash owns no file format or output destination. A host writes its own
[record sink](#writing-records-to-a-file). Nothing persisted for tracing contains
host-supplied content beyond what execution needs: retained causes, scope ids,
start/end times and the workflow's structural `trace_nodes` choice are lash
facts. Enrichment, tool annotations, sampler attributes, hook references and
selector results stay in memory.

Public boundaries use lash-owned types: `TraceCarrier`, `TracePosition`,
`TraceTimestamp`, `TraceIds`, `TraceValue`, `ContentPolicy` and `ExtensionTrace`.
There are exactly two versioned contract exceptions:

- The OTel API and SDK at the installer, enricher and explicit OTel context
  accessor, behind `otel-trace`. Lash pins `opentelemetry` 0.33 and
  `opentelemetry_sdk` 0.33; the in-tree OTLP exporter is `opentelemetry-otlp`
  0.33. Hosts must align those crates and their instrumentation bridges with
  lash's versions. Changing the declared OTel API/SDK version is a breaking
  facade change. An OTel upgrade is announced in its commit body under
  **OTel version**, including what hosts must align.
- JSON is `serde_json::Value` 1.x, also spelled `lash::JsonValue`. It is the
  facade's shared JSON contract, including `ContentValue::Json`,
  `TraceValue::Json` and record fields. A serde_json major change breaks the
  facade contract.

Lash does not install a global provider or diagnostic subscriber. Rust
`tracing` diagnostics remain the host's separate logging channel; see
[logging and event practice](../agents/logging-and-events.md).

## Installing OpenTelemetry and sampling

Enable the facade's `otel-trace` feature. The host builds a provider through
`LashTracerProvider`; `OtelTelemetry::new` requires that wrapper by type.

```rust
use lash::tracing::{LashTracerProvider, OtelOptions, OtelTelemetry};
use opentelemetry_sdk::trace::{RandomIdGenerator, Sampler, SdkTracerProvider};

// Configure the host's resource, processors, exporters and span limits here.
let provider = LashTracerProvider::new(
    SdkTracerProvider::builder(),
    Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
    RandomIdGenerator::default(), // id_fallback
);
// `meter` and `builder` are the host's meter provider and LashCoreBuilder.
let builder = builder.telemetry(OtelTelemetry::new(
    &provider, &meter, OtelOptions::standard(),
));
// The host retains `provider` for force_flush() and shutdown().
```

Both `sampler` and `id_fallback` are required; lash chooses neither. The example
copies the SDK's defaults. They replace any sampler or id generator previously
set on the SDK builder. `id_fallback` supplies producer, leaf and non-lash span
ids. Private wrappers preset lash's ids and decisions for each synchronous SDK
span start, consuming a preset before a processor can open another span.

**Sampler limits (D7):** the host sampler decides at the producer, after
producer enrichment, once per trace. It is never consulted for lash spans
below that producer. `RecordOnly` and `Drop` both become unsampled: no lash SDK
span is built, and processors never see a recording-but-unsampled lash span.
The sampler cannot select individual lash spans by name or attribute. Use
`TraceExportPolicy` to select optional kinds, or a host `SpanProcessor` for
further filtering. Non-lash spans still use the host sampler normally.

The decision and tracestate travel in every retained carrier, so a resumed
owner follows the original decision even with a different sampler. Sinks still
receive unsampled records and decide for themselves. The carrier handed to a
sink, child, extension body or MCP server has the same trace id, span id,
sampled flag and tracestate as the exported span. Metrics remain independent
of sampling and export selection; removing a record through detail selection
also removes that record's metric sample.

## Identity and causality

`TraceRecord.context.ids()` exposes `TraceIds`: session, turn, send, batch,
process, tool intent, call, cell, workflow node and model call where known.
They are lash identities, stable across owners. Producer enrichment sees ids
known at open; acceptance adds the send id only to its settling record.
`LlmRequestScope.llm_call_id` joins a provider decorator to the logical `chat`;
`LlmRequestScope.trace_carrier` is that chat's carrier when built. Request
digests and business identities do not cover trace provenance.

A scoped record's `TracePosition` carries its span, parent, links, kind, role,
real start/end, scope and optional substrate-attempt link. `Opens` and `Event`
are live hints, which do not promise a close. `Closes` and `Whole` publish a
span; the adapter starts and ends the SDK span synchronously from that record's
retained times. No SDK span stays open across an await, suspension or takeover.
Unobserved admissions and unscoped diagnostics have no position. Installing a
sink later does not invent provenance for an unobserved admission.

| Entry | Producer | Admitted work's parent |
|---|---|---|
| Send | `lash.send` | that producer |
| Batch send | `lash.send.batch` | the head input's producer; other inputs are links |
| Host process start | `lash.process.start` | that producer |
| Tool-intent submission | `lash.tool_intent.submit` | that producer |
| Admin plugin operation | `lash.plugin.invoke` | that producer |
| Standalone direct completion | `lash.direct` | that producer |

A producer captures the explicit `trace_context(carrier)`, otherwise the
ambient caller, before its first await. `trace_context_none()` detaches from
the ambient caller. `trace_link_mode(TraceLinkMode::Parent)` is the default;
`Link` opens a new root linking the caller. Admitted work still parents to the
producer. Settlement publishes one `Whole` producer record (`selected`,
`reused`, `refused` or `abandoned`); a retry that reused an earlier acceptance
has its own producer with no newly admitted child.

A turn parents to the head admitted input's producer. Other admitted inputs
and inputs delivered at its checkpoints are deduplicated links, in that order,
under `TraceLimits::links`; delivery retains their causes for the closer on
any owner. A consumed input never changes a turn's fixed parent. A follow-on
turn parents to the switching turn; a child-session turn to its process.
Each continuation can therefore add another agent level. Resolving a wait or
timer keeps the receiving scope's parent. A substrate attempt is a link.

Containers derive ids with `derive_span(parent, key)` from retained identities:
`TraceScopeId` for turns, calls, processes and intents; graph key for cells;
the opaque `EffectId` for workflow occurrences. Loops, recursion and
concurrent occurrences stay distinct. Leaves (model calls and attempts,
extensions and points) draw random ids at open; every real re-execution is a
new leaf. Restoring a scope reads its retained position without minting one.

## Closure and publication

Only the owner whose acknowledged commit wrote a container's terminal closes
it, after that acknowledgement, from retained times. A retained read, resend,
redrive or successor folding an already committed result emits no close.
Tracing adds no commit and creates no export obligation.

| Container | Anchored by | Own-terminal closer | Parent-terminal closer | End time |
|---|---|---|---|---|
| Turn, `invoke_agent` | `turn.admit` | owner of the committed run terminal: completion, cancellation, unserved end or refusal | session close's cancellation of that turn | retained terminal `at_ms` |
| Tool, `execute_tool` | round or process-step admission retaining its scope | owner committing its member outcome (`round.outcome`, fused form, `step.outcome`, or a no-body settlement) | terminal owner settles unfinished members cancelled or discarded according to its terminal facts | outcome `ended_at_ms`, or terminal time for discard |
| Cell, `execute_tool {language}` | first `model.done…` or `cell.snapshot…` retaining its first start | owner of the commit consuming its answer | turn terminal owner discards the stopped cell and its calls with no outcome | answering execution's end, or terminal time |
| Process, `lash.process` / `invoke_workflow` | `process.register` | owner of `process.terminal` | each process has its own terminal | retained `occurred_at_ms` |
| Workflow node, `lash.workflow.node {site}` | occurrence's `step.start` or starting `process.advance` | owner committing the occurrence's completed, failed or cancelled fact | process terminal owner cancels open occurrences | fact's commit time |
| Tool intent, `lash.tool_intent` | ingress ledger row | submission first retaining settlement | — | retained settlement time |

Every cause that ends an admitted turn, including an operator cancel and a
fork, emits `turn_completed`. A deleted-session read response and a command
run end no admitted turn. A tool call runs in memory within the execution that
makes it durable. Its scope stays with that admission: each execution that
reaches the call observes its start, and `execute_tool` closes once on the
owner whose commit retained its outcome.

Anchored containers can publish children before their own close because their
terminal path owns the eventual close. An execution-local parent (model call,
extension span, or cell before anchoring) holds children until it publishes,
parent first. A crash discards unpublished holds. A cell's tools are admitted
only by `cell.snapshot+admit`, after the cell is anchored.

All holds of one body execution share one `publication_hold` record/byte
account, including serialized records and fixed entry overhead. Pressure
finalizes the gating extension ancestors outermost first, then the oldest
remaining parents as necessary, with `lash.span.ended_by = hold_pressure`.
It releases every billed model close; later writes to finalized handles count
`Late`. Parents can end before children. An unanchored cell cannot finalize
early: over its cap it sheds the arriving live hint or an entire extension
subtree, counting `PublicationHold`. It never drops a lash closing record.
Its first anchoring commit releases the kept hold in order. Per body memory is
bounded by `extension_bytes + publication_hold.bytes`.

| Situation | Observation |
|---|---|
| Journal answers a reconstructed step | none; speculative shift observations are dropped |
| Read of a retained scope or terminal | none; no emission permit |
| A body's owner dies before committing its outcome; another runs it | one tool close from the committing owner; distinct extension/model leaves per actual execution |
| Successor takes over a partially settled round | closes only members its own acknowledged commits settle |
| Cell re-executes before its answer is consumed | one cell close with the first retained start; ledger-served tools emit nothing new |
| A admits and B completes a turn | B closes the same derived `invoke_agent` |
| Start hint is lost | close still builds the full span |
| Terminal commit lands but acknowledgement or publishing owner is lost | that container can be missing; published children become an orphan subtree |

### Publication, acceptance and delivery

Container publication is **at most once** (D4), and the owner whose
acknowledged commit wrote the terminal owns the one call into the emitter. It
is never repeated or deduplicated: at-least-once export would retain an
obligation in the terminal commit and discharge it with another commit per
container. An owner that died between publishing and discharging that
obligation could export a second span with the same id, duration and metric
sample. Lash keeps loss rather than duplication, and tracing adds no commit.

The close path has no `.await` between the terminal commit's acknowledgement
and publication. Publication still runs host code synchronously: every sink's
`append`, the flight recorder's snapshot callback, and, under `OtelTelemetry`,
the host's `SpanEnricher` and the SDK's span processors. Host code has no
wall-clock bound, so publication has none either.

- **Sink acceptance** is per sink. Each `append` runs under its own panic
  containment and counts its own failures, so one sink may accept a record
  another sink misses.
- **Delivery** past a sink (an SDK batch processor, exporter or backend) is the
  host's pipeline. Lash observes no delivery acknowledgement.

A container can be lost in three cases:
- its owner loses the terminal commit's acknowledgement;
- its owner dies after the commit and before or during publication, including
  while a host callback runs;
- a consumer fails after publication: its `append` fails or panics, or the
  host pipeline drops the span. Each consumer fails independently.

In the first two cases, the next owner finds the terminal written and closes
nothing. Published children keep a parent id that no span carries and appear
as an orphan subtree. A crash after a commit and before emission can lose an
observation the same way. There is no telemetry outbox, and trace delivery
does not decide control flow or billing. A body that exports before its result
commits can execute again and then export a distinct attempt.

### Measured crash matrix

The tool crash laws (`crates/lash-durable-test/tests/tool_crash_laws.rs` and
`publication_loss.rs`) cut a traced turn at its commits under every fault,
resume it on another node, and report per terminal label what each cut lost
(L1(f)). Only a cut that lands a terminal commit and then loses its owner's
acknowledgement (`ack-hidden`) or life (`commit-then-abort`) loses anything,
and it loses only the container that commit closes. A cut that fails before
the commit, aborts before it, or pauses the owner as a zombie loses nothing:
the successor writes the terminal and publishes. Measured on SQLite:

| Terminal label | Lossy cut | Lost | Orphaned published spans |
| --- | --- | --- | --- |
| `turn.commit` | `ack-hidden`, `commit-then-abort` | `invoke_agent` | the turn's two model calls and its tool call, and in the cell turn its cell too |
| `round.outcome+present+model.start` (a native call) | `ack-hidden`, `commit-then-abort` | the call's `execute_tool` | none |
| `round.outcome` (a cell's call) | `ack-hidden`, `commit-then-abort` | the call's `execute_tool` | none |

A cell span still publishes from the execution that ran it, so no cut in this
matrix loses it. Once a cell closes on the commit that consumes its answer,
that commit joins the table. `process.terminal` (a process) and a tool intent's
settlement follow the same rule, but this matrix does not cut them.

Every host callback runs outside engine locks. The capture selector, content
hook, sink, enricher and flight-recorder callback must not block on or re-enter
the calling engine: no `send`, admin call or store read. Hand I/O work to a
host-owned bounded queue. The rules apply individually to each callback below.

## Content and selection

`LashCoreBuilder::content_policy(ContentPolicy)` sets the node's capture ceiling.
Every category defaults to `Omitted`. Identity, typed status, failure classes,
counts, hashes, sizes, durations, usage, model and tool names remain available.
The policy governs telemetry only; durable requests/results, session history,
product observations and returned host answers keep their execution contracts.
`DirectLlmClient::with_content_policy` follows the same default.

| Category | Content |
|---|---|
| `SystemInstructions` | rendered instructions and system prompts |
| `InputMessages` | history messages, nested tool-call/results, protocol history |
| `OutputMessages` | responses, stream text/tool parts, settled assistant answer on agent close |
| `Reasoning` | reasoning parts, also requiring their input/output direction's consent |
| `ToolDefinitions` | descriptions and schemas; tool names are structural |
| `OutputSchema` | request output schema; output type/name remain structural |
| `ToolArguments`, `ToolResults` | standalone tool args/results and failure text |
| `Code`, `CodeOutput` | cell source and output/error text |
| `ProviderPayloads` | raw provider bodies and envelope-diff values |
| `ResponseMetadata` | allowlisted response metadata, a separate record-sink category |
| `DiagnosticsText` | provider/program/store diagnostic prose |
| `Extension` | values passed through extension `content()` |

Nested tool parts follow their message's direction, rather than the standalone
tool categories. Reasoning needs both the direction's and `Reasoning` consent;
encrypted/redacted reasoning is absent. `ResponseMetadata` is not exported by
the OTel exporter. It matters to record sinks; a host enricher may explicitly
read it to add its own attributes. ADR 0038's operational allowlist and telemetry
consent are separate gates.

**Openers carry no content.** Since FIG-6302, `LlmCallStarted` always carries an
omitted request. Hosts read request content from `LlmCallCompleted` or
`LlmCallFailed`, and output from closers. Content-bearing changes follow the
[Hosts that read this](../agents/logging-and-events.md#hosts-that-read-this) rule.

`CaptureSelector::select` runs before construction with `SelectorView.ids`, kind
and policy. Install one with `capture_selector`; compose host policies yourself.
It can omit categories and skip optional leaves only. Skip children first:
`ModelAttempt` before `Model`, and `Point`, `ModelAttempt`, `Model` before
`Extension`. A container cannot be skipped. Narrowing is inherited beneath
leaves, even through skipped no-op handles; children cannot widen it. A record
directly beneath a container starts at the core ceiling. Selector results are
never stored; resumed owners call it with the same ids. A panic fails closed
for content, preserves detail and counts `SelectorFailed`. This synchronous
callback must not block, do I/O or re-enter the engine.

Policy-aware builders use `policy.capture(category, || build())`, so an omitted
category is never cloned, rendered or serialized. `TraceDetailPolicy` selects
what records are built for every consumer; `TraceExportPolicy` selects which
records become OTel spans. Containers cannot be off. An optional kind cannot
be off while a kind it can parent stays on: models with attempts; extensions
with points, models and attempts. `extension_spans = 0` requires model and
attempt detail off. Invalid combinations return typed policy errors; nothing
is reparented. Workflow `Extended` retains `trace_nodes` at registration so
all owners agree about node spans.

### Redaction and offload

One `ContentHook::on_content` sees every captured field via
`RecordContent::for_each`, with its category, typed path and text/JSON value.
Conveniences access system instructions, input messages and output parts.
Install it with `content_hook`. Order is policy, hook, then truncation, for
every sink and regardless of sampling. The hook must not block, perform I/O or
re-enter the engine.

A tool call's `ToolResults` field is `tool_results`. For a success it is the
result value. For a failure (`TraceToolFailure`) or a cancellation
(`TraceToolCancellation`) it is the call's `diagnostics`: one JSON object with
`message`, plus `cause` and `raw` when the failure carries them. A hook may
rewrite or omit it like any other field. The classification beside it is typed
structure, not content: a failure's `class`, `code`, `source` and
`suggested_delay_ms`, and a cancellation's `source`, `origin` and `forced`.
No policy, hook or bound reaches it, and the exporter's `error.type` reads the
terminal failure's `class` directly.

To offload without exporting inline content:

1. Capture the required categories at the core ceiling.
2. In the hook, enqueue owned content in the host's bounded upload queue under
   a host-generated key, without awaiting or uploading inline.
3. Add `content.reference("app.content.ref".into(), key.into())`; handle a typed
   refusal. References charge their sub-cap and the span contribution budget.
4. Call `content.omit(category)` for the offloaded category.

References commit only if the hook returns normally. A panic empties all
captured content, discards this call's references, sets
`ContentMarks.hook_failed` and counts `HookFailed`. It cannot retract work
already handed to the host's queue; that queue and access policy are the host's.

`ContentLimits::standard()` cuts prose parts at **16 KiB**, which truncates
typical agent system prompts. Hosts wanting the full prompt raise
`max_part_bytes`; Figments uses 64 KiB parts and 256 KiB attributes:
`ContentLimits::new(64 * 1024, 256 * 1024, 2048)?`.
Strings are cut at UTF-8 boundaries; tool/extension JSON string leaves are cut
structurally. Old input messages are dropped whole until the attribute fits.
Schemas, provider bodies and response metadata stay atomic. Any field or
serialized content attribute still exceeding `max_attribute_bytes` is omitted
whole. JSON text is never cut mid-document. `ContentMarks` distinguishes
capture, omitted fields, truncation, oversize and hook failure.

### Semantic content and vendor rendering

Lash follows GenAI semconv snapshot `07794021`: messages have `role` and `parts`,
system instructions are a parts array, tool args/results are JSON objects
(non-objects wrapped under `value`), and span attribute values are serialized
JSON. Tool response parts use `tool_call_response.response`; reasoning parts
use `reasoning`. Laminar's renderer reads `result`/`thinking` and semconv
messages only on LLM spans. A Laminar host maps these itself: for example, map
an agent's `invoke_agent` closing `gen_ai.output.messages` into the host's agent
output representation, and map nested tool response `response` to `result`
and `reasoning` to `thinking` where its renderer expects those spellings.
Lash's records and semconv contract keep their own shapes.

## Enrichment and Laminar

`OtelOptions::enricher` installs one `SpanEnricher`. It runs once per exported
span before SDK creation; producers run it before sampling. Its `SpanView`
contains kind, default name, `TraceIds`, position, closing/whole record,
attributes and effective policy. On an attempt, `record` is the shared logical
call closer: use `view.attempt` for that attempt, and `view.usage()` for the
folded logical usage where available. No host identity is retained by lash.
The enricher must not block, do I/O or re-enter the engine; use a host-owned
lookup snapshot. A panic discards enrichment, exports lash's attributes and
counts `EnricherFailed`.

`set` adds host keys, `rename` changes the name, and `replace(RefinableKey, ..)`
refines a registered semantic default. Provenance is immutable. `set` on a
refinable key refuses `UseReplace`; refinement order is lash, body, host.
Metric labels retain lash's defaults. Every method returns a typed omission
on refusal and charges the bounded writer before buffering.

Laminar is host code, using the same ids as every sink. This example's
`session_labels` and `upstreams` are host-owned maps populated outside callbacks:

```rust
use std::collections::HashMap;
use lash::tracing::{RefinableKey, SpanEnricher, SpanEnrichment, SpanView, TraceSpanKind};

struct Laminar {
    // session id -> (agent name, host user id)
    session_labels: HashMap<String, (String, String)>,
    upstreams: HashMap<String, String>, // llm_call id -> routed provider
}
impl SpanEnricher for Laminar {
    fn enrich(&self, span: &SpanView<'_>, out: &mut SpanEnrichment) {
        let kind = match span.kind {
            TraceSpanKind::Model => "LLM",
            TraceSpanKind::Tool | TraceSpanKind::Cell => "TOOL",
            _ => "DEFAULT",
        };
        let _ = out.set("lmnr.span.type", kind);
        if let Some(session) = &span.ids.session {
            let _ = out.set("lmnr.association.properties.session_id", session.to_string());
            if let Some((agent, user)) = self.session_labels.get(&session.to_string()) {
                let _ = out.set("lmnr.association.properties.user_id", user.clone());
                if span.kind == TraceSpanKind::Agent {
                    let _ = out.set("gen_ai.agent.name", agent.clone());
                    let _ = out.rename(format!("invoke_agent {agent}"));
                }
            }
        }
        if let Some(provider) = span.ids.llm_call.as_ref().and_then(|id| self.upstreams.get(id)) {
            let _ = out.replace(RefinableKey::GenAiProviderName, provider.clone());
            let _ = out.set("gen_ai.system", provider.clone());
        }
        if let Some(usage) = span.usage() {
            let _ = out.set("gen_ai.usage.cache_read_input_tokens", usage.cache_read_input_tokens);
            let _ = out.set("gen_ai.usage.cache_creation_input_tokens", usage.cache_write_input_tokens);
        }
    }
}
```

Unmapped providers can copy the default `gen_ai.provider.name` from
`SpanView.attributes` into Laminar's `gen_ai.system`. Cost is host policy:
price the folded usage or look up a ledger by `TraceIds.llm_call`. To use a
provider-reported cost, allowlist it under ADR 0038, capture
`ResponseMetadata`, and read the closing record. Lash has no price table or
typed cost field. Set neither `lmnr.span.path` nor `ids_path` unless the host
supplies both; parent ids already describe the tree. Monitor omissions rather
than silently assuming the example's ignored results succeeded.

## Extensions and ambient context

Plugins receive a registration-bound `ExtensionTrace`, never caller-supplied
plugin attribution. `span`, `span_with`, `event`, `position`, `as_parent`,
`content_policy`, `remaining` and `omissions` are the extension API.
`SpanSpec` offers internal/client/producer/consumer spans and embeddings or
retrieval presets. `ExtensionSpan` supports attributes, lazy content, events,
failures, children, `trace()` and explicit `end()`; dropping it ends it too.
`trace.event` emits a zero-duration point span; `span.event` buffers an event
on that span. Plain attributes are never a place for prompts, documents or secrets. Use
`content(key, || value)` so capture, hook and bounds apply.

The [plugin authoring guide](../plugins.md#tracing-an-extension) covers body
lifetime, revocation, the tool annotation allowlist and tool-live joins. The
[generated slot scopes](#callback-slot-scopes) come from the canonical callback
slot table. `Attempt` and `Call` stand at the tool span, `Turn` at the agent,
`Executing` at the cell or turn, `Producer` at the operation's producer;
`Untraced` grants no recording capability. `ProcessEngine::advance` remains a
pure state machine; plugin factories receive policy with no position.

A body owns its emission right. Normal return, cancellation and unwind close
its open spans and revoke every clone. Late mutations are counted no-ops.
Journal-served reconstruction emits nothing. Children of unpublished extension
parents stay held; runtime span-budget refusal mints no span and leaves nested
work at the nearest minted ancestor. Work lash durably admits uses
`as_parent()`, the anchored container, even through an extension's `trace()`.
It never retains an execution-local extension parent.

`trace.instrument(future)` enters the position on every poll; `in_scope` does so
synchronously. Lash wraps its own callbacks and provider dispatch. Use
`span.trace().instrument(work())` for nested library work and
`spawn(trace.instrument(work()))` for each spawned task. With `otel-trace`,
`ExtensionTraceOtelExt::otel_context()` supplies an explicit OTel context.

The supported `tracing-opentelemetry` 0.33 configuration has its default
`context_activation = true`. Lash attaches its context as the innermost poll
wrapper, after an outer Rust tracing span enters, so instrumented libraries
parent to the current tool or extension position. With activation disabled the
layer consults tracing-span data instead, so lash cannot steer those parents.
Third-party spans use the host sampler and bypass lash's capture policy and
emission permit; re-executing a body emits them again. The plugin author owns
content-bearing third-party instrumentation.

## Model provider propagation

Lash injects no HTTP trace headers. Wrap the host's `LlmHttpTransport`; its
`send(request, timeout)` reads the current OTel context while polled under the
`chat`, injects W3C `traceparent` and `tracestate` into `request.headers` using
`HttpHeaderValue::new`, then delegates to the inner transport. An
`opentelemetry_sdk::propagation::TraceContextPropagator` with a host
`Injector` over that header vector handles formatting. Pass the wrapper to the
provider constructor:

| Provider | Transport constructor |
|---|---|
| `OpenAiCompatibleProvider` | `with_transport(Arc<dyn LlmHttpTransport>)` |
| `OpenAiProvider` | `with_transport(Arc<dyn LlmHttpTransport>)` |
| `AnthropicProvider` | `with_transport`; `with_client` wraps the client in the same transport |
| `GoogleOAuthProvider` | `with_transport`; `with_client` likewise |
| `CodexProvider` | `with_http_transport`, HTTP/SSE only |

`CodexProvider` defaults to Auto transport, trying a cached WebSocket connection
before SSE. WebSockets bypass `LlmHttpTransport` and do not carry per-request
headers. Hosts requiring per-request propagation call **`force_sse_transport()`**
as well as installing the HTTP wrapper. The [transport contract](../../crates/lash-http-transport/src/http.rs)
defines the request and typed headers.

MCP propagation is destination policy instead: `McpServerConfig.trace_propagation`
defaults to `W3c`. Each `tools/call` puts the tool span's `traceparent` and
`tracestate` in `params._meta`, beside `lash.dev/*`. `Off` omits those two keys
without suppressing the tool span. Third-party clients propagate according to
their own instrumentation.

## Budgets and omission diagnostics

Every optional contribution charges one per-span budget before buffering:
`attributes_per_span - attribute_reserve(kind)` attributes and
`span_contribution_bytes` bytes. Body annotations, extension attributes/events,
hook references, enrichment and producer sampler attributes share it in arrival
order; source sub-caps also apply. Replacing a reserved default charges bytes,
not another attribute slot. A string exceeding `attribute_value_bytes` is cut
at a UTF-8 boundary (`ValueTruncated`); JSON is refused whole
(`ValueTooLarge`); non-finite floats are refused (`NonFinite`).

The [reserve table](#registry-attribute-reserves) is generated per kind. Lash's
attributes precede contributions in the SDK. The SDK is a **second stage**:
with `max_attributes_per_span >= attributes_per_span` it loses nothing to its
count bound; a lower SDK bound drops contributions first and preserves every
lash attribute while at least `attribute_reserve(kind)` slots remain.

Omissions do not fail execution. They are visible at four levels:

- Writer methods return `TraceOmission`; chainable extension methods expose
  counts via `ExtensionSpan::omissions()` and `ExtensionTrace::omissions()`.
- Each affected record keeps its omission counts with its id/position. Body
  exit emits one `BodyOmissions` diagnostic at its callback, span, call and
  attempt address. Two bodies of the same callback stay distinguishable.
- Spans carry `lash.trace.omitted` reasons and `lash.trace.omitted.count`.
- `LashCore::trace_omissions()` aggregates by site kind and reason;
  `lash.trace.omissions{reason}` is a runtime metric without identity labels.

Sites distinguish body, record, span, shift, admission and cell. Key/namespace,
name, count/byte, reference, hold, link, late-write and callback failures have
typed reasons. The content marker separately names truncated/oversize paths.
Logs report degradation episodes without repeating full payloads.

## Configuration

Policies are sealed and validated. `TracePolicyError::Range` names field,
value and bounds; `Relation` names both operands and values, including
`Reserve(kind)` or `Remaining(kind)`; `Structural` names an invalid selection.
The facade returns `EmbedError::TracePolicy`. `LashCore::trace_policy()` reports
the effective policies. `OtelOptions.metrics` selects `TraceMetrics::Off`,
`Runtime`, `GenAi` or the default `All`. A deployment ceiling is node-local, not session config;
only `trace_nodes` is retained as a structural cross-owner choice.

| Level | Installation and default |
|---|---|
| Core ceiling | `content_policy` (omitted), `trace_detail` (Standard), `trace_limits`, `content_limits` (standard bounds) |
| Core, per-record narrowing | `capture_selector` (none); can only narrow content and optional leaves |
| Core hook | `content_hook` (none); one, host-composed |
| Exporter | `OtelOptions`: all built kinds, all metrics, no enricher |
| Per call | `trace_context` / `trace_context_none` (otherwise ambient), `trace_link_mode` (Parent) |
| Destination | MCP `trace_propagation` (W3c); model headers in a host transport (none injected by lash) |
| Host provider | required sampler and `id_fallback`; host resource, limits, pipeline and lifecycle |
| Host ingress | `TraceCarrier::parse_w3c_with` and `TraceStateLimits` (W3C ceilings); invalid data is `InvalidTraceCarrier` |
| Host sink | `FlightRecorderSettings`, inner sink and snapshot callback; callback must not block or re-enter |

`EvidenceLimits`, `DiagnosticLimits` and `CompositionCacheLimits` bound
execution evidence, diagnostics and working memory, rather than export. Their
facade installers and defaults are in the
[hosting limits reference](../operations/durable-hosting.md).

Regenerate the section below with:

```sh
kiln test //crates/lash:otel_trace_evidence__test --local-test-execution \
  --test_arg=--ignored --test_arg=--exact \
  --test_arg=tracing_configuration_is_regenerated \
  --test_env=BUILD_WORKSPACE_DIRECTORY=$PWD
```

<!-- BEGIN GENERATED CONFIGURATION -->

Generated from policy rustdoc and its numeric field registry by
`tracing_configuration_is_regenerated` in `otel_trace_evidence.rs`.
Numeric sizes are bytes, durations milliseconds, other values counts.

### ContentPolicy (core ceiling)

The node's ceiling: all categories omitted by default. A selector can only narrow it.

### CaptureSelector (core, per record)

Runs before a record or optional leaf is built. It must not block or re-enter.

### ContentHook (core)

One synchronous host hook. It must not block, do I/O or re-enter the engine.

### ContentLimits (core)

The bounds lash puts on the content a record carries.

Sealed: built through `ContentLimits::new`, which refuses a value
outside its range.

### ContentLimits defaults

16 KiB parts, 128 KiB attributes and a 2048-byte provider request body.

### ContentLimits bounds and relations

Part: 1 KiB..=1 MiB; attribute: 1 KiB..=8 MiB; provider: 0..=8 MiB.
Parts and provider bodies must each fit the attribute bound.

### TraceDetailPolicy (core)

What lash builds for each span kind: one `TraceDetail` per kind.

A container (producer, turn, tool, cell, process, workflow, node, tool
intent) cannot be off, and only a model call and a workflow have
extended detail. A leaf can be off only with every kind it can parent:
a model call with its attempts, an extension span with points, model
calls and their attempts.

### TraceDetailPolicy refusals

Refuses, in kind order, a container turned off, extended detail for a
kind that has none, and any workflow node setting but `Standard`
(`Structural`); then a leaf turned off while a kind it can parent is
on (`Relation`, `ParentOffChildOn`).

### TraceExportPolicy (exporter)

Which kinds the OpenTelemetry exporter turns into spans. A skipped kind's
records still reach every sink, and its metrics are still recorded.

A container is always exported, and a leaf may be skipped only with every
kind it can parent, so no exported span names a parent that was not.

### TraceMetrics (exporter)

Which metric families the exporter records.

### OtelOptions (exporter)

The exporter's host choices.

### OtelOptions defaults

No enricher, every kind exported, every metric family recorded.
Export remains opt-in through installing the exporter.

### TraceLimits (core)

The admission and buffering bounds of the tracing seam: how much a
shift holds, how many links a span keeps, and what every host or
plugin contribution to a span may take.

Sealed: built only through `TraceLimits::builder`, which checks
every row against its documented range and then the relations
between rows. Each accessor documents its default and range.

### TraceStateLimits (host ingress)

A host's narrowing of the W3C `tracestate` ceilings, for carriers it
parses from its own transport (`TraceCarrier::parse_w3c_with`). The W3C
maxima are the defaults and the hard ceilings: a host narrows them, never
raises them.

Sealed: built through `TraceStateLimits::new`, which refuses a value
over its ceiling.

### TraceStateLimits defaults

The W3C ceilings: 32 members, 512 characters, 256-byte keys, 241-byte
tenant and 14-byte system parts of a multi-tenant key, and 256-byte
values.

### FlightRecorderSettings (host sink)

Host policy for retention and snapshot frequency, independent of content
policy.

Sealed: built through `FlightRecorderSettings::new`, which refuses a
value outside its range.

### FlightRecorderSettings defaults

256 records, a 5 s slow threshold, a 60 s minimum interval, and
snapshots on failure.

### FlightRecorderSettings bounds

Settings that retain at most `capacity` records, including a
snapshot's triggering record (`flight_recorder.capacity`: 1..=16384);
snapshot when a reported duration strictly exceeds `slow_threshold`,
or never on duration when it is `None`
(`flight_recorder.slow_threshold_ms`: 1 ms..=24 h); accept triggers at
least `minimum_interval` apart in record time, older timestamps
suppressed even at zero (`flight_recorder.minimum_interval_ms`:
0..=24 h); and snapshot records whose typed outcome is a failure when
`snapshot_on_failure` is set.

### TraceLimits field contracts

- Records a shift holds behind its journal frontier, lash's and extensions' alike; the overflow is dropped and counted (`trace_limits.held_observations`: default 256, 0..=4096).
- Links one span keeps, at its admission and at its close; the overflow is counted in `lash.links.omitted` (`trace_limits.links`: default 64, 0..=128).
- Extension and point spans one body execution opens; 0 turns both off, which requires model calls and their attempts off too (`trace_limits.extension_spans`: default 256, 0..=4096).
- Attributes of one assembled span, lash's included; contributions share what the registry's reserve for the kind leaves. Also the attributes of one event (`trace_limits.attributes_per_span`: default 128, 1..=1024, at least every kind's reserve).
- Events per extension span (`trace_limits.events_per_span`: default 64, 0..=256).
- Bytes of one contributed value: a longer string is cut, a longer JSON value refused (`trace_limits.attribute_value_bytes`: default 4 KiB, 64 B..=64 KiB, at most `span_contribution_bytes`).
- Every byte one body's contributions charge: names, keys, values and events (`trace_limits.extension_bytes`: default 1 MiB, 4 KiB..=16 MiB, at least `attribute_value_bytes`).
- Annotations a tool body sets on its tool span (`trace_limits.tool_span_attributes`: default 16, 0..=64, at most `attributes_per_span` less the tool kind's reserve).
- Bytes of one span, event or error-type name a plugin or the enricher supplies (`trace_limits.name_bytes`: default 1 KiB, 16 B..=16 KiB).
- Bytes of one contributed key (`trace_limits.attribute_key_bytes`: default 128 B, 16 B..=1 KiB).
- Every byte the optional sources contribute to one span: body annotations, extension attributes and events, hook references, the sampler's attributes and enrichment (`trace_limits.span_contribution_bytes`: default 256 KiB, 4 KiB..=8 MiB).
- Content-hook references per record (`trace_limits.references`: default 16, 0..=64, at most `attributes_per_span` less the largest content-bearing kind's reserve).
- Content-hook reference keys and values per record (`trace_limits.reference_bytes`: default 64 KiB, 0..=1 MiB, at most `span_contribution_bytes`).

### Numeric defaults and inclusive bounds

| Field | Default | Min | Max |
|---|---|---|---|
| `content_limits.max_part_bytes` | 16384 | 1024 | 1048576 |
| `content_limits.max_attribute_bytes` | 131072 | 1024 | 8388608 |
| `content_limits.max_provider_body_bytes` | 2048 | 0 | 8388608 |
| `trace_limits.held_observations` | 256 | 0 | 4096 |
| `trace_limits.links` | 64 | 0 | 128 |
| `trace_limits.extension_spans` | 256 | 0 | 4096 |
| `trace_limits.attributes_per_span` | 128 | 1 | 1024 |
| `trace_limits.events_per_span` | 64 | 0 | 256 |
| `trace_limits.attribute_value_bytes` | 4096 | 64 | 65536 |
| `trace_limits.extension_bytes` | 1048576 | 4096 | 16777216 |
| `trace_limits.tool_span_attributes` | 16 | 0 | 64 |
| `trace_limits.name_bytes` | 1024 | 16 | 16384 |
| `trace_limits.attribute_key_bytes` | 128 | 16 | 1024 |
| `trace_limits.span_contribution_bytes` | 262144 | 4096 | 8388608 |
| `trace_limits.references` | 16 | 0 | 64 |
| `trace_limits.reference_bytes` | 65536 | 0 | 1048576 |
| `trace_limits.publication_hold.records` | 256 | 1 | 4096 |
| `trace_limits.publication_hold.bytes` | 8388608 | 131072 | 67108864 |
| `tracestate_limits.members` | 32 | 0 | 32 |
| `tracestate_limits.chars` | 512 | 0 | 512 |
| `tracestate_limits.key_bytes` | 256 | 0 | 256 |
| `tracestate_limits.tenant_bytes` | 241 | 0 | 241 |
| `tracestate_limits.system_bytes` | 14 | 0 | 14 |
| `tracestate_limits.value_bytes` | 256 | 0 | 256 |
| `flight_recorder.capacity` | 256 | 1 | 16384 |
| `flight_recorder.slow_threshold_ms` | 5000 | 1 | 86400000 |
| `flight_recorder.minimum_interval_ms` | 60000 | 0 | 86400000 |

### Registry attribute reserves

| Span kind | attribute_reserve |
|---|---|
| `producer` | 36 |
| `agent` | 26 |
| `model` | 47 |
| `model_attempt` | 30 |
| `tool` | 31 |
| `cell` | 29 |
| `process` | 26 |
| `workflow` | 24 |
| `workflow_node` | 22 |
| `tool_intent` | 24 |
| `extension` | 26 |
| `point` | 22 |

### Callback slot scopes

Generated from `CallbackSlot::ALL` and `CallbackSlot::trace_scope` in `lash-vocabulary/src/providers.rs`.

| Slot | Scope |
|---|---|
| `ToolProvider` | `Attempt` |
| `ToolReads` | `Untraced` |
| `PackageProvider` | `Untraced` |
| `DeferredPackageProvider` | `Untraced` |
| `ToolCatalog` | `Untraced` |
| `BeforeTurn` | `Turn` |
| `ToolArgsTransform` | `Call` |
| `ToolArgsCheck` | `Call` |
| `ToolResultTransform` | `Call` |
| `ToolResultCheck` | `Call` |
| `AfterTurn` | `Turn` |
| `Checkpoint` | `Turn` |
| `AssistantStream` | `Turn` |
| `AssistantResponse` | `Turn` |
| `AssistantStreamFinished` | `Turn` |
| `PresentationStep` | `Untraced` |
| `PresentationPresenter` | `Untraced` |
| `RuntimeEvent` | `Untraced` |
| `Operation` | `Producer` |
| `AttachmentOmission` | `Untraced` |
| `ContextCompactor` | `Turn` |
| `ContextPressure` | `Turn` |
| `ProtocolSession` | `Executing` |
| `ProtocolDriver` | `Executing` |
| `CodeExecutor` | `Executing` |
| `AssistantProseProjector` | `Untraced` |
| `CellFailureFeedback` | `Untraced` |

<!-- END GENERATED CONFIGURATION -->

## Writing records to a file

Lash ships no file or console writer. Hosts choose format, rotation, queues and
durability with their own `TraceSink`, then install it through `trace_sink`.
`append` and a flight-recorder snapshot callback run synchronously outside
engine locks and must not block on or re-enter the engine. A production sink
can enqueue into a bounded writer queue; this minimal synchronous file example
illustrates the record serialization seam:

```rust
use std::{fs::File, io::Write, path::PathBuf, sync::Mutex};
use lash::tracing::{TraceRecord, TraceSink, TraceSinkError};

struct JsonlFile { path: PathBuf, file: Mutex<File> }
impl TraceSink for JsonlFile {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        file.write_all(&line)
            .map_err(|source| TraceSinkError::Write { path: self.path.clone(), source })
    }
}
```

Serialize each record and newline as one locked write so concurrent writers
cannot interleave lines. Override `flush` for the host's durability contract
(`sync_all` when required). Tests read typed records through
`lash::testing::RecordingTraceSink`. The
[flight recorder](../perf/instruments/flight-recorder.md) is a bounded metadata
ring and host callback, with no output destination of its own. The
[process observation guide](../observing-processes.md) explains product graphs,
which reconstruct on replay through a separate `ProductObservationSink`.
