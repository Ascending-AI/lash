mod formats;
mod transition;
use std::future::Future;
use std::sync::Arc;
pub use transition::{
    PluginNativeView, PluginTransitionBase, PluginTransitionId, PluginTransitionRecord,
    PluginTransitionRequest,
};

use crate::runtime::AssembledTurn;
use crate::{MessageRole, SessionPolicy, ToolManifest, ToolProvider};

pub use lash_core_store::store::plugin_writers::{
    PluginCallbackIdentity, PluginExecutionRefusal, PluginRevision,
};

pub use lash_core_store::plugin_state::{FormatNamespace, FormatRefusal, PluginConfigNamespace};

pub use lash_sansio::{CheckpointKind, PluginMessage, PluginRuntimeEvent, ToolCatalogContribution};

mod actions;
pub mod config;
mod error;
#[cfg(test)]
mod error_class_tests;
pub(crate) mod history;
mod hook_key;
mod hooks;
pub(crate) mod protocol;
mod recorded_callbacks;
mod registrar;
mod registry;
pub mod runtime_host;
mod runtime_impl;
mod services;
pub mod session_obj;
pub use session_obj::{ResolvedToolSurface, plugin_lifecycle_hook_issue};
pub(crate) mod session_types;
pub(crate) mod state;
pub use recorded_callbacks::{PluginCallbackBody, RecordedCallbackPhase, record_plugin_callbacks};
pub use state::EffectPublication;
use state::PluginStateRegistry;
pub(crate) use state::{Proposal, collect_proposals, propose, propose_all};
mod tool_catalog;
mod tool_hooks;
mod trigger_registry;

pub(crate) use actions::{
    ErasedPluginOperationInvokeFuture, PluginCommandHandler, PluginOperationContext,
    PluginOperationRegistration, PluginQueryHandler, PluginQueryInvokeFuture, PluginTaskHandler,
    RegisteredPluginOperation, declared_operation_failure, operation_protocol_failure,
    plugin_operation_spec,
};
pub(crate) use actions::{ErasedPluginOperationOutcome, PluginOperationSpec};
pub use actions::{
    PluginCommand, PluginCommandContext, PluginFailureClass, PluginFailureOrigin,
    PluginHookFailure, PluginOperation, PluginOperationDef, PluginOperationFailure,
    PluginOperationFuture, PluginOperationKind, PluginOperationOutcome, PluginOperationReceipt,
    PluginQuery, PluginQueryContext, PluginRuntimeDirective, PluginTask, PluginTaskContext,
    ProcessReadService, SessionParam, SessionReadService,
};
pub use config::{
    AdmittedPluginConfig, CandidateFacts, ConfigCommand, ConfigCommandCatalog,
    ConfigCommandDescriptor, ConfigOwner, ConfigRegistrar, ConfigRegistrationError, ConfigRegistry,
    ConfigSubmitError, ConfigTransaction, ConfigWire, CoreConfigOwner, CoreConfigRefusal,
    CreationConfigError, CreationFacts, NoRunOptions, OwnerChange, PluginConfig,
};
pub use error::{
    PluginError, PluginErrorClass, ToolIntentCommandFailure, ToolIntentRuntimeFailure,
    durable_identity_conflict, is_durable_identity_conflict, is_trigger_occurrence_reclaimed,
    trigger_occurrence_reclaimed,
};
pub use history::{
    CompactionContext, CompactionSystemPrompt, ContextCompaction, ContextCompactor, ContextError,
    ContextPressureContext, ContextPressureDecision, ContextPressureHook, DecidedContextPressure,
    PluginTraceEmitter, SessionReadView, TurnContextTransform, TurnTransformContext,
};
pub use hook_key::HookKey;
pub(crate) use hooks::owner_trace_context;
pub use hooks::require_session_owner;
pub use hooks::{
    AfterTurnHook, AssistantResponseHook, AssistantResponseHookContext, AssistantResponseTransform,
    AssistantStreamFinishReason, AssistantStreamFinishedContext, AssistantStreamFinishedHook,
    AssistantStreamHook, AssistantStreamHookContext, AssistantStreamTransform, BeforeTurnHook,
    CheckpointHook, CheckpointHookContext, NoPresentationArtifacts, PluginFuture,
    PluginLifecycleEvent, PluginLifecycleEventHook, PluginLifecycleFuture, PluginSessionTask,
    SessionConfigChangedContext, SessionStateChangedContext, ToolCatalogContributor,
    ToolPresentationArtifacts, ToolPresentationInput, ToolPresentationPresenter,
    ToolPresentationStep, ToolResultProjectionContext, TurnHookContext, TurnHookReport,
    TurnResultHookContext,
};
pub use protocol::{
    AssistantProseProjectorPlugin, CheckpointComponentKey, CodeExecutionOutcome,
    CodeExecutorPlugin, EXECUTION_STATE_LEAF_MIN_BODY_BYTES, ExecutionLeafName,
    ExecutionStateCapture, HydratedExecutionState, InvalidExecutionLeafName, LeafChange,
    PluginOptions, ProtocolBeforeLlmCallContext, ProtocolDriverPlugin, ProtocolLlmCallAction,
    ProtocolSessionContext, ProtocolSessionPlugin, ProtocolSessionRestoreView, SystemPromptContext,
    SystemPromptPurpose, TranscriptRowProjectorPlugin,
};
pub use registrar::{
    ContextRegistrations, ExecutionRegistrations, OutputRegistrations,
    PluginOperationRegistrations, PluginRegistrar, ProtocolRegistrations, SessionRegistrations,
    ToolCallRegistrations, ToolCatalogRegistrations, ToolRegistrations, ToolResultRegistrations,
    TriggerEventRegistrations, TurnRegistrations,
};
pub(crate) use registrar::{PluginContributions, RegisteredHook};
pub use registry::{
    BehaviorRevision, FormatVersion, PluginComposition, PluginDeclaration, PluginDeclarationError,
    PluginExecutionTrace, PluginExtensionContribution, PluginExtensions, PluginFactory, PluginId,
    PluginSessionContext, PluginSessionMaterialization, PluginSpec, PluginSpecBuilder,
    PluginSpecFactory, ProcessEngineContributionContext, SessionPlugin, SessionReadyContext,
    StaticPluginFactory,
};
pub use runtime_host::{
    AppendSessionNodesOutcome, AppendSessionNodesRequest, DirectCompletion, DirectLlmCompletion,
    SessionGraphService, SessionLifecycleService, SessionStateService,
};
pub use runtime_impl::{
    PluginHost, PluginSessionMaterializationRequest, PluginSessionRequest, SessionAuthorityContext,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use services::NoopSessionManager;
pub use services::{PersistentRuntimeServices, PluginOperationInvokeError, RuntimeServices};
pub use session_obj::PluginSession;
pub use session_types::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, FrameNodeId, FrameNodeIdError,
    OpenAgentFrameOutcome, OpenAgentFrameRequest, PluginOwned, SESSION_PLUGIN_INIT_MAX_BYTES,
    SessionCreateRequest, SessionHandle, SessionLineage, SessionObservedProcessOutcome,
    SessionObservedProcessReceipt, SessionObserverIntent, SessionPluginInit, SessionPluginSource,
    SessionRelation, SessionSnapshot, SessionStartPoint, SessionToolAccess, SessionToolAccessError,
    SubagentSessionContext, UnstatedSessionConfig,
};
pub use state::{
    FrontierRefusal, HookCause, HookOccurrence, KeyRejection, NamespaceFrontierRefusal,
    PluginNamespaceState, PluginState, PluginStateEffect, PluginStateError, PluginStateView,
    PublicationOrdinal, ResolvedStateChange, StateCommand, StateCommandOrigin, StateCommandRefusal,
    StateCommands, StateReducer, StateReduction, StateResolution, StateResolutionOutcome,
};
pub use tool_catalog::{
    AfterTurnContributions, CheckpointApplication, PluginAbort, PluginRecordContribution,
    RecordedTurnContribution, ToolCatalogContext, TurnContributions, TurnFinalization,
    TurnPreparation,
};
pub use tool_catalog::{observe_plugin_runtime_events, plugin_runtime_session_events};
pub use tool_hooks::{
    AfterToolContributions, AfterToolDecision, AttemptOrdinal, BeforeToolDecision,
    CachedToolSuccess, CheckRank, PreparedCallReadView, RankedVerdict, ToolArgsCheckHook,
    ToolArgsCheckInput, ToolArgsTransformHook, ToolArgsTransformInput, ToolHookContext,
    ToolHookOccurrence, ToolHookPhase, ToolResultCandidate, ToolResultCheckHook,
    ToolResultCheckInput, ToolResultTransformHook, ToolResultTransformInput,
};
pub(crate) use tool_hooks::{
    AttributedContributions, BeforeSelection, ResultChecks, after_resolution, before_selection,
    displaced_terminals, failed_check, failed_transform,
};
pub(crate) fn builtin_plugin_factories() -> Vec<Arc<dyn PluginFactory>> {
    // Protocol plugins must be registered by the embedder before calling
    // `PluginHost::build_session`. Unit tests use an in-tree fake to avoid
    // a dev-dep cycle through the protocol crates.
    let factories: Vec<Arc<dyn PluginFactory>> =
        vec![Arc::new(trigger_registry::TriggerResourcePluginFactory)];
    #[cfg(not(test))]
    return factories;

    #[cfg(test)]
    {
        factories
            .into_iter()
            .chain(crate::testing::test_standard_protocol_factories())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::SessionId;
    use crate::ToolOutcome;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;
    use crate::ToolDefinition;

    struct MockToolProvider;

    #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
    struct TypedEchoArgs {
        value: String,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
    struct TypedEchoOutput {
        value: String,
        session_id: Option<SessionId>,
    }

    struct TypedEchoOp;

    #[tokio::test]
    async fn plugin_boundary_fanout_keeps_all_typed_causes() {
        let factories: Vec<Arc<dyn PluginFactory>> = ["first", "second"]
            .into_iter()
            .map(|id| {
                Arc::new(StaticPluginFactory::new(
                    PluginDeclaration::initial(id),
                    PluginSpec::new().with_runtime_event(
                        crate::hook_key!("fail"),
                        Arc::new(move |_| {
                            Box::pin(async move {
                                if id == "first" {
                                    tokio::task::yield_now().await;
                                }
                                let error = PluginError::StoredDataCorrupt {
                                    record_kind: id.into(),
                                    message: "broken record".into(),
                                };
                                if id == "first" {
                                    let mut failure = PluginOperationFailure::from(error);
                                    failure.origin = Some(PluginFailureOrigin {
                                        plugin_id: "original-owner".into(),
                                        behavior_revision: std::num::NonZeroU32::new(7).unwrap(),
                                        operation: "original-operation".into(),
                                    });
                                    Err(PluginError::Operation(Box::new(failure)))
                                } else {
                                    Err(error)
                                }
                            })
                        }),
                    ),
                )) as Arc<dyn PluginFactory>
            })
            .collect();
        let session = PluginHost::new(factories)
            .build_session(PluginSessionRequest::creation(
                "typed-causes",
                Default::default(),
            ))
            .unwrap();
        let error = session
            .dispatch(None)
            .emit_runtime_event(PluginLifecycleEvent::SessionConfigChanged(Box::new(
                SessionConfigChangedContext {
                    session_id: "typed-causes".into(),
                    previous: SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    ),
                    current: SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    ),
                    sessions: Arc::new(NoopSessionManager),
                },
            )))
            .await
            .unwrap_err();
        let encoded = serde_json::to_value(&error).unwrap();
        assert_eq!(
            encoded["message"]["causes"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(
            encoded["message"]["causes"][0]["failure"]["payload"]["message"]["record_kind"],
            "first"
        );
        let replayed: PluginError = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(&replayed).unwrap(), encoded);
        let durable = ToolIntentCommandFailure::from(&replayed);
        let durable: ToolIntentCommandFailure =
            serde_json::from_slice(&serde_json::to_vec(&durable).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(durable).unwrap(), encoded);
        let issue = plugin_lifecycle_hook_issue(replayed.clone());
        let issue: crate::runtime::TurnIssue =
            serde_json::from_slice(&serde_json::to_vec(&issue).unwrap()).unwrap();
        assert_eq!(issue.plugin_failures.len(), 2);
        for (failure, owner) in issue.plugin_failures.iter().zip(["first", "second"]) {
            assert_eq!(failure.error_type, "lash.plugin.hook");
            assert_eq!(failure.origin.as_ref().unwrap().plugin_id, owner);
            let original: PluginHookFailure =
                serde_json::from_value(failure.payload.clone()).unwrap();
            assert_eq!(original.origin.plugin_id, owner);
            if owner == "first" {
                let origin = original.failure.origin.unwrap();
                assert_eq!(origin.plugin_id, "original-owner");
                assert_eq!(origin.behavior_revision.get(), 7);
                assert_eq!(origin.operation, "original-operation");
            }
        }
        let runtime = replayed.into_turn_failure(crate::RuntimeErrorCode::Plugin);
        let runtime: crate::RuntimeError =
            serde_json::from_slice(&serde_json::to_vec(&runtime).unwrap()).unwrap();
        let Some(crate::RuntimeErrorCause::PluginHooks { causes }) = runtime.cause else {
            panic!("typed runtime aggregate");
        };
        assert_eq!(causes.len(), 2);
        for (cause, owner) in causes.iter().zip(["first", "second"]) {
            assert_eq!(cause.origin.plugin_id, owner);
            assert_eq!(cause.origin.behavior_revision.get(), 1);
            assert_eq!(cause.origin.operation, "runtime_event:fail");
            assert_eq!(cause.failure.class, PluginFailureClass::Terminal);
            let original: PluginError =
                serde_json::from_value(cause.failure.payload.clone()).unwrap();
            assert!(
                matches!(original, PluginError::StoredDataCorrupt { record_kind, .. } if record_kind == owner)
            );
        }
    }

    #[test]
    fn plugin_boundary_operations_declare_error_schema() {
        let registration = PluginOperationRegistration::query(
            plugin_operation_spec::<TypedEchoOp>(),
            Arc::new(|_, args| Box::pin(async move { Ok(args) })),
        );
        let encoded = serde_json::to_value(registration.def()).unwrap();
        assert!(encoded["error_schema"].is_object());
        assert!(encoded["error_type"].is_string());
        assert!(encoded["error_version"].as_u64().is_some());
    }

    #[test]
    fn plugin_boundary_observer_contexts_only_offer_reads() {
        let syntax = syn::parse_file(include_str!("hooks.rs")).unwrap();
        for name in ["SessionStateChangedContext", "SessionConfigChangedContext"] {
            let fields = syntax
                .items
                .iter()
                .find_map(|item| match item {
                    syn::Item::Struct(item) if item.ident == name => Some(&item.fields),
                    _ => None,
                })
                .unwrap();
            assert!(
                fields.iter().all(|field| {
                    !matches!(
                        field.ident.as_ref().map(ToString::to_string).as_deref(),
                        Some("session_graph" | "direct_completions")
                    )
                }),
                "observer {name} exposes execution services"
            );
            let session_field = fields
                .iter()
                .find(|field| field.ident.as_ref().is_some_and(|id| id == "sessions"))
                .unwrap();
            assert!(
                matches!(&session_field.ty, syn::Type::Path(path)
                if path.path.segments.last().is_some_and(|segment| matches!(
                    &segment.arguments, syn::PathArguments::AngleBracketed(args)
                    if args.args.iter().any(|arg| matches!(arg,
                        syn::GenericArgument::Type(syn::Type::TraitObject(object))
                        if object.bounds.iter().any(|bound| matches!(bound,
                            syn::TypeParamBound::Trait(trait_bound)
                            if trait_bound.path.is_ident("SessionReadService")))))))),
                "observer {name} exposes session mutation"
            );
        }
    }

    impl PluginOperation for TypedEchoOp {
        const NAME: &'static str = "mock.typed_echo";
        const DESCRIPTION: &'static str = "typed echo";
        const SESSION_PARAM: SessionParam = SessionParam::Optional;
        type Args = TypedEchoArgs;
        type Output = TypedEchoOutput;
        type Error = String;
        const ERROR_TYPE: &'static str = Self::NAME;
        const ERROR_VERSION: crate::FormatVersion = crate::FormatVersion::ONE;
        fn error_class(_: &Self::Error) -> lash_sansio::PluginFailureClass {
            lash_sansio::PluginFailureClass::Terminal
        }
    }

    impl PluginQuery for TypedEchoOp {}

    #[async_trait::async_trait]
    impl ToolProvider for MockToolProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            self.tool_definitions()
                .into_iter()
                .map(|tool| tool.manifest())
                .collect()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            self.tool_definitions()
                .into_iter()
                .find(|tool| tool.name() == name)
                .map(|tool| Arc::new(tool.contract()))
        }

        async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(call.args.clone()).into()
        }
    }

    impl MockToolProvider {
        fn tool_definitions(&self) -> Vec<ToolDefinition> {
            vec![
                ToolDefinition::raw(
                    "tool:mock_tool",
                    "mock_tool",
                    "",
                    json!({
                        "type": "object",
                        "properties": { "value": { "type": "string" } },
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                    json!({ "type": "string" }),
                )
                .expect("valid declared tool schemas"),
            ]
        }
    }

    struct MockPluginFactory;

    impl PluginFactory for MockPluginFactory {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn declaration(&self) -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial(self.id())
        }

        fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            let session_id = ctx.owner.session_id().cloned().ok_or_else(|| {
                PluginError::Session("the mock plugin serves sessions".to_string())
            })?;
            Ok(Arc::new(MockPlugin { session_id }))
        }
    }

    struct MockPlugin {
        session_id: SessionId,
    }

    impl SessionPlugin for MockPlugin {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            reg.tools().provider(Arc::new(MockToolProvider))?;
            let session_id = self.session_id.clone();
            reg.operations().query(
                PluginOperationSpec {
                    name: "mock.echo".to_string(),
                    description: "echo".to_string(),
                    session_param: SessionParam::Optional,
                    input_schema: json!({}),
                    output_schema: json!({}),
                    error_type: "test.operation".into(),
                    error_version: crate::FormatVersion::ONE,
                    error_schema: json!({"type": "object"}),
                },
                Arc::new(move |ctx, args| {
                    let session_id = session_id.clone();
                    Box::pin(async move {
                        Ok(json!({
                            "session_id": ctx.session_id,
                            "plugin_session_id": session_id,
                            "args": args,
                        }))
                    })
                }),
            )?;
            reg.operations()
                .typed_query::<TypedEchoOp, _, _>(move |ctx, args| async move {
                    Ok(TypedEchoOutput {
                        value: args.value,
                        session_id: ctx.session_id,
                    })
                })?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn external_query_defaults_to_current_session_when_requested() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("session");
        let (_plugin_id, result) = session
            .query_plugin(
                "mock.echo",
                json!({"ok":true}),
                None,
                true,
                Arc::new(NoopSessionManager),
                Arc::new(NoopSessionManager),
            )
            .await
            .expect("invoke");
        assert_eq!(
            result.get("session_id").and_then(|v| v.as_str()),
            Some("root")
        );
    }

    #[tokio::test]
    async fn plugin_query_generates_schema_and_invokes_typed_output() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("session");

        let def = session
            .plugin_operations()
            .into_iter()
            .find(|def| def.name == TypedEchoOp::NAME)
            .expect("typed op definition");
        assert_eq!(def.kind(), PluginOperationKind::Query);
        assert_eq!(def.session_param, SessionParam::Optional);
        let value_type = def
            .input_schema
            .pointer("/schema/properties/value/type")
            .or_else(|| def.input_schema.pointer("/properties/value/type"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(value_type, Some("string"));

        let (_plugin_id, output) = session
            .query_plugin(
                TypedEchoOp::NAME,
                serde_json::to_value(TypedEchoArgs {
                    value: "hello".to_string(),
                })
                .unwrap(),
                None,
                true,
                Arc::new(NoopSessionManager),
                Arc::new(NoopSessionManager),
            )
            .await
            .expect("typed invoke");
        let output: TypedEchoOutput = serde_json::from_value(output).unwrap();
        assert_eq!(output.value, "hello");
        assert_eq!(output.session_id.as_deref(), Some("root"));
    }

    #[test]
    fn plugin_operation_rejects_cross_kind_duplicate_names() {
        struct EchoTaskOp;
        impl PluginOperation for EchoTaskOp {
            const NAME: &'static str = TypedEchoOp::NAME;
            const DESCRIPTION: &'static str = "task colliding with a query name";
            const SESSION_PARAM: SessionParam = SessionParam::Optional;
            type Args = TypedEchoArgs;
            type Output = TypedEchoOutput;
            type Error = String;
            const ERROR_TYPE: &'static str = Self::NAME;
            const ERROR_VERSION: crate::FormatVersion = crate::FormatVersion::ONE;
            fn error_class(_: &Self::Error) -> lash_sansio::PluginFailureClass {
                lash_sansio::PluginFailureClass::Terminal
            }
        }
        impl PluginTask for EchoTaskOp {}

        struct CrossKindPlugin;
        impl SessionPlugin for CrossKindPlugin {
            fn id(&self) -> &'static str {
                "cross_kind"
            }

            fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
                reg.operations()
                    .typed_query::<TypedEchoOp, _, _>(move |ctx, args| async move {
                        Ok(TypedEchoOutput {
                            value: args.value,
                            session_id: ctx.session_id,
                        })
                    })?;
                reg.operations()
                    .typed_task_value::<EchoTaskOp, _, _>(move |ctx, args| async move {
                        Ok(TypedEchoOutput {
                            value: args.value,
                            session_id: ctx.session_id,
                        })
                    })
            }
        }

        struct CrossKindFactory;
        impl PluginFactory for CrossKindFactory {
            fn id(&self) -> &'static str {
                "cross_kind"
            }

            fn declaration(&self) -> crate::plugin::PluginDeclaration {
                crate::plugin::PluginDeclaration::initial(self.id())
            }

            fn build(
                &self,
                _ctx: &PluginSessionContext,
            ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
                Ok(Arc::new(CrossKindPlugin))
            }
        }

        let err = match PluginHost::new(vec![Arc::new(CrossKindFactory)])
            .build_session(PluginSessionRequest::creation("root", Default::default()))
        {
            Ok(_) => panic!("a task may not reuse a registered query name"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains(&format!(
                "duplicate plugin operation name `{}`",
                TypedEchoOp::NAME
            )),
            "unexpected refusal: {err}"
        );
    }

    fn kindless_operation_spec(name: &str) -> PluginOperationSpec {
        PluginOperationSpec {
            name: name.to_string(),
            description: "operation registered without naming a kind".to_string(),
            session_param: SessionParam::Forbidden,
            input_schema: json!({}),
            output_schema: json!({}),
            error_type: "test.operation".into(),
            error_version: crate::FormatVersion::ONE,
            error_schema: json!({"type": "object"}),
        }
    }

    fn query_context() -> PluginOperationContext {
        PluginOperationContext::Query(PluginQueryContext {
            session_id: None,
            sessions: Arc::new(NoopSessionManager),
            processes: Arc::new(NoopSessionManager),
        })
    }

    /// A kind mismatch is unrepresentable because `PluginOperationSpec` has no kind to
    /// contradict: the stored discriminant comes from whichever registration constructor
    /// wrapped the handler, and nowhere else.
    #[tokio::test]
    async fn plugin_operation_registration_stamps_kind_from_its_constructor() {
        let query = PluginOperationRegistration::query(
            kindless_operation_spec("test.kindless_query"),
            Arc::new(|_ctx, args| Box::pin(async move { Ok(args) })),
        );
        let command = PluginOperationRegistration::command(
            kindless_operation_spec("test.kindless_command"),
            Arc::new(|_ctx, args| {
                Box::pin(async move { Ok(ErasedPluginOperationOutcome::new(args)) })
            }),
        );
        let task = PluginOperationRegistration::task(
            kindless_operation_spec("test.kindless_task"),
            Arc::new(|_ctx, args| {
                Box::pin(async move { Ok(ErasedPluginOperationOutcome::new(args)) })
            }),
        );

        assert_eq!(query.def().kind(), PluginOperationKind::Query);
        assert_eq!(command.def().kind(), PluginOperationKind::Command);
        assert_eq!(task.def().kind(), PluginOperationKind::Task);

        // The query registration accepts the context its stamped kind names.
        let outcome = query
            .invoke(query_context(), json!({"ok": true}))
            .await
            .expect("query invocation");
        assert_eq!(outcome.output, json!({"ok": true}));

        // The other two degrade to a typed failure rather than panicking.
        let err = command
            .invoke(query_context(), json!({}))
            .await
            .expect_err("command registration must refuse a query context");
        assert_eq!(
            err.to_string(),
            "command registration invoked with a query context"
        );
        let err = task
            .invoke(query_context(), json!({}))
            .await
            .expect_err("task registration must refuse a query context");
        assert_eq!(
            err.to_string(),
            "task registration invoked with a query context"
        );
    }

    #[tokio::test]
    async fn plugin_session_queries_forked_session() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let root = host
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("root");
        let child = root
            .fork_for_session("child", SessionAuthorityContext::default())
            .expect("child");

        let (_plugin_id, result) = child
            .query_plugin(
                "mock.echo",
                json!({"ok":true}),
                Some(SessionId::from("child")),
                false,
                Arc::new(NoopSessionManager),
                Arc::new(NoopSessionManager),
            )
            .await
            .expect("invoke");
        assert_eq!(
            result.get("session_id").and_then(|v| v.as_str()),
            Some("child")
        );
        assert_eq!(
            result.get("plugin_session_id").and_then(|v| v.as_str()),
            Some("child")
        );

        drop(child);
    }

    #[test]
    fn plugin_host_unregisters_sessions() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let _session = host
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("session");
        assert!(host.session(&SessionId::from("root")).is_ok());
        host.unregister_session(&SessionId::from("root"))
            .expect("unregister");
        match host.session(&SessionId::from("root")) {
            Err(PluginOperationInvokeError::UnknownSession(id)) => assert_eq!(id, "root"),
            Ok(_) => panic!("expected missing session"),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}
