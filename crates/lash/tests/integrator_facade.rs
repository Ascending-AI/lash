//! Compile-time proof that protocol and process-engine integrators need only `lash` paths.

#![allow(unused_imports)]

use std::sync::Arc;

use lash::SessionError;
use lash::attachments::MediaType;
use lash::direct::AttachmentSource;
use lash::durability::{
    BoundaryReason, CanonicalRuntimeEffectEnvelope, EffectJournalIdentity, EffectJournalRetirement,
    ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation,
    RuntimeAwaitEventOptions, RuntimeEffectReplayTrace, RuntimeReplay, RuntimeReplayAttribution,
    RuntimeSleepOptions, RuntimeSubject, SegmentProgress, ToolAttemptLaunch, ToolCallLaunch,
    TriggerLocalExecution,
};
use lash::messages::{PartAttachment, SessionMessageTreeNode, SharedJsonValue};
use lash::persistence::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, AttachmentOwnerKind,
    LiveReplayOutcome, LiveReplaySubscription, ProcessWakeSource, QueuedCheckpointTurnInput,
    QueuedCheckpointWork, QueuedTurnWork, SessionCursorError, SessionNodePayload,
    SessionNodeProjection, SessionNodeRecord, TurnInputClaimMode,
    queued_work::{PendingSessionWorkOrdering, PendingWorkOrderingKey},
};
use lash::plugins::{
    AgentFrameAssignment, AgentFrameId, AgentFrameReason, AgentFrameRecord, ChatContextProjector,
    CheckpointApplication, CodeExecutorPlugin, ContextProjector, ExecRequest, ExecResponse,
    ExecutionStateComponentSnapshot, ExecutionStateSnapshot, HostTurnProtocol,
    HydratedExecutionState, LlmToolSpec, PersistedSegmentHandover, PluginAbort, PluginExtensions,
    PluginSessionSnapshot, PluginSnapshotArtifact, PluginSnapshotEntry, PrepareTurnRequest,
    ProcessEngine, ProcessEngineProcessContext, ProcessEngineRegistry, ProcessEngineRunContext,
    ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessEngineValidationContext,
    ProcessInfraError, ProcessRunOutcome, PromptFingerprint, ProtocolBeforeLlmCallContext,
    ProtocolBuildInput, ProtocolDriverHandle, ProtocolDriverPlugin, ProtocolLlmCallAction,
    ProtocolRuntimeContext, ProtocolSessionContext, ProtocolSessionMaterialization,
    ProtocolSessionPlugin, ProtocolTurnOptionsError, RuntimeExecutionContext, SegmentHandover,
    SessionAuthorityContext, SessionContextOverlay, SessionPluginSource, ToolCatalog,
    TurnDriverConfig, TurnDriverPreamble, TurnFinalization, TurnHookReport, TurnLimitFinalMessage,
    TurnPreparation,
};
use lash::process::{
    ObserverInheritance, ProcessChange, ProcessCompletionOutcome, ProcessEventSemantics,
    ProcessExecutionConcurrencyError, ProcessExecutionWriteAuthority, ProcessId, ProcessOutcome,
    ProcessParentEndPlan, ProcessStartOutcome, ProcessTerminalSemantics, ProcessTerminalSpec,
    ProcessTombstone, SessionId, WaitKind, WaitState, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryDisposition, WakeDeliveryReport, WakeDeliveryState,
    WakeDiscardReason,
};
use lash::provider::{
    CacheRetention, ProviderCompletion, ProviderCompletionError, ProviderRateLimitPermit,
    ProviderRateLimiter,
};
use lash::runtime::{
    ApplyConfigPatch, OutputState, ProtocolTurnExtensionHandle, RuntimeControlConfig,
    RuntimeDurabilityConfig, RuntimeNamedPhase, RuntimePromptConfig, RuntimeProviderConfig,
    RuntimeTracingConfig, RuntimeTurnPhaseProbeSlot,
};
use lash::tools::{
    CompactToolContract, OrchestratingToolDef, PreparedToolBatch, PreparedToolBatchCall,
    ToolBatchReplies, ToolCallOutcome, ToolChildExecutionTraceHook, ToolChildProcessStarted,
    ToolIntentSubmissionAdmission, ToolIntentSubmissionRecord, ToolInvocation, ToolInvocationReply,
    ToolTriggerEffectOutcome, ToolValue,
};
use lash::triggers::TriggerEventCatalog;

struct Protocol;

#[async_trait::async_trait]
impl ProtocolSessionPlugin for Protocol {}

struct Executor;

#[async_trait::async_trait]
impl CodeExecutorPlugin for Executor {
    async fn execute_code(
        &self,
        _ctx: RuntimeExecutionContext<'_>,
        _request: ExecRequest,
    ) -> Result<ExecResponse, SessionError> {
        unreachable!("compile-time signature proof")
    }
}

struct Driver;

impl ProtocolDriverPlugin for Driver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        let protocol: Arc<dyn ProtocolDriverHandle<HostTurnProtocol>> =
            Arc::new(lash_protocol_standard::StandardDriver);
        let turn_limit_final_message: TurnLimitFinalMessage =
            Arc::new(|message_id, _max_turns| lash::messages::Message {
                id: message_id,
                role: lash::messages::MessageRole::System,
                parts: Arc::new(Vec::new()),
                origin: None,
            });
        let tool_specs: Arc<Vec<LlmToolSpec>> = input.tool_catalog.model_tool_specs();
        let tool_names = input.tool_catalog.tool_names();
        let tool_names_fingerprint: PromptFingerprint = input.tool_catalog.tool_names_fingerprint();

        TurnDriverPreamble {
            config: TurnDriverConfig::chat(protocol, true, turn_limit_final_message),
            tool_specs,
            tool_names,
            tool_names_fingerprint,
            execution_prompt: Arc::from("facade protocol witness"),
            prompt_contributions: input.extra_prompt_contributions,
        }
    }
}

#[test]
fn protocol_integrator_traits_are_implementable_from_the_facade() {
    fn assert_protocol<T: ProtocolSessionPlugin>() {}
    fn assert_executor<T: CodeExecutorPlugin>() {}
    fn assert_driver<T: ProtocolDriverPlugin>() {}
    fn assert_projector<T: ContextProjector<HostTurnProtocol>>() {}

    assert_protocol::<Protocol>();
    assert_executor::<Executor>();
    assert_driver::<Driver>();
    assert_projector::<ChatContextProjector>();

    let preamble = Driver.build_preamble(ProtocolBuildInput {
        tool_catalog: Arc::new(ToolCatalog::default()),
        plugin_extensions: PluginExtensions::default(),
        trigger_events: TriggerEventCatalog::default(),
        extra_prompt_contributions: Vec::new(),
    });
    assert!(preamble.config.sync_execution_environment);
    assert!(preamble.tool_specs.is_empty());
    assert!(preamble.tool_names.is_empty());
    assert_eq!(&*preamble.execution_prompt, "facade protocol witness");
    assert!(preamble.prompt_contributions.is_empty());
}

#[test]
fn remaining_host_ui_types_are_constructible_from_the_facade() {
    let node = SessionNodeRecord {
        node_id: "plugin-node".to_string(),
        parent_node_id: None,
        timestamp: "2026-08-25T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "facade-witness".to_string(),
            body: SharedJsonValue::new(serde_json::json!({ "reachable": true })),
        },
    };
    let (plugin_type, body) = node.plugin().expect("plugin node");
    assert_eq!(plugin_type, "facade-witness");
    assert_eq!(body["reachable"], true);

    let output_state = OutputState::Usable;
    assert!(matches!(output_state, OutputState::Usable));

    let contract = CompactToolContract {
        name: "lookup".to_string(),
        signature: "lookup(query: string)".to_string(),
        returns: "object".to_string(),
        parameters: Vec::new(),
        return_fields: Vec::new(),
        description: "Facade-only discovery contract".to_string(),
        examples: Vec::new(),
    };
    assert_eq!(
        contract.render_signature(),
        "lookup(query: string) -> object"
    );

    let outcome = ToolCallOutcome::Success(ToolValue::untrusted_json(
        serde_json::json!({ "reachable": true }),
    ));
    assert!(matches!(outcome, ToolCallOutcome::Success(_)));

    let attachment = PartAttachment {
        source: AttachmentSource::inline(
            MediaType::parse("text/plain").expect("valid media type"),
            b"facade witness".to_vec(),
        ),
    };
    assert!(matches!(attachment.source, AttachmentSource::Inline { .. }));
}
