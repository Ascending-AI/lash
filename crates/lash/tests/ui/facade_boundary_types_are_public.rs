use std::sync::Arc;

use async_trait::async_trait;
use lash::SessionId;
use lash::TurnId;
use lash::direct::{
    DirectLlmClient, DirectLlmError, DirectLlmOutcome, DirectRequest, GenerationOptionOutcome,
    GenerationOptions, GenerationReceipt, LlmEventSender, LlmOutputPart, LlmUsage,
    NonNegativeFiniteF64, NonNegativeFiniteF64Error,
};
use lash::durability::RuntimeHostConfig;
use lash::messages::MessageRole;
use lash::persistence::{
    CheckpointAdmission, CheckpointAdmissionRequest, GraphAppend, IngressSettlement, OperationId,
    PersistedSessionConfig, RealizedNodeTimestamp, RuntimeCommit, RuntimeCommitReceipt,
    RuntimeSessionState, RuntimeStore, RuntimeTurnCommitStamp, SessionCommitStore, SessionHeadMeta,
    SessionHeadPayload, StoreError, TurnInputCheckpointBoundary, TurnInputIngress, TurnInputState,
    commit_runtime_state_verified,
};
use lash::plugins::{
    AfterToolContributions, AfterToolDecision, BeforeToolDecision, CompactionContext,
    ContextCompaction, ContextCompactor, ContextError, PluginHost, PluginSpec, PluginSpecBuilder,
    PluginSpecFactory, ToolArgsCheckHook, ToolArgsCheckInput, ToolArgsTransformHook,
    ToolArgsTransformInput, ToolCatalogContribution, ToolResultCheckHook, ToolResultCheckInput,
    ToolResultTransformHook, ToolResultTransformInput,
};
use lash::provider::{ProviderRateLimitPolicy, ProviderReliability, ProviderRetryPolicy};
use lash::tools::{ToolCallRecord, ToolOutputContract};
use lash::turn::{TurnFailureCode, TurnFailureKind, TurnIssue};
use lash::{
    EmptyLlmProfiles, LlmProfileConfig, LlmProfileKey, LlmProfileLimits, LlmProfileMetadata,
    LlmProfileRegistry, LlmProfileUnavailable, LlmProfileUnavailableReason, LlmProfiles,
    RecordedLlmProfile, RegisteredLlmProfile, RegistrationError, RunResolveError, SpecResolveError,
};

fn persistence_types_are_nameable(graph: GraphAppend) -> RuntimeCommit {
    let operation = OperationId::turn("facade", "turn", "final");
    RuntimeCommit {
        session_id: SessionId::from("facade"),
        expected_head_revision: 0,
        run_terminal: None,
        trace: None,
        config: PersistedSessionConfig::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
            lash_core::SessionToolAccess::ambient(),
        ),
        execution_config: None,
        frame_transition: None,
        graph,
        graph_base_leaf_node_id: None,
        checkpoint: Default::default(),
        adopted_intent_rows: 0,
        failure_evidence: Vec::new(),
        outcome: None,
        turn_commit: RuntimeTurnCommitStamp::new(operation),
        ingress: None::<IngressSettlement>,
        applied_commands: None,
        command_outcomes: Default::default(),
        committed_attachment_ids: Vec::new(),
        commit_budget: lash::CommitBudget::bounded(1024 * 1024, 512),
    }
}

fn plugin_current_frame_is_borrowed(
    view: &lash::persistence::SessionReadView,
) -> Option<&lash::plugins::FrameNodeId> {
    view.current_frame()
}

fn plugin_types_are_nameable() -> PluginHost {
    let normalize: ToolArgsTransformHook =
        Arc::new(|input: ToolArgsTransformInput| Box::pin(async move { Ok(input.current) }));
    let policy: ToolArgsCheckHook = Arc::new(|input: ToolArgsCheckInput| {
        let _ = (input.prepared.call_id(), input.prepared.args());
        Box::pin(async move { Ok(BeforeToolDecision::Allow) })
    });
    let recover: ToolResultTransformHook =
        Arc::new(|input: ToolResultTransformInput| Box::pin(async move { Ok(input.current) }));
    let audit: ToolResultCheckHook = Arc::new(|input: ToolResultCheckInput| {
        let _ = input.final_result.outcome.clone();
        Box::pin(async move { Ok(AfterToolContributions::from(AfterToolDecision::Allow)) })
    });
    let builder: PluginSpecBuilder = Arc::new(move |_ctx| {
        Ok(PluginSpec::new()
            .with_tool_args_transform(lash::hook_key!("normalize"), Arc::clone(&normalize))
            .with_tool_args_check(lash::hook_key!("policy"), Arc::clone(&policy))
            .with_tool_result_transform(lash::hook_key!("recover"), Arc::clone(&recover))
            .with_tool_result_check(lash::hook_key!("audit"), Arc::clone(&audit)))
    });
    PluginHost::new(vec![Arc::new(PluginSpecFactory::new(
        lash::plugins::PluginDeclaration::initial("facade"),
        builder,
    ))])
}

struct FacadeCompactor;

#[async_trait]
impl ContextCompactor for FacadeCompactor {
    fn id(&self) -> &'static str {
        "facade.compactor"
    }

    async fn compact(
        &self,
        _ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError> {
        Ok(Some(ContextCompaction::default()))
    }
}

fn context_compactor_types_are_nameable() -> PluginSpec {
    PluginSpec::new().with_context_compactor(10, Arc::new(FacadeCompactor))
}

async fn direct_response_type_is_nameable(
    client: &mut DirectLlmClient,
    request: DirectRequest,
) -> Result<DirectLlmOutcome, DirectLlmError> {
    client.complete(request).await
}

fn direct_payload_types_are_nameable(
    attachment: lash::attachments::AttachmentRef,
    event_sender: LlmEventSender,
    output: LlmOutputPart,
    usage: LlmUsage,
) {
    let _ = (attachment, event_sender, output, usage);
}

fn generation_option_types_are_nameable(
    mut request: DirectRequest,
    temperature: f64,
) -> Result<GenerationOptions, NonNegativeFiniteF64Error> {
    request.generation.temperature = Some(NonNegativeFiniteF64::new(temperature)?);
    request.generation.seed = Some(1);
    Ok(request.generation)
}

fn generation_disposition_is_readable(
    response: lash::direct::DirectLlmOutcome,
) -> Option<(GenerationReceipt, bool)> {
    let disposition = response.generation_disposition?;
    let requested_temperature_survived =
        disposition.temperature == GenerationOptionOutcome::Applied;
    Some((disposition, requested_temperature_survived))
}

fn builder_accepts_tools_and_plugins(
    builder: lash::LashCoreBuilder,
    tools: Arc<dyn lash::tools::ToolProvider>,
    plugin: Arc<dyn lash::plugins::PluginFactory>,
) -> lash::LashCoreBuilder {
    builder.tools(tools).plugin(plugin)
}

fn a_core_is_built_over_one_backend(backend: lash::Backend) -> lash::LashCoreBuilder {
    lash::LashCore::standard_builder(backend)
}

fn tool_contract_types_are_nameable(record: ToolCallRecord, contract: ToolOutputContract) {
    let _ = (record, contract);
}

fn tool_catalog_types_are_nameable(
    contribution: ToolCatalogContribution,
    module: lash::tools::ToolModule,
) {
    let _ = (contribution, module);
}

fn message_role_type_is_nameable(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Event => "event",
    }
}

fn turn_result_detail_types_are_nameable(issue: TurnIssue) {
    let _ = issue;
}

fn turn_failure_vocabulary_is_nameable(kind: TurnFailureKind, code: TurnFailureCode) {
    let _ = (kind, code);
}

fn provider_reliability_types_are_nameable(
    reliability: ProviderReliability,
    retry: ProviderRetryPolicy,
    rate_limits: ProviderRateLimitPolicy,
) {
    let _ = (reliability, retry, rate_limits);
}

fn model_types_are_nameable(
    metadata: LlmProfileMetadata,
    limits: LlmProfileLimits,
    key: LlmProfileKey,
    recorded: RecordedLlmProfile,
    config: LlmProfileConfig,
    registry: LlmProfileRegistry,
    entry: RegisteredLlmProfile,
) {
    let _: &dyn LlmProfiles = &registry;
    let _: &dyn LlmProfiles = &EmptyLlmProfiles;
    let _ = (metadata, limits, key, recorded, config, entry);
}

fn model_errors_are_nameable(
    unavailable: LlmProfileUnavailable,
    reason: LlmProfileUnavailableReason,
    registration: RegistrationError,
    spec_error: SpecResolveError,
    run_error: RunResolveError,
) {
    let _ = (unavailable, reason, registration, spec_error, run_error);
}

fn cancellation_token_is_at_root(token: lash::CancellationToken, session: &lash::LashSession) {
    token.cancel();
    let _: lash::CancelBuilder = session.cancel(lash::CancelTarget::Run("turn".into()));
    let _ = lash::CancelReceipt::Withdrawn {
        run: "turn".into(),
        input: Some("input".into()),
    };
    let _ = lash::CancelReceipt::Cancelled {
        run: "turn".into(),
        receipt: Box::new(lash::TurnCancelReceipt {
            outcome: lash::TurnCancelOutcome::UnknownOrRevoked,
        }),
    };
    let _ = lash::CancelReceipt::UnknownOrRevoked;
}

fn turn_input_ingress_types_are_nameable(
    ingress: TurnInputIngress,
    boundary: TurnInputCheckpointBoundary,
    state: TurnInputState,
) {
    let _ = (ingress, boundary, state);
}

async fn pending_turn_input_cancel_facade_is_nameable(
    session: &lash::LashSession,
    target: lash::PendingTurnInputCancelTarget,
) -> lash::Result<()> {
    let _: Vec<lash::PendingTurnInputCancelReceipt> = session
        .durable()
        .cancel_pending_turn_inputs(vec![target.clone()])
        .await?;
    let _: lash::PendingTurnInputSuffixCancelOutcome = session
        .durable()
        .cancel_pending_turn_input_suffix(target)
        .await?;
    Ok(())
}

fn observation_types_are_homed_in_observe(
    cursor: lash::observe::SessionCursor,
    observation: lash::observe::SessionObservation,
    resume: lash::observe::SessionResume,
    revision: lash::observe::SessionRevision,
) {
    let _ = (cursor, observation, resume, revision);
}

async fn persistence_load_helpers_are_nameable(
    store: &lash::persistence::SessionStore,
) -> Result<Option<RuntimeSessionState>, StoreError> {
    Ok(lash::persistence::load_session_window_state(
        store,
        lash::persistence::WindowSelector::Current,
    )
    .await?
    .map(|loaded| loaded.state))
}

async fn verified_commit_chokepoint_is_nameable(
    store: &dyn SessionCommitStore,
    commit: RuntimeCommit,
) -> Result<RuntimeCommitReceipt, StoreError> {
    commit_runtime_state_verified(store, commit, &Default::default()).await
}

fn wrapped_session_store_refusal_is_nameable(error: lash::EmbedError) -> bool {
    matches!(
        error,
        lash::EmbedError::Session(lash::SessionError::Store {
            source: lash::persistence::StoreError::SessionDeleted { .. },
            ..
        })
    )
}

fn head_ownership_refusal_is_public(
    session_id: SessionId,
    owner: lash::SessionHeadOwner,
) -> Option<(SessionId, lash::SessionHeadOwner)> {
    let plugin = lash::plugins::PluginError::SessionHeadOwned { session_id, owner };
    let host = lash::EmbedError::from(plugin);
    match host {
        lash::EmbedError::Plugin(lash::plugins::PluginError::SessionHeadOwned {
            session_id,
            owner,
        }) => Some((session_id, owner)),
        _ => None,
    }
}

fn assert_store_object(_: &dyn RuntimeStore) {}

fn schema_dialect_types_are_nameable() {
    use lash::schema::{JsonSchema, SchemaContract, SchemaDialect, SchemaProjectionOverride};

    let dialect = SchemaDialect::OpenaiToolParameters;
    let contract = SchemaContract::default().with_override(dialect, JsonSchema::any());
    let _: SchemaProjectionOverride = contract.projection.overrides[0].clone();
    let _ = lash::tools::ToolDefinition::raw(
        "schema",
        "schema",
        "Schema",
        serde_json::json!({}),
        serde_json::json!({}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(120))
    .with_input_schema_projection(dialect, JsonSchema::any())
    .with_output_schema_projection(SchemaDialect::OpenaiStructuredOutput, JsonSchema::any());
}

fn main() {
    let _ = assert_store_object;
    let _ = schema_dialect_types_are_nameable;
    let _ = head_ownership_refusal_is_public;
    let _ = SessionHeadMeta::assemble(
        &SessionId::from("facade"),
        SessionHeadPayload {
            schema_version: 1,
            session_id: SessionId::from("facade"),
            config: PersistedSessionConfig::new(
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
                lash::NoProgressBudget::bounded(12),
                lash_core::SessionToolAccess::ambient(),
            ),
        },
        0,
        None,
        None,
        None,
    );
    let _ = persistence_types_are_nameable(GraphAppend::PreserveHead);
    let _ = plugin_types_are_nameable();
    let _ = context_compactor_types_are_nameable();
    let _ = direct_response_type_is_nameable;
    let _ = direct_payload_types_are_nameable;
    let _ = generation_option_types_are_nameable;
    let _ = builder_accepts_tools_and_plugins;
    let _ = a_core_is_built_over_one_backend;
    let _ = tool_contract_types_are_nameable;
    let _ = tool_catalog_types_are_nameable;
    let _ = message_role_type_is_nameable;
    let _ = turn_result_detail_types_are_nameable;
    let _ = provider_reliability_types_are_nameable;
    let _ = model_types_are_nameable;
    let _ = model_errors_are_nameable;
    let _ = persistence_load_helpers_are_nameable;
    let _ = verified_commit_chokepoint_is_nameable;
    let _ = wrapped_session_store_refusal_is_nameable;
    let _ = observation_types_are_homed_in_observe;
    let _ = cancellation_token_is_at_root;
    let _ = pending_turn_input_cancel_facade_is_nameable;
}

#[cfg(feature = "otel-trace")]
fn telemetry_types_are_nameable(
    telemetry: lash::tracing::OtelTelemetry,
    builder: lash::LashCoreBuilder,
) -> lash::LashCoreBuilder {
    use lash::tracing::{
        GEN_AI_SEMCONV_SNAPSHOT, LASH_INSTRUMENTATION_CONTRACT, LASH_INSTRUMENTATION_NAME,
        OtelOptions, OtelPayloadExport, OtelSpanEnricher, TelemetryMetrics, contract_markdown,
        otel,
    };
    let _: &OtelOptions = telemetry.options();
    let _: &TelemetryMetrics = telemetry.metrics();
    let _: OtelPayloadExport = OtelPayloadExport::Off;
    let _: Option<Arc<dyn OtelSpanEnricher>> = None;
    let _: Option<otel::trace::SpanContext> = None;
    let _: fn() -> String = contract_markdown;
    let _: &str = LASH_INSTRUMENTATION_NAME;
    let _: &str = LASH_INSTRUMENTATION_CONTRACT;
    let _: &str = GEN_AI_SEMCONV_SNAPSHOT;
    builder.telemetry(telemetry)
}

fn typed_host_run_surface(
    run: lash::RunId,
    handle: lash::RunHandle<String, String>,
    error: lash::admin::PluginTaskResultError<String>,
    page: lash::ChangePage<lash::SessionFault, Option<lash::SessionId>>,
) {
    let _: &lash::RunId = handle.run();
    let _ = (run, error, page.next, page.changes);
}

fn runtime_host_strings_are_validated_before_binding(
    core: &lash::LashCore,
    session: &str,
    input: &str,
) -> Result<(), lash::BlankIdentity> {
    let _builder = core.session(lash::SessionId::parse(session)?);
    let input = lash::TurnId::try_from(input.to_owned())?;
    let _batch_input = lash::BatchInput::new(lash::TurnInput::text("host input")).id(input);
    Ok(())
}
