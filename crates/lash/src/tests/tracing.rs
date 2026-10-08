//! A host's turns write the runtime's trace records: the composition a model
//! call ran under, each tool call's ordered lifecycle pair, the model call's
//! stream and provider evidence, through a host's `send()` on the core's
//! node over SQLite memory stores (FIG-5307; the runtime laws FIG-5190
//! deleted with the engine double).

use super::*;
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};

use crate::support::TurnInput;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmProviderTraceEvent, LlmUsage, StreamBlockIdentity};
use lash_core::testing::runtime_helpers::{EchoTool, MockCall, mock_provider};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

/// A core whose trace records land in a JSONL file it owns.
struct Traced {
    core: LashCore,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Traced {
    async fn new(
        provider: ProviderHandle,
        configure: impl FnOnce(crate::core::LashCoreBuilder) -> crate::core::LashCoreBuilder,
    ) -> Result<Self> {
        Self::over(
            sqlite_memory_store_backend().await,
            crate::testing::runtime_lease_owner(),
            provider,
            configure,
        )
    }

    /// A core over `backend`, its own node under `owner`.
    fn over(
        backend: lash_core::Backend,
        owner: crate::persistence::LeaseOwnerIdentity,
        provider: ProviderHandle,
        configure: impl FnOnce(crate::core::LashCoreBuilder) -> crate::core::LashCoreBuilder,
    ) -> Result<Self> {
        let dir = tempfile::tempdir().expect("trace directory");
        let path = dir.path().join("trace.jsonl");
        let core = configure(
            explicit_ephemeral_facets(LashCore::standard_builder(backend))
                .serve_test_llm_profile(provider, mock_llm_profile_spec())
                .trace_jsonl_path(path.clone()),
        )
        .build(owner)?;
        Ok(Self {
            core,
            path,
            _dir: dir,
        })
    }

    async fn session(&self, id: &str) -> Result<crate::LashSession> {
        self.core
            .session(crate::SessionId::parse(id).expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await
    }

    /// Every record written so far.
    #[allow(
        clippy::disallowed_methods,
        reason = "the law reads back the trace file its own core wrote"
    )]
    fn entries(&self) -> Vec<serde_json::Value> {
        self.core.flush_trace_sink().expect("flush the trace sink");
        lash_trace::parse_jsonl_records::<serde_json::Value>(
            &std::fs::read_to_string(&self.path).unwrap_or_default(),
        )
        .expect("trace records")
    }
}

fn of_type<'a>(entries: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    entries
        .iter()
        .filter(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some(kind))
        .collect()
}

fn text_call(text: &str) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(text_response(text)),
    }
}

fn tool_call(call_id: &str, tool_name: &str, input_json: &str) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: call_id.to_owned(),
                tool_name: tool_name.to_owned(),
                input_json: input_json.to_owned(),
                replay: None,
            }],
            ..LlmResponse::default()
        }),
    }
}

fn completed(output: &crate::TurnOutput) -> bool {
    matches!(
        &output.result.outcome,
        crate::TurnOutcome::Finished(_) | crate::TurnOutcome::AgentFrameSwitch { .. }
    )
}

// ---- composition --------------------------------------------------------

/// The plugin whose initial-instructions section renders the text it holds.
#[derive(Clone)]
struct SwitchablePrompt(Arc<StdMutex<&'static str>>);

impl lash_core::plugin::PluginDefinition for SwitchablePrompt {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("switchable-prompt")
    }
}

impl PluginFactory for SwitchablePrompt {
    fn id(&self) -> &'static str {
        "switchable-prompt"
    }

    fn build(
        &self,
        _: &crate::plugins::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn crate::plugins::SessionPlugin>, crate::plugins::PluginError>
    {
        Ok(Arc::new(self.clone()))
    }
}

impl crate::plugins::SessionPlugin for SwitchablePrompt {
    fn id(&self) -> &'static str {
        "switchable-prompt"
    }

    fn register(
        &self,
        reg: &mut crate::plugins::PluginRegistrar,
    ) -> std::result::Result<(), crate::plugins::PluginError> {
        let text = Arc::clone(&self.0);
        reg.prompt().section(
            crate::plugins::PromptSectionSpec::new(
                crate::prompt::PromptSectionKey::new("switchable").expect("valid section key"),
                crate::prompt::PromptPlacement::InitialInstructions,
            ),
            Arc::new(move |_: &crate::plugins::PromptInput<'_>| {
                Ok::<_, crate::plugins::PromptRenderError>(crate::plugins::SectionText::text(
                    *text.lock_recover(),
                ))
            }),
        )
    }
}

fn composition_tool_contract<'a>(
    entry: &'a serde_json::Value,
    name: &str,
) -> &'a serde_json::Value {
    entry["tool_schemas"]
        .as_array()
        .expect("ordered tool schemas")
        .iter()
        .find(|tool| tool["name"] == name)
        .expect("named tool contract remains present")
}

/// The composition record is a snapshot written when the composition
/// changes: an identical composition writes none and serializes no schema,
/// a route's capacity is not composition, and a changed rendered prompt is.
#[ignore = "FIG-5353: a node-run turn writes no composition_changed record"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composition_trace_is_snapshot_on_change_and_ignores_route_capacity_noise() -> Result<()> {
    let prompt = Arc::new(StdMutex::new("the first rendered prompt"));
    let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
    let seen_by_provider = Arc::clone(&seen);
    let replies = Arc::new(StdMutex::new(std::collections::VecDeque::from([
        "first",
        "unchanged",
        "route noise",
        "prompt changed",
    ])));
    let provider = crate::testing::TestProvider::builder()
        .kind("mock")
        .complete(move |request| {
            seen_by_provider
                .lock_recover()
                .push(request.instructions.clone().unwrap_or_default().to_string());
            let reply = replies.lock_recover().pop_front().expect("scripted reply");
            async move { Ok(text_response(reply)) }
        })
        .build()
        .into_handle();
    let noise = llm_profile_spec("different-route", None, 150_000);
    let traced = Traced::new(provider.clone(), |builder| {
        builder
            .llm_profiles(Arc::new(
                lash_core::LlmProfileRegistry::new()
                    .register(
                        mock_llm_profile_spec().wire_model,
                        lash_core::RegisteredLlmProfile::new(
                            mock_llm_profile_spec(),
                            provider.clone(),
                        ),
                    )
                    .expect("the session's route registers")
                    .register(
                        "different-route",
                        lash_core::RegisteredLlmProfile::new(noise, provider),
                    )
                    .expect("the noise route registers"),
            ))
            .plugin(Arc::new(SwitchablePrompt(Arc::clone(&prompt))))
    })
    .await?;
    let session = traced.session("composition").await?;

    let serializations_before = lash_core::trace::composition_schema_serialization_count();
    session.send(TurnInput::text("first")).output().await?;
    let serializations_after_first = lash_core::trace::composition_schema_serialization_count();
    assert!(
        serializations_after_first > serializations_before,
        "the first composition fingerprints and materializes its tool contracts"
    );
    session.send(TurnInput::text("same")).output().await?;
    assert_eq!(
        lash_core::trace::composition_schema_serialization_count(),
        serializations_after_first,
        "an identical composition must not serialize schemas or allocate a fresh schema Vec"
    );
    let config = session.admin().config();
    let revision = config.revision().await?;
    let selected = config
        .apply(
            crate::config::ConfigWrite::new("route-noise", revision),
            crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("different-route"),
            }),
        )
        .await?
        .await_outcome(&config)
        .await?;
    assert!(
        matches!(
            selected,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{selected:?}"
    );
    session
        .send(TurnInput::text("route noise"))
        .output()
        .await?;
    *prompt.lock_recover() = "This text proves the rendered prompt changed.";
    session.send(TurnInput::text("changed")).output().await?;
    assert!(
        seen.lock_recover()
            .last()
            .is_some_and(|instructions| instructions
                .contains("This text proves the rendered prompt changed.")),
        "the changed section reaches the model's instructions: {:?}",
        seen.lock_recover()
    );

    let entries = traced.entries();
    let changes = of_type(&entries, "composition_changed");
    assert_eq!(
        changes.len(),
        2,
        "initial composition and one genuine prompt change emit exactly once: {changes:?}"
    );
    assert_ne!(changes[0]["fingerprint"], changes[1]["fingerprint"]);
    assert!(
        changes[1]["rendered_system_prompt"]
            .as_str()
            .is_some_and(|prompt| prompt.contains("This text proves the rendered prompt changed."))
    );
    assert_eq!(
        changes[0]["tool_schemas"], changes[1]["tool_schemas"],
        "a prompt-only change retains the complete ordered tool schemas"
    );
    traced.core.shutdown().await?;
    Ok(())
}

#[ignore = "FIG-5353: a node-run turn writes no composition_changed record"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composition_trace_fires_once_when_tool_membership_changes_with_full_ordered_schemas()
-> Result<()> {
    let traced = Traced::new(
        mock_provider(vec![
            text_call("tool present"),
            text_call("tool absent"),
            text_call("still absent"),
        ])
        .into_handle(),
        |builder| builder.tools(Arc::new(EchoTool)),
    )
    .await?;
    let session = traced.session("tool-membership").await?;

    session
        .send(TurnInput::text("tool-member"))
        .output()
        .await?;
    session
        .admin()
        .tools()
        .set_membership(
            lash_core::ToolId::from("tool:echo_tool"),
            false,
            "host:tracing:set_membership:319",
        )
        .await?
        .settle_with(
            &session.admin().commands(),
            crate::testing::admin_fixture_outcome,
        )
        .await?;
    session
        .send(TurnInput::text("tool-removed"))
        .output()
        .await?;
    session
        .send(TurnInput::text("tool-still-removed"))
        .output()
        .await?;

    let entries = traced.entries();
    let changes = of_type(&entries, "composition_changed");
    assert_eq!(
        changes.len(),
        2,
        "initial membership and one genuine removal emit exactly once: {changes:?}"
    );
    let initial_tools = changes[0]["tool_schemas"]
        .as_array()
        .expect("initial ordered tool schemas");
    let echo = composition_tool_contract(changes[0], "echo_tool");
    assert_eq!(echo["input_schema"]["canonical"]["required"][0], "value");
    assert!(echo["output_schema"]["canonical"].is_object());
    let changed_tools = changes[1]["tool_schemas"]
        .as_array()
        .expect("changed ordered tool schemas");
    assert_eq!(initial_tools.len(), changed_tools.len() + 1);
    assert!(
        changed_tools.iter().all(|tool| tool["name"] != "echo_tool"),
        "the changed snapshot must carry the complete catalog without the removed member"
    );
    assert_ne!(changes[0]["fingerprint"], changes[1]["fingerprint"]);
    traced.core.shutdown().await?;
    Ok(())
}

/// A member whose contract refreshes when its revision changes.
struct SchemaChangingTool {
    revision: StdMutex<u64>,
}

impl SchemaChangingTool {
    fn definition(&self) -> lash_core::ToolDefinition {
        let field = if *self.revision.lock_recover() == 1 {
            "first_value"
        } else {
            "second_value"
        };
        lash_core::ToolDefinition::raw(
            "tool:schema_changing",
            "schema_changing",
            "A stable member whose contract can refresh",
            serde_json::json!({
                "type": "object",
                "properties": { field: { "type": "string" } },
                "required": [field],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
    }
}

#[async_trait]
impl ToolProvider for SchemaChangingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "schema_changing").then(|| Arc::new(self.definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::Value::Null).into()
    }
}

#[ignore = "FIG-5353: a node-run turn writes no composition_changed record"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composition_trace_fires_once_when_same_member_tool_schema_changes() -> Result<()> {
    let tool = Arc::new(SchemaChangingTool {
        revision: StdMutex::new(1),
    });
    let tools = Arc::clone(&tool);
    let traced = Traced::new(
        mock_provider(vec![
            text_call("first schema"),
            text_call("second schema"),
            text_call("unchanged second schema"),
        ])
        .into_handle(),
        |builder| builder.tools(tools),
    )
    .await?;
    let session = traced.session("tool-schema").await?;

    session.send(TurnInput::text("schema-one")).output().await?;
    *tool.revision.lock_recover() = 2;
    let receipt = session
        .admin()
        .commands()
        .refresh_tool_catalog("schema changed", "schema-refresh")
        .await?;
    session.admin().commands().settle(receipt).await?;
    session.send(TurnInput::text("schema-two")).output().await?;
    session
        .send(TurnInput::text("schema-two-unchanged"))
        .output()
        .await?;

    let entries = traced.entries();
    let changes = of_type(&entries, "composition_changed");
    assert_eq!(changes.len(), 2, "one schema change emits exactly once");
    assert_eq!(
        composition_tool_contract(changes[0], "schema_changing")["input_schema"]["canonical"]["required"]
            [0],
        "first_value"
    );
    assert_eq!(
        composition_tool_contract(changes[1], "schema_changing")["input_schema"]["canonical"]["required"]
            [0],
        "second_value"
    );
    assert_ne!(changes[0]["fingerprint"], changes[1]["fingerprint"]);
    traced.core.shutdown().await?;
    Ok(())
}

// ---- spans --------------------------------------------------------------

/// The ancestry of each `provider.complete` span opened in this process.
#[derive(Clone, Default)]
struct ProviderSpans(Arc<StdMutex<Vec<Vec<String>>>>);

impl<S> Layer<S> for ProviderSpans
where
    S: ::tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        _attrs: &::tracing::span::Attributes<'_>,
        id: &::tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("new span present in registry");
        if span.metadata().name() == "provider.complete" {
            self.0.lock_recover().push(
                span.scope()
                    .skip(1)
                    .map(|ancestor| ancestor.metadata().name().to_owned())
                    .collect(),
            );
        }
    }
}

/// A provider's own spans nest under the span of the turn that called it,
/// so a host's provider instrumentation reads as part of that turn.
#[ignore = "FIG-5353: a node-run turn's provider work runs under no turn span"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_spans_are_children_of_the_turn_span() -> Result<()> {
    let capture = ProviderSpans::default();
    ::tracing::subscriber::set_global_default(Registry::default().with(capture.clone()))
        .expect("install capture subscriber");
    let provider = crate::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|_request| async {
            drop(::tracing::info_span!("provider.complete"));
            Ok(text_response("done"))
        })
        .build()
        .into_handle();
    let traced = Traced::new(provider, |builder| builder).await?;
    let session = traced.session("provider-span-parentage").await?;
    session.send(TurnInput::text("hello")).output().await?;
    traced.core.shutdown().await?;

    let ancestries = capture.0.lock_recover().clone();
    assert_eq!(ancestries.len(), 1, "{ancestries:?}");
    assert!(
        ancestries[0]
            .iter()
            .any(|ancestor| ancestor.contains("turn")),
        "the provider span must inherit the turn span; its ancestry: {ancestries:?}"
    );
    Ok(())
}

// ---- tool lifecycle -----------------------------------------------------

/// One call of `tool_name` with `input_json` writes exactly one ordered
/// `tool_call_started`/`tool_call_completed` record pair and one ordered
/// activity pair, both keyed by the call, the completion nested under the
/// turn as `tool:<lash call id>`.
async fn assert_standard_tool_lifecycle(
    call_id: &str,
    tool_name: &str,
    input_json: &str,
    expected_success: bool,
    plugins: Vec<Arc<dyn PluginFactory>>,
) -> Result<()> {
    let traced = Traced::new(
        mock_provider(vec![
            tool_call(call_id, tool_name, input_json),
            text_call("done"),
        ])
        .into_handle(),
        |builder| {
            plugins
                .into_iter()
                .fold(builder.tools(Arc::new(EchoTool)), |builder, plugin| {
                    builder.plugin(plugin)
                })
        },
    )
    .await?;
    let session = traced.session("trace-standard-tool").await?;
    let turn = session
        .send(TurnInput::text("call the tool"))
        .output()
        .await?;

    assert!(completed(&turn), "{:?}", turn.result.outcome);
    let calls = &turn.result.tool_calls;
    assert_eq!(calls.len(), 1, "one accounting record per call");
    assert_eq!(calls[0].provider_call_id.as_deref(), Some(call_id));
    assert_eq!(calls[0].tool, tool_name);
    assert_eq!(calls[0].output.is_success(), expected_success);

    let lash_call_id = turn
        .activities
        .iter()
        .find_map(|activity| match &activity.event {
            lash_core::TurnEvent::ToolCallStarted {
                call_id: lash_call_id,
                provider_call_id: Some(observed),
                ..
            } if observed == call_id => Some(lash_call_id.clone()),
            _ => None,
        })
        .expect("the started activity names lash's call id");
    let lifecycle = turn
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash_core::TurnEvent::ToolCallStarted {
                provider_call_id: Some(observed),
                ..
            } if observed == call_id => Some("started"),
            lash_core::TurnEvent::ToolCallCompleted {
                provider_call_id: Some(observed),
                ..
            } if observed == call_id => Some("completed"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        ["started", "completed"],
        "exactly one ordered activity pair keyed by {call_id}: {:?}",
        turn.activities
    );
    let expected_correlation = lash_core::TurnActivityId::new(format!("tool:{lash_call_id}"));
    assert!(
        turn.activities
            .iter()
            .filter(|activity| matches!(
                &activity.event,
                lash_core::TurnEvent::ToolCallStarted { provider_call_id: Some(observed), .. }
                    | lash_core::TurnEvent::ToolCallCompleted { provider_call_id: Some(observed), .. }
                    if observed == call_id
            ))
            .all(|activity| activity.correlation_id == expected_correlation),
        "tool activity correlation remains keyed by call id: {:?}",
        turn.activities
    );

    let entries = traced.entries();
    let keyed = |kind: &str| {
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.get("type").and_then(serde_json::Value::as_str) == Some(kind)
                    && entry
                        .get("provider_call_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(call_id)
            })
            .collect::<Vec<_>>()
    };
    let started = keyed("tool_call_started");
    let completions = keyed("tool_call_completed");
    assert_eq!(
        started.len(),
        1,
        "expected exactly one ToolCallStarted trace: {entries:?}"
    );
    assert_eq!(
        completions.len(),
        1,
        "expected exactly one ToolCallCompleted trace: {entries:?}"
    );
    assert_eq!(started[0].1["name"].as_str(), Some(tool_name));
    assert!(
        started[0].0 < completions[0].0,
        "Started must precede Completed: {entries:?}"
    );
    assert_eq!(
        completions[0].1["context"]["graph_node_id"].as_str(),
        Some(format!("tool:{lash_call_id}").as_str())
    );
    traced.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_runtime_emits_single_tool_call_trace_pair_per_call() -> Result<()> {
    // A successful prepared call keeps the one-pair contract: reporting must
    // not duplicate the start batch execution already wrote.
    assert_standard_tool_lifecycle(
        "call-success",
        "echo_tool",
        r#"{"value":"sample"}"#,
        true,
        Vec::new(),
    )
    .await
}

#[ignore = "FIG-5353: a node-run turn writes no tool_call_started or tool_call_completed record, and an unknown name gets no activity pair"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_tool_name_emits_an_ordered_lifecycle_pair() -> Result<()> {
    assert_standard_tool_lifecycle(
        "call-missing-name",
        "missing_tool",
        r#"{"value":1}"#,
        false,
        Vec::new(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_tool_arguments_emit_an_ordered_lifecycle_pair() -> Result<()> {
    assert_standard_tool_lifecycle(
        "call-invalid-args",
        "echo_tool",
        r#"{"other":true}"#,
        false,
        Vec::new(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn before_tool_hook_refusal_emits_an_ordered_lifecycle_pair() -> Result<()> {
    let refusal: Arc<dyn PluginFactory> = Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("tool-refusal"),
        lash_core::facade_support::PluginSpec::new().with_tool_args_check(
            crate::hook_key!("refuse"),
            Arc::new(|_input| {
                Box::pin(async {
                    Ok(lash_core::plugin::BeforeToolDecision::Deny(
                        lash_core::ToolFailure::tool(
                            lash_core::ToolFailureClass::PermissionDenied,
                            "refused",
                            "refused by test hook",
                        ),
                    ))
                })
            }),
        ),
    ));
    assert_standard_tool_lifecycle(
        "call-hook-refusal",
        "echo_tool",
        r#"{"value":"blocked"}"#,
        false,
        vec![refusal],
    )
    .await
}

/// An `echo_tool` whose completion the law resolves out of band.
struct PendingEchoTool(tokio::sync::mpsc::UnboundedSender<(lash_core::PinnedKey, String)>);

#[async_trait]
impl ToolProvider for PendingEchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![pending_echo_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "echo_tool").then(|| Arc::new(pending_echo_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        let value = call
            .args
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let _ = self.0.send((key, value));
        lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into()
    }
}

fn pending_echo_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:echo_tool",
        "echo_tool",
        "Return a tool payload",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_declaration(lash_core::ToolDeclaration::deferring())
    .with_park(lash_core::ParkBound::Within(
        std::time::Duration::from_secs(120),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_then_resolved_tool_call_emits_one_completion_per_channel() -> Result<()> {
    let call_id = "call-pending";
    let (keys, mut published) = tokio::sync::mpsc::unbounded_channel();
    let traced = Traced::new(
        mock_provider(vec![
            tool_call(call_id, "echo_tool", r#"{"value":"pending"}"#),
            text_call("done"),
        ])
        .into_handle(),
        |builder| builder.tools(Arc::new(PendingEchoTool(keys))),
    )
    .await?;
    let session = traced.session("pending-tool").await?;
    let turn = tokio::spawn(
        session
            .send(TurnInput::text("call the pending tool"))
            .output(),
    );
    let (key, value) = published.recv().await.expect("the call publishes its key");
    assert_eq!(
        traced
            .core
            .completions()
            .resolve(
                key.as_str(),
                lash_core::Resolution::Ok(serde_json::json!({ "payload": format!("raw:{value}") })),
            )
            .await?,
        lash_core::ResolveAnswer::Resolved
    );
    let turn = turn.await.expect("the turn task")?;

    let calls = &turn.result.tool_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].provider_call_id.as_deref(), Some(call_id));
    let entries = traced.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| {
                entry.get("type").and_then(serde_json::Value::as_str) == Some("tool_call_completed")
                    && entry
                        .get("provider_call_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(call_id)
            })
            .count(),
        1,
        "pending resolution owns exactly one trace completion: {entries:?}"
    );
    let completions = turn
        .activities
        .iter()
        .filter(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::ToolCallCompleted { provider_call_id: Some(observed), .. }
                    if observed == call_id
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completions.len(),
        1,
        "pending resolution owns exactly one activity completion: {:?}",
        turn.activities
    );
    assert_eq!(
        completions[0].correlation_id,
        lash_core::TurnActivityId::new(format!("tool:{}", calls[0].call_id))
    );
    traced.core.shutdown().await?;
    Ok(())
}

// ---- model calls --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_runtime_trace_records_stream_event_entries() -> Result<()> {
    let usage = LlmUsage {
        input_tokens: 10,
        output_tokens: 2,
        ..LlmUsage::default()
    };
    let traced = Traced::new(
        mock_provider(vec![MockCall {
            stream_events: vec![
                LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: "Hello ".to_owned(),
                }),
                LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: "world".to_owned(),
                }),
                LlmStreamEvent::Part(LlmOutputPart::Text {
                    text: "Hello world".to_owned(),
                    response_meta: None,
                }),
                LlmStreamEvent::Usage(usage),
            ],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "Hello world".to_owned(),
                    response_meta: None,
                }],
                execution_evidence: Some(lash_core::ExecutionEvidence {
                    served_model: Some("served-model".to_owned()),
                    provider_response_id: Some("provider-response-1".to_owned()),
                    reasoning_output_tokens: Some(0),
                    provider_finish_reason: Some("stop".to_owned()),
                    ..lash_core::ExecutionEvidence::default()
                }),
                ..LlmResponse::default()
            }),
        }])
        .into_handle(),
        |builder| builder.trace_level(lash_trace::TraceLevel::Extended),
    )
    .await?;
    let session = traced.session("trace-stream-events").await?;
    let turn = session.send(TurnInput::text("hello")).output().await?;

    assert!(completed(&turn), "{:?}", turn.result.outcome);
    let attempt_evidence = turn.result.llm_calls[0].attempts[0]
        .evidence
        .as_ref()
        .expect("response evidence reaches the sealed attempt ledger");
    assert_eq!(
        attempt_evidence.provider_response_id.as_deref(),
        Some("provider-response-1")
    );

    let entries = traced.entries();
    let stream_events = of_type(&entries, "runtime_stream_event");
    assert!(
        stream_events.iter().any(|entry| {
            let payload = &entry["event"]["payload"];
            payload["type"] == "block"
                && payload["event"]["phase"] == "delta"
                && payload["event"]["kind"] == "assistant_text"
                && payload["event"]["block"]["ordinal"].is_u64()
                && payload["raw_text"] == "Hello "
        }),
        "expected the block delta, with its full identity, in trace: {entries:?}"
    );
    assert!(
        stream_events.iter().any(|entry| {
            let payload = &entry["event"]["payload"];
            payload["type"] == "text_part" && payload["text"] == "Hello world"
        }),
        "expected text_part stream event in trace: {entries:?}"
    );
    let response_entry = *of_type(&entries, "llm_call_completed")
        .first()
        .expect("completed llm call entry");
    assert_eq!(
        response_entry["response"]["request_model"].as_str(),
        Some("mock-model")
    );
    assert_eq!(
        response_entry["attempts"][0]["evidence"]["served_model"].as_str(),
        Some("served-model")
    );
    assert_eq!(
        response_entry["attempts"][0]["evidence"]["reasoning_output_tokens"].as_u64(),
        Some(0)
    );
    let stream_summary = response_entry["stream_summary"]
        .as_object()
        .expect("stream summary");
    assert_eq!(stream_summary["text_delta_count"].as_u64(), Some(2));
    assert_eq!(stream_summary["visible_chunk_count"].as_u64(), Some(2));
    assert_eq!(stream_summary["max_visible_chunk_chars"].as_u64(), Some(6));
    let avg_chunk_chars = stream_summary["avg_visible_chunk_chars"]
        .as_f64()
        .expect("avg visible chunk chars");
    assert!((avg_chunk_chars - 5.5).abs() < f64::EPSILON);
    assert!(!stream_summary["first_visible_token_latency_ms"].is_null());
    assert!(!stream_summary["stream_duration_ms"].is_null());
    traced.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extended_runtime_trace_records_provider_request_and_stream_events() -> Result<()> {
    let provider = crate::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            if let Some(tx) = request.provider_trace.as_ref() {
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "responses",
                    serde_json::json!({
                        "model": "mock-model",
                        "input": "x".repeat(40),
                    })
                    .to_string(),
                ));
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "chat/completions",
                    r#"{"model":"small"}"#.to_owned(),
                ));
                tx.send(LlmProviderTraceEvent::request(
                    "mock",
                    "invalid",
                    "not-json".to_owned(),
                ));
                for (index, id) in ["msg_1", "msg_2"].into_iter().enumerate() {
                    tx.send(LlmProviderTraceEvent::response(
                        "mock",
                        "response.output_item.done".to_owned(),
                        serde_json::json!({
                            "type": "response.output_item.done",
                            "output_index": index,
                            "item": { "id": id }
                        })
                        .to_string(),
                    ));
                }
            }
            Ok(text_response("Hello"))
        })
        .build()
        .into_handle();
    let traced = Traced::new(provider, |builder| {
        builder
            .trace_level(lash_trace::TraceLevel::Extended)
            .trace_limits(crate::tracing::TraceLimits {
                provider_request_body_json_bytes: 32,
                ..Default::default()
            })
    })
    .await?;
    let session = traced.session("trace-provider-stream").await?;
    let turn = session.send(TurnInput::text("hello")).output().await?;
    assert!(completed(&turn), "{:?}", turn.result.outcome);

    let entries = traced.entries();
    // Both directions are one event kind; the typed direction tells them apart.
    let observations = of_type(&entries, "provider_event");
    let in_direction = |direction: &str| {
        observations
            .iter()
            .filter(|entry| entry["event"]["direction"]["direction"] == direction)
            .cloned()
            .collect::<Vec<_>>()
    };
    let provider_requests = in_direction("request");
    assert_eq!(provider_requests.len(), 3, "provider traces: {entries:?}");
    let request_event = &provider_requests
        .iter()
        .find(|entry| entry["event"]["direction"]["endpoint"] == "responses")
        .expect("large provider request")["event"];
    let expected_serialized = serde_json::json!({
        "model": "mock-model",
        "input": "x".repeat(40),
    })
    .to_string();
    assert_eq!(request_event["provider"], "mock");
    assert!(request_event.get("raw_json").is_none());
    assert_eq!(request_event["raw_json_omitted_reason"], "size_limit");
    assert_eq!(request_event["raw_len"], expected_serialized.len());
    assert!(request_event["raw_len"].as_u64().expect("body length") > 32);
    assert_eq!(
        request_event["raw_sha256"],
        lash_trace::sha256_hex(expected_serialized.as_bytes())
    );
    let small_request = &provider_requests
        .iter()
        .find(|entry| entry["event"]["direction"]["endpoint"] == "chat/completions")
        .expect("small provider request")["event"];
    assert_eq!(small_request["raw_json"]["model"], "small");
    assert!(small_request.get("raw_json_omitted_reason").is_none());
    let invalid_request = &provider_requests
        .iter()
        .find(|entry| entry["event"]["direction"]["endpoint"] == "invalid")
        .expect("invalid provider request")["event"];
    assert!(invalid_request.get("raw_json").is_none());
    assert_eq!(invalid_request["raw_json_omitted_reason"], "invalid_json");
    assert_eq!(invalid_request["raw_len"], "not-json".len());
    assert_eq!(
        invalid_request["raw_sha256"],
        lash_trace::sha256_hex(b"not-json")
    );
    let provider_events = in_direction("response");
    assert_eq!(
        provider_events.len(),
        2,
        "provider trace entries: {entries:?}"
    );
    assert_eq!(
        provider_events[0]["event"]["direction"]["event_name"],
        "response.output_item.done"
    );
    assert_eq!(provider_events[0]["event"]["item_id"], "msg_1");
    assert_eq!(provider_events[0]["event"]["output_index"], 0);
    assert_eq!(provider_events[1]["event"]["item_id"], "msg_2");
    assert_eq!(
        provider_events[1]["event"]["raw_json"]["item"]["id"],
        "msg_2"
    );
    assert!(
        provider_events[1]["event"]["raw_sha256"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty())
    );
    traced.core.shutdown().await?;
    Ok(())
}

/// A provider gets a request trace sender only at the extended level with a
/// sink to write to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_request_trace_sender_requires_extended_level_and_sink() -> Result<()> {
    fn sender_absent() -> ProviderHandle {
        crate::testing::TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(|request| async move {
                assert!(request.provider_trace.is_none());
                Ok(text_response("Hello"))
            })
            .build()
            .into_handle()
    }
    // The standard level with a sink.
    let traced = Traced::new(sender_absent(), |builder| builder).await?;
    let session = traced.session("standard-trace-level").await?;
    let turn = session.send(TurnInput::text("hello")).output().await?;
    assert!(completed(&turn), "{:?}", turn.result.outcome);
    traced.core.shutdown().await?;

    // The extended level with no sink.
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(sender_absent(), mock_llm_profile_spec())
    .trace_level(lash_trace::TraceLevel::Extended)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("extended-without-sink").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let turn = session.send(TurnInput::text("hello")).output().await?;
    assert!(completed(&turn), "{:?}", turn.result.outcome);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_runtime_trace_omits_stream_event_entries_by_default() -> Result<()> {
    let traced = Traced::new(
        mock_provider(vec![MockCall {
            stream_events: vec![
                LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: "Hello ".to_owned(),
                }),
                LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: "world".to_owned(),
                }),
            ],
            response: Ok(text_response("Hello world")),
        }])
        .into_handle(),
        |builder| builder,
    )
    .await?;
    let session = traced.session("trace-standard").await?;
    let turn = session.send(TurnInput::text("hello")).output().await?;
    assert!(completed(&turn), "{:?}", turn.result.outcome);

    let entries = traced.entries();
    assert!(
        of_type(&entries, "runtime_stream_event").is_empty(),
        "stream event entries should be opt-in: {entries:?}"
    );
    let response_entry = *of_type(&entries, "llm_call_completed")
        .first()
        .expect("completed llm call entry");
    assert!(
        response_entry
            .get("stream_summary")
            .is_some_and(|value| !value.is_null()),
        "stream summary should remain in completed LLM trace: {response_entry:?}"
    );
    traced.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_runtime_trace_records_failed_llm_calls() -> Result<()> {
    let traced = Traced::new(
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Err(LlmTransportError::new("HTTP request failed: builder error")
                .with_code(crate::provider::FailureCode::provider("builder"))
                .with_raw("transport raw body")
                .with_request_body("{\"model\":\"mock-model\"}")),
        }])
        .into_handle(),
        |builder| builder,
    )
    .await?;
    let session = traced.session("trace-failed-llm").await?;
    let turn = session.send(TurnInput::text("hello")).output().await?;

    assert!(
        matches!(&turn.result.outcome, crate::TurnOutcome::Stopped(_)),
        "{:?}",
        turn.result.outcome
    );
    // A durable report is thin: the failure is the call's attempt record,
    // which keeps the code and no raw body.
    let failure = turn.result.llm_calls[0].attempts[0]
        .error
        .as_ref()
        .expect("the attempt records its failure");
    assert_eq!(
        failure.code.as_ref().map(|code| code.namespaced()),
        Some("provider:builder".to_owned())
    );

    let entries = traced.entries();
    let error_entry = *of_type(&entries, "llm_call_failed")
        .first()
        .expect("llm error entry");
    assert!(error_entry["error"].get("message").is_none());
    assert_eq!(
        error_entry["error"]["code"].as_str(),
        Some("provider:builder")
    );
    assert!(error_entry["error"].get("raw").is_none());
    let request_entry = *of_type(&entries, "llm_call_started")
        .first()
        .expect("llm request entry");
    assert_eq!(
        request_entry["request"]["model"].as_str(),
        Some("mock-model")
    );
    traced.core.shutdown().await?;
    Ok(())
}

// ---- the tree across nodes ----------------------------------------------

/// The span tree a run's records build: each record's type and the
/// `(graph_node_id, parent_graph_node_id)` edge a consumer nests it by, with
/// the record's identity last.
fn span_tree(entries: &[serde_json::Value]) -> Vec<[String; 4]> {
    let field = |entry: &serde_json::Value, pointer: &str| {
        entry
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let mut tree = entries
        .iter()
        .filter(|entry| {
            let kind = field(entry, "/type");
            ["turn_", "tool_call_", "llm_call_"]
                .iter()
                .any(|prefix| kind.starts_with(prefix))
        })
        .map(|entry| {
            [
                field(entry, "/type"),
                field(entry, "/context/graph_node_id"),
                field(entry, "/context/parent_graph_node_id"),
                field(entry, "/id"),
            ]
        })
        .collect::<Vec<_>>();
    tree.sort();
    tree
}

/// One turn that parks on a pending tool and finishes on its resolution:
/// the session, the turn and the scripted calls every run of the law uses.
const TREE_SESSION: &str = "golden-tree";
const TREE_TURN: &str = "golden-tree-turn";

fn tree_provider() -> ProviderHandle {
    mock_provider(vec![
        tool_call("call-tree", "echo_tool", r#"{"value":"tree"}"#),
        text_call("done"),
    ])
    .into_handle()
}

async fn resolve_tree_key(core: &LashCore, key: &lash_core::PinnedKey) -> Result<()> {
    assert_eq!(
        core.completions()
            .resolve(
                key.as_str(),
                lash_core::Resolution::Ok(serde_json::json!({ "payload": "raw:tree" })),
            )
            .await?,
        lash_core::ResolveAnswer::Resolved
    );
    Ok(())
}

/// A turn's span tree is the golden tree after a kill and resume on another
/// node. The golden tree is the turn run on one node: it parks on a pending
/// tool and finishes when the host resolves it. The same turn on node A is
/// cut while it is parked (A shuts down), and node B over the same stores
/// takes the resolution and finishes it. The records A and B wrote together
/// build exactly the golden tree: every record once, under the parent it
/// has in the golden run, the tool's records under the turn and the model
/// calls' under the turn.
#[ignore = "FIG-5353: a node-run turn writes no turn_started, turn_completed or tool_call record"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn golden_tree_survives_a_kill_and_resume_on_another_node() -> Result<()> {
    let send = |session: &crate::LashSession| {
        session
            .send(TurnInput::text("call the pending tool"))
            .id(crate::TurnId::parse(TREE_TURN).expect("nonblank host identity"))
            .output()
    };

    // The golden run: one node from the send to the turn's end.
    let (keys, mut published) = tokio::sync::mpsc::unbounded_channel();
    let golden = Traced::new(tree_provider(), |builder| {
        builder.tools(Arc::new(PendingEchoTool(keys)))
    })
    .await?;
    let session = golden.session(TREE_SESSION).await?;
    let turn = tokio::spawn(send(&session));
    let (key, _) = published.recv().await.expect("the call publishes its key");
    resolve_tree_key(&golden.core, &key).await?;
    assert!(completed(&turn.await.expect("the golden turn joins")?));
    let golden_tree = span_tree(&golden.entries());
    golden.core.shutdown().await?;
    // The cut run: parked on node A, resumed and finished on node B.
    let stores = sqlite_memory_store_set().await;
    let (keys, mut published) = tokio::sync::mpsc::unbounded_channel();
    let node_a = Traced::over(
        lash_conformance::backend_over(Arc::clone(&stores) as Arc<dyn lash_core::StoreSet>),
        crate::persistence::LeaseOwnerIdentity::opaque("golden-tree", "node-a"),
        tree_provider(),
        |builder| builder.tools(Arc::new(PendingEchoTool(keys))),
    )?;
    let session = node_a.session(TREE_SESSION).await?;
    let parked = tokio::spawn(send(&session));
    let (key, _) = published.recv().await.expect("the call publishes its key");
    node_a.core.shutdown().await?;
    drop(parked);
    let before = span_tree(&node_a.entries());

    let (keys, _) = tokio::sync::mpsc::unbounded_channel();
    let node_b = Traced::over(
        lash_conformance::backend_over(Arc::clone(&stores) as Arc<dyn lash_core::StoreSet>),
        crate::persistence::LeaseOwnerIdentity::opaque("golden-tree", "node-b"),
        mock_provider(vec![text_call("done")]).into_handle(),
        |builder| builder.tools(Arc::new(PendingEchoTool(keys))),
    )?;
    let session = node_b
        .core
        .session(crate::SessionId::parse(TREE_SESSION).expect("nonblank host identity"))
        .open()
        .await?;
    resolve_tree_key(&node_b.core, &key).await?;
    let output = session
        .attach_id(crate::TurnId::parse(TREE_TURN).expect("nonblank host identity"))
        .output()
        .await?;
    assert!(completed(&output), "{:?}", output.result.outcome);
    let mut cut_tree = before;
    cut_tree.extend(span_tree(&node_b.entries()));
    cut_tree.sort();
    node_b.core.shutdown().await?;
    let edges = |tree: &[[String; 4]]| {
        tree.iter()
            .map(|[kind, node, parent, _]| [kind.clone(), node.clone(), parent.clone()])
            .collect::<Vec<_>>()
    };
    assert_eq!(
        edges(&cut_tree),
        edges(&golden_tree),
        "the records nodes A and B wrote build the golden tree"
    );
    assert_eq!(
        cut_tree
            .iter()
            .map(|record| &record[3])
            .collect::<std::collections::HashSet<_>>()
            .len(),
        cut_tree.len(),
        "no record identity repeats across the cut"
    );
    for kind in [
        "turn_started",
        "turn_completed",
        "tool_call_started",
        "tool_call_completed",
        "llm_call_completed",
    ] {
        assert!(
            golden_tree.iter().any(|record| record[0] == kind),
            "the golden tree has a `{kind}` record: {golden_tree:#?}"
        );
    }
    let turn_node = golden_tree
        .iter()
        .find(|record| record[0] == "turn_started")
        .map(|record| record[1].clone())
        .expect("the turn's span");
    for record in golden_tree
        .iter()
        .filter(|record| record[0].starts_with("tool_call_") || record[0].starts_with("llm_call_"))
    {
        assert_eq!(
            record[2], turn_node,
            "a tool or model call nests under its turn: {record:?}"
        );
    }
    Ok(())
}

/// A host's warning threshold affects a real publication without changing refusal ceilings.
#[tokio::test]
async fn facade_plugin_state_warning_threshold_takes_effect() -> Result<()> {
    let plugin = crate::plugins::StaticPluginFactory::new(
        crate::plugins::PluginDeclaration::initial("capacity-warning"),
        crate::plugins::PluginSpec::new().with_after_turn(
            crate::hook_key!("state"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(crate::plugins::AfterTurnContributions {
                        state: crate::plugins::StateCommands::new()
                            .set("key", serde_json::json!("value")),
                        ..Default::default()
                    })
                })
            }),
        ),
    );
    let traced = Traced::new(
        mock_provider(vec![text_call("done")]).into_handle(),
        |builder| {
            builder
                .plugin(Arc::new(plugin))
                .trace_limits(crate::tracing::TraceLimits {
                    plugin_state_warn_bytes: 1,
                    ..Default::default()
                })
        },
    )
    .await?;
    let session = traced.session("capacity-warning").await?;
    let (turn, capture) = lash_core::testing::trace_capture::capturing(|| async {
        session.send(TurnInput::text("publish")).output().await
    })
    .await;
    assert!(completed(&turn?));
    assert_eq!(
        capture
            .exactly_one("plugin_state.session_budget_warn")
            .field("warn"),
        "1"
    );
    traced.core.shutdown().await?;
    Ok(())
}
