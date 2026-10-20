Plugins are fully trusted in-process Rust with the host's full authority; lash provides no sandbox or CPU/memory isolation ([ADR 0143](adr/0143-plugins-are-fully-trusted-code.md)).

Recovery and external-effect guarantees follow [What survives a crash](../README.md#what-survives-a-crash).

# Authoring and certifying a plugin

Declare metadata and executable binders together with `PluginRegistrar`.
`PluginRecipe::bound` binds plain session data to those entries; use
`PluginRecipe::new` when your owner implements a readiness callback.
`PluginRecipe::from_registrar` joins entries that need no session-local data.
Prompt, state reducer, tool and config entries still use the one registrar
and the production executor.

The [compiled specimens](../crates/lash/examples/plugin_authoring.rs) demonstrate:

- `greetings`: two fixed tools sharing a recorded per-session prefix.
- `protected_path`: a keyed before-tool check over the provider-prepared call.
- `counter`: a reducer publishes state and a prompt section reads the committed cut.
- `large_state`: publish 64 KiB under raised host limits and reopen on a fresh runtime.

They compile as a facade example and in the facade's
`plugin_certification__test` target. Their certification laws are executable;
these replace the uncompiled factory recipes. Creation-only namespaces still use `ConfigOwner`, including explicit validation
and `NoRunOptions`. The proposed adapter was not admitted: only the tool/config
specimen needed it. The state/prompt specimen needs no configuration owner.

## One-call certification

```rust
let report = lash::testing::certify_plugin(factory).await?;
assert!(report.failures().next().is_none());
// A host needing complete evidence additionally requires:
assert_eq!(report.verdict(), lash::testing::PluginCertificationVerdict::Full);
```

Enable the facade's `testing` feature in a development dependency.
`certify_plugin` enumerates the factory's callbacks from `InstalledInventory`
and reports the applicable laws for each callback:

| Law | Evidence |
| --- | --- |
| REPLAY-INVOKES-NO-RECORDED-CALLBACK | Counting adapters on the recipe's binders, a recorded cut, runtime destruction and continuation of the same accepted input. |
| STATE-SURVIVES-REOPEN | Published namespaces, including unfinished-turn rows, compared after fresh materialization under the host's `RuntimeDataLimits`. |
| UNSERVED-IS-UNCLAIMABLE | A session actor owing the declared callback token, a node lacking that registration, `plugin_diagnostics().explain_claimability` and the real SQL claim predicate. |
| LIFECYCLE-OBSERVER-COMPLETES | An automatic SQLite session delivers restoration and finalized-turn events; an explicit completion receipt checks the observer returned successfully within the host scenario timeout. |
| RECORDED-DECISIONS-STAY-ATTRIBUTABLE | `plugin_diagnostics().attribute` reads retained binding/presentation decisions after removing the factory. |

A `lifecycle().on_event` observer receives the lifecycle completion law and
UNSERVED-IS-UNCLAIMABLE. Its advisory notifications publish no recorded decision
or state, so the replay, publication and attribution laws have no subject.
No author scenario is required for observation; `certify_plugin_with` can supply
one when the observer needs configuration or supporting services. Other
contributions retain their scenario and publication requirements.

An executed law is `Passed` or `Failed` with a readable message.
`NotApplicable` has a typed reason for a law with no subject, while
`Unavailable` means applicable evidence or author input is missing. The verdict
is `Failed` if any law fails, `Partial` if an applicable law is unavailable,
and `Full` when every applicable law is exercised and passes. An exercised
callback that can publish state needs a scenario that publishes values:
an empty namespace yields `Unavailable { reason:
CertificationUnavailable::ScenarioDidNotPublish }`. Every tool body can return
state commands, so a stateless tool provider, such as an HTTP lookup, declares
it with `PluginRecipe::stateless()`. The runtime enforces that declaration: it
refuses every state command the plugin returns as `DecisionOnly` and refuses to
install it with a state reducer or initial values. Its callbacks, and a
decision-only contribution that ran no tool body and whose factory declares no
state reducer, receive `NotApplicable { reason:
CertificationNotApplicable::NoPublishedState }`, after verifying that the
namespace stayed empty on reopen.
A factory whose inventory has no certifiable callbacks, such as a prompt-only
plugin, receives `NoCertifiableContributions`. Its inventory passed admission,
but the battery proves no prompt rendering, replay or reopen behavior.
A host decides whether partial evidence is sufficient. An unexecuted callback
never receives an execution-law pass; a deferred callback without an author
scenario remains `Partial`.

`PluginScenario` supplies typed author scenarios for callbacks requiring
specific tool inputs, history, services or credentials. Register scenarios
with `certify_plugin_with(factory, policy, &scenarios)`. Its `prepare` method
can install supporting factories, create the session with `PluginOptions`
and build preconditions through the real core. Its scripted model responses
and input run through `send()`. `availability` can report a typed missing
service or credential instead of attempting the scenario. Protocol authors
select `HostPolicy.plugin_role = PluginRole::Protocol`.

The crash cut is before a scripted model response, after the preceding
production phase committed. A scenario states how many model answers precede
that cut. It must provide the pending response as well. A completed callback
is counted before and after reopen; unfinished work is allowed to run.
Request-view policies such as context pressure and attachment omission can
execute again during preparation, so their invocation alone does not prove
a recorded answer. These are reported as `NotRecordedAtCut`.

Check replies have no retained attribution in the current baseline.
Before-turn records name their owner without a callback key. Those branches
report `AttributionNotRecorded`; the battery never guesses a callback from
its installed roster. Empty state never excuses an unexecuted callback.
`NoScenario` means no author scenario was supplied. A scenario that reaches
the cut without invoking its callback reports `ScenarioNotExercised`, with
whether it was supplied and invocation counts before the cut and during reopen.
For a deferred provider those counts are resolutions: a standard-protocol
tool call does not exercise the resolver. Use the supporting RLM scenario below.
MCP without a server fixture is explicit partial certification. RLM and kernel
process factories need a backend-bound VM client and workers; their default
scenario is `NeedsVmBackend`, outside this battery's cheap fixtures.

## Deferred resolvers under a supporting protocol

An extension keeps `PluginRole::Extension` when its tools run under RLM.
`PluginHarness::install_protocol` installs the author's supporting protocol,
including the VM service bound to that factory. The harness retains the same
factory on every certification reopen and when removing the primary plugin
for attribution. `install` continues to install supporting extensions. A
second supporting protocol, or one supplied for a primary protocol, returns
`PluginHarnessError::SupportingProtocolConflict`.

The [complete compiled deferred scenario](../crates/lash/tests/plugin_certification.rs)
(`deferred_state_free_provider_is_full_under_a_supporting_protocol`) registers
an empty-catalogue resolver and a separate empty-catalogue executable. Its
`prepare` installs RLM in the native tool channel with a bound worker service,
installs the executable, and creates the session. Its response script is:

```rust
vec![
    lash::provider::LlmResponse {
        parts: vec![lash::direct::LlmOutputPart::ToolCall {
            call_id: "deferred-cell".into(),
            tool_name: "execute_code".into(),
            input_json: serde_json::json!({
                "code": "await mail.sendEmail({});"
            }).to_string(),
            replay: None,
        }],
        ..Default::default()
    },
    lash::testing::text_response("done"),
]
```

The scenario names `PluginCallbackIdentity::tool_provider("resolver", "deferred")`
and pauses after one response. The first cell must resolve and execute before
that cut; prose completes the resumed turn without introducing another cell.
The resolved definition carries its operation (`mail.send_email`) and
execution bound. Hosts put real authorization and other preconditions in `prepare`,
through the core, and supply one scenario for each callback they certify.

Resolver entries are exposed separately through `deferred_resolution_counts`;
`callback_counts` observes prepare/execute on the selected executable. A
callable placeholder under the resolver's provider key cannot supply resolution
evidence. Attribution joins the retained turn surface's resolver identity to
completed cell calls outside that surface's catalogue. It remains readable
through `DecisionRef::DeferredTool` after removing the resolver; it never
consults a live catalogue or resolves a committed selection again. With no
published state, the compiled scenario receives three passed laws and one
not-applicable state law, hence `Full`.

## Publish and reopen with host limits

The compiled `large_state` example builds the author's publisher once. Its
host-specific part is only:

```rust
let mut policy = lash::testing::HostPolicy::standard();
policy.data_limits.plugin_state.max_value_bytes = 128 * 1024;
let mut harness = lash::testing::PluginHarness::new(factory, policy).await?;
harness.create(lash::plugins::PluginOptions::default()).await?;
harness.script([lash::testing::text_response("published")]);
harness.publish_then_reopen(lash::TurnInput::text("publish")).await?;
let state = harness.plugin_state().await?;
```

`PluginHarness` uses lash's own durable engine over in-memory SQLite. Each
boot has a fresh runtime. `crash` destroys its execution tasks without a drain
or factory shutdown; `reopen` builds a fresh runtime over the same SQL stores.
It also exposes the core, accepted input handles, scripted requests and callback
counts for the author's assertions. It supplies no second executor and never
calls a callback to stand in for a turn.

For a mid-turn law, arm `pause_model_after`, send, await
`wait_for_model_pause`, crash, then `reopen_at_model_pause`. Inspect restored
state before `release_model` and reattach the original accepted input. Keep
old live-session handles out of the reopen: they retain the old runtime's
capabilities. The timeout is a scenario liveness aid, not isolation of fully
trusted plugin code.

## Tracing an extension

Execution callbacks receive a registration-bound `ExtensionTrace`: a tool
body through `call.context.trace()`, a hook through its context's `trace`
field, a plugin operation through its context's `trace()`. Lash binds it
from the callback's registration, so every span it opens carries the
registered `plugin_id` and `lash.plugin.callback`; nothing the plugin
passes can change them. The canonical callback slot table declares one trace
scope per slot; the [generated slot scopes](architecture/tracing.md#callback-slot-scopes)
list every slot without a separate API inventory. `Attempt` and `Call` mean the
call's tool span, `Turn` the agent span, `Executing` the running cell (otherwise
the turn), and `Producer` the operation's host-call producer. `Untraced` readers
and decision-only slots receive no recording capability. `ProcessEngine::advance`
is a pure state machine and receives none; plugin factories read deployment
policy without a position.

```rust
async fn execute(&self, call: lash::ToolCall<'_>) -> lash::ToolAttemptOutcome {
    let trace = call.context.trace();
    let mut span = trace.span("index.lookup");
    span.attr("index.shard", 3_i64)
        .content("query", || call.args["query"].clone().into());
    let hits = span.trace().instrument(self.lookup(&call.args)).await;
    span.trace().event("index.hit", |attrs| {
        attrs.set("index.hits", hits.len() as i64);
    });
    span.end();
    // ...
}
```

- **Lifetime.** A span ends at `end()`, at `fail(error_type)` plus `end()`,
  or when its handle drops (`lash.span.ended_by = "drop"`). The callback's
  exit revokes the capability: spans still open are closed `body_exit`, or
  `cancelled` or `panic` when the body was dropped or panicked, and a later
  `end()` or mutation from a task the body spawned emits nothing and is counted
  `Late`. Record what matters before the body returns.
- **Replay.** Spans are emitted only by a body that really runs. A body the
  journal serves emits none; a body that runs again after a crash emits its
  spans again, as distinct spans. Protocol bodies running behind a shift's
  journal frontier hold their spans until a step body really runs.
- **Bounds.** `TraceLimits` bounds the spans one body opens, the bytes its
  names, keys, values and events charge, and the records a span may hold
  for its children (`publication_hold`); under hold pressure the span is
  ended early (`hold_pressure`) and no model call's record is dropped.
  `remaining()` reads the budget left, and `omissions()` what was refused,
  each refusal counted with its typed `TraceOmission`; the body's exit
  records one `body_omissions` diagnostic when anything was refused.
- **Content.** `content(key, || value)` runs its builder only when the
  host captures the `extension` category and the span records. Attributes
  are not content: never put prompts, documents or secrets in them.
- **Policy.** When the host turns `Extension` (or `Point`) off, or a capture
  selector skips them for the call, the handles are no-ops and no builder
  runs; a model call made under a skipped span is skipped with it.
- **Nesting libraries.** `instrument(fut)` polls `fut` with the position as
  the ambient context, and `in_scope(f)` runs `f` under it: OpenTelemetry
  instrumentation (an HTTP client, a database driver) and `tracing` spans
  exported through `tracing-opentelemetry` at its default
  `context_activation = true` nest under the span, and a direct model completion
  made inside nests its `chat` span there. Lash runs every callback under
  its capability's position already; use `instrument` for work under a span
  of your own and for every task the body spawns. These third-party spans
  are the host SDK's: lash's content policy does not reach them, and a body
  that runs again emits them again. With the `otel-trace` feature,
  `ExtensionTraceOtelExt::otel_context()` hands a library that takes an
  explicit context the position's. Disabling context activation is unsupported: it
  makes the layer choose tracing-span data instead of Lash's attached position.
- **The tool span.** A tool body annotates its own `execute_tool` span with
  `call.context.tool_span().set(key, value)`, through the same bounded
  writer, for the `mcp.*`, `jsonrpc.*`, `rpc.*`, `network.*` and `server.*`
  namespaces and the `gen_ai.tool.type` and `error.type` defaults; other
  keys are refused `DisallowedKey`. `call.context.tool_span_context()` is
  that span's W3C context, equal to the exported span's, wherever the body
  is in its own spans: the parent for OpenTelemetry work the body starts
  itself, with no record sink.
- **Joining tool-live.** `call_address()` is the `CallAddress` the call's
  tool-live publisher is bound to; it equals the span's
  `gen_ai.tool.call.id` call. Hooks have none.
- **Propagation.** `position()` is the W3C context of the position, equal to
  the exported span's, for an outbound carrier: a queue message, an HTTP
  header. `as_parent()` is the anchored container, for work lash admits on
  the plugin's behalf. Even through `span.trace()`, admitted durable work uses
  that anchored container, never the execution-local extension span.

The MCP plugin propagates for you: each `tools/call` sends the tool span's
`traceparent` and `tracestate` in `params._meta`, beside `lash.dev/*`, and
annotates the tool span with `mcp.method.name`, `network.transport`,
`jsonrpc.request.id` and, for an `isError` result, `error.type =
tool_error`. A server whose operators must not see the trace is configured
`trace_propagation: off` (`McpTracePropagation::Off`); its tool span is
still exported.

### Propagating to a model gateway

Lash injects no trace context into provider requests. A provider's request
runs with its `chat` span as the current OpenTelemetry context, so a host
that wants its gateway to join the trace wraps the provider's transport:

```rust
use std::sync::Arc;
use std::time::Duration;
use lash::provider::LlmTransportError;
use lash_llm_transport::{HttpHeaderValue, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use opentelemetry::global;
use opentelemetry::propagation::Injector;

struct Headers<'a>(&'a mut Vec<(String, HttpHeaderValue)>);

impl Injector for Headers<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.push((key.to_owned(), HttpHeaderValue::new(value)));
    }
}

#[derive(Debug)]
struct Propagating(Arc<dyn LlmHttpTransport>);

#[async_trait::async_trait]
impl LlmHttpTransport for Propagating {
    async fn send(
        &self,
        mut request: LlmHttpRequest,
        timeout: Option<Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        // The current context is the provider request's `chat` span.
        global::get_text_map_propagator(|propagator| {
            propagator.inject(&mut Headers(&mut request.headers));
        });
        self.0.send(request, timeout).await
    }
}
```

Pass the wrapper to the provider's transport constructor:
`with_transport` on `OpenAiCompatibleProvider`, `OpenAiProvider`,
`AnthropicProvider` and `GoogleOAuthProvider` (whose `with_client` wraps a
client in the same transport), or `with_http_transport` on
`CodexProvider`. Codex's WebSocket path opens one cached connection
outside the transport and carries no per-request header; a host that needs
per-request propagation there pins `force_sse_transport()`.

## Retiring a process component

A process component can stop being installed while sealed process envs still
name it. Declare it retired on the core builder with the declaration it was
installed under:

```rust
let builder = builder.retire_process_component(
    lash::process::ProcessComponentDeclaration::initial("my-component", FormatVersion::ONE)?,
);
```

The node then serves the component's claim token at each format the
declaration reads, so it still claims the processes of a sealed env that names
the component. Such an env builds without the component, and
`core.processes().environment(id)` lists it under
`ProcessAuthority::Engine::retired`. A new env that names it is refused with
`ProcessEnvRefusal::ComponentRetired`, and a child process restricted from a
sealed env leaves it out. The build refuses a component that is both installed
and retired.

On replay, what the component decided before it was retired is already
recorded with each call's outcome: its hooks' and presenters' part in a call is
read from the record, not run again. A run that still depends on the component
does not resume without it. If the env links an effect to one of its providers,
every later call of that effect would have no body. If the component published
step state in the process, its later steps would find nothing to read. Either
way the process parks before any step resumes, with
`ProcessParkReason::EnvRefused` carrying
`ProcessEnvRefusal::RetiredComponentServes` or
`ProcessEnvRefusal::RetiredComponentHasState`. Retire only a component whose
sealed envs link none of its tools.


## Retiring a session plugin

Rebuild checks no required installed-plugin roster. A session can name a
plugin in these durable records:

- Its config head: the protocol plugin id and config-owner namespaces, including
  defaults. Config revisions and an undelivered config change retain those
  configurations too. Creation's `SessionSpec::plugin_options` accepts only
  registered config owners: an empty option for a configless plugin is refused
  with `ConfigRefusalReason::UnknownOwner`, even while it is installed.
- Plugin-state namespaces in session revisions and turn checkpoints, including
  each namespace's format stamp, values and publication receipts; state-command
  batches and their outcomes also name the addressed namespace. Plugin
  transitions retain writer admissions, namespace postimages and config.
- A run's recorded config and owner run options, and its linked tool surface:
  provider identities for ordinary tools and the deferred resolver.
- Recorded callback identities in hook, tool preparation and presentation
  decisions, opaque stream state and owed callbacks. Actor claim requirements
  carry the callbacks still needed by the run. Completed decisions remain
  history and are not executed again.
- The session prompt plan's section ids, and a model call's resolved prompt
  sections, wrappers and recorded text. Message origins, plugin events and
  plugin-produced turn outputs can also retain plugin provenance.
- Pending config-change delivery observers. An outstanding
  observer has its own `retire_config_observer` declaration; retiring its plugin
  does not retire that delivery.

A plugin that leaves none of these references needs no retirement. A session
created while an empty, configless plugin was installed already rebuilds
without it: rebuild uses the recorded config and state with the current
factories. An empty state namespace or a transition's recorded writer admission
does not require that factory to be installed. A host law that removes such a
plugin is therefore expected to pass without a retirement.
Fleet plugin-writer registrations are deployment records, not a session roster.

Declare a plugin permanently removed from this deployment by its stable id:

```rust
let builder = builder.retire_session_plugin(lash::plugins::PluginId::new("my-plugin"));
```

Recorded sessions keep their recorded configuration and rebuild without the
plugin. New sessions explicitly naming it refuse with the typed
`RetirementRefusal::Admission`. Installing and retiring the same session id,
or retiring it twice, refuses the core build. Session and process registries
are independent, including when their registrations share an id.

Recorded hook and presentation decisions remain recorded decisions. A run
whose recorded tool surface still links a provider of the retired plugin, or
whose plugin namespace holds nonempty state, parks before replay delivers
work. `SessionParkReason::PluginRetired` carries `RetirementRefusal::Serves`
or `RetirementRefusal::HasState`. Its input and checkpoint remain open for a
build that restores the plugin, or cancellation. Retirement serves only the
retired owner's callback claim tokens so the node can record this refusal;
it supplies no callback implementation.
