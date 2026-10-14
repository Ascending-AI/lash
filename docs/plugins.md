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
and reports each of these laws for each callback:

| Law | Evidence |
| --- | --- |
| REPLAY-INVOKES-NO-RECORDED-CALLBACK | Counting adapters on the recipe's binders, a recorded cut, runtime destruction and continuation of the same accepted input. |
| STATE-SURVIVES-REOPEN | Published namespaces, including unfinished-turn rows, compared after fresh materialization under the host's `RuntimeDataLimits`. |
| UNSERVED-IS-UNCLAIMABLE | A session actor owing the declared callback token, a node lacking that registration, `plugin_diagnostics().explain_claimability` and the real SQL claim predicate. |
| RECORDED-DECISIONS-STAY-ATTRIBUTABLE | `plugin_diagnostics().attribute` reads retained binding/presentation decisions after removing the factory. |

An executed law is `Passed` or `Failed` with a readable message.
`NotApplicable` has a typed reason for a law with no subject, while
`Unavailable` means applicable evidence or author input is missing. The verdict
is `Failed` if any law fails, `Partial` if an applicable law is unavailable,
and `Full` when every applicable law is exercised and passes. An exercised
callback that can publish state needs a scenario that publishes values:
an empty namespace yields `Unavailable { reason:
CertificationUnavailable::ScenarioDidNotPublish }`. Only a decision-only
contribution that ran no tool body and whose factory declares no state reducer
receives `NotApplicable { reason: CertificationNotApplicable::NoPublishedState }`,
after verifying that its namespace stayed empty on reopen.
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
                "code": "await tools.send_email({});"
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
The resolved definition carries its explicit `ToolBinding` and execution
bound. Hosts put real authorization and other preconditions in `prepare`,
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
