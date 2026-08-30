use std::future::Future;
use std::sync::Arc;

use crate::runtime::AssembledTurn;
use crate::{
    MessageRole, ProtocolTurnOptions, SessionPolicy, ToolDefinition, ToolManifest, ToolOutcome,
    ToolProvider, TurnInput,
};

pub use lash_sansio::{
    CheckpointKind, PluginMessage, PluginRuntimeEvent, PromptContribution, ToolCatalogContribution,
};

mod actions;
mod error;
pub(crate) mod history;
mod hooks;
pub(crate) mod protocol;
mod registrar;
mod registry;
pub mod runtime_host;
mod runtime_impl;
mod services;
mod session_obj;
pub(crate) use session_obj::plugin_lifecycle_hook_issue;
pub(crate) mod session_types;
mod snapshot;
mod tool_catalog;
mod trigger_registry;

pub(crate) use actions::{
    ErasedPluginOperationInvokeFuture, ErasedPluginOperationOutcome, PluginCommandHandler,
    PluginOperationContext, PluginOperationRegistration, PluginOperationSpec, PluginQueryHandler,
    PluginQueryInvokeFuture, PluginTaskHandler, RegisteredPluginOperation, plugin_operation_spec,
};
pub use actions::{
    PluginCommand, PluginCommandContext, PluginOperation, PluginOperationDef,
    PluginOperationFailure, PluginOperationFuture, PluginOperationKind, PluginOperationOutcome,
    PluginOperationReceipt, PluginQuery, PluginQueryContext, PluginRuntimeDirective, PluginTask,
    PluginTaskContext, ProcessReadService, SessionParam, SessionReadService,
};
pub use error::PluginError;
pub use history::{
    CompactionContext, ContextCompaction, ContextCompactor, ContextError, SessionReadView,
    TurnContextTransform, TurnTransformContext,
};
pub use hooks::{
    AfterToolCallHook, AfterTurnHook, AssistantResponseHook, AssistantResponseHookContext,
    AssistantResponseTransform, AssistantStreamFinishReason, AssistantStreamFinishedContext,
    AssistantStreamFinishedHook, AssistantStreamHook, AssistantStreamHookContext,
    AssistantStreamTransform, BeforeToolCallHook, BeforeTurnHook, CheckpointHook,
    CheckpointHookContext, PluginFuture, PluginLifecycleEvent, PluginLifecycleEventHook,
    PluginLifecycleFuture, PluginSessionTask, PromptContributor, PromptHookContext,
    SessionConfigChangedContext, SessionConfigMutator, SessionStateChangedContext,
    ToolCallHookContext, ToolCatalogContributor, ToolResultHookContext,
    ToolResultProjectionContext, ToolResultProjector, TurnHookContext, TurnHookReport,
    TurnResultHookContext,
};
pub use protocol::{
    AssistantProseProjectorPlugin, CodeExecutionDisposition, CodeExecutorPlugin,
    EXECUTION_STATE_LEAF_MIN_BODY_BYTES, ExecutionStateComponentSnapshot, ExecutionStateSnapshot,
    HydratedExecutionState, PluginOptions, ProtocolBeforeLlmCallContext, ProtocolDriverPlugin,
    ProtocolLlmCallAction, ProtocolRuntimeContext, ProtocolSessionContext,
    ProtocolSessionMaterialization, ProtocolSessionPlugin,
};
pub use registrar::{
    ContextRegistrations, ExecutionRegistrations, OutputRegistrations,
    PluginOperationRegistrations, PluginRegistrar, PromptRegistrations, ProtocolRegistrations,
    SessionRegistrations, ToolCallRegistrations, ToolCatalogRegistrations, ToolRegistrations,
    ToolResultRegistrations, TriggerEventRegistrations, TurnRegistrations,
};
pub(crate) use registrar::{PluginContributions, RegisteredHook};
pub use registry::{
    PluginExtensionContribution, PluginExtensions, PluginFactory, PluginSessionContext,
    PluginSessionMaterialization, PluginSpec, PluginSpecBuilder, PluginSpecFactory,
    ProcessEngineContributionContext, SessionPlugin, SessionReadyContext, StaticPluginFactory,
};
pub use runtime_host::{
    AppendSessionNodesOutcome, AppendSessionNodesRequest, DirectCompletion, DirectLlmCompletion,
    SessionGraphService, SessionLifecycleService, SessionStateService, SessionTurnInput,
    SessionTurnRequest,
};
pub use runtime_impl::{
    PluginHost, RecordedSessionConfig, SessionAuthorityContext, SessionCreationConfig,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use services::NoopSessionManager;
pub use services::{PersistentRuntimeServices, PluginOperationInvokeError, RuntimeServices};
pub use session_obj::PluginSession;
pub use session_types::{
    AgentFrameAssignment, AgentFrameId, AgentFrameReason, AgentFrameRecord, FrameNodeId,
    OpenAgentFrameRequest, OpenAgentFrameResult, PluginOwned, SessionContextOverlay,
    SessionCreateRequest, SessionHandle, SessionObservedProcessOutcome,
    SessionObservedProcessReceipt, SessionObserverIntent, SessionObserverIntentAttribution,
    SessionPluginSource, SessionRelation, SessionSnapshot, SessionStartPoint, SessionToolAccess,
    SubagentSessionContext,
};
pub(crate) use snapshot::{InMemorySnapshotReader, InMemorySnapshotWriter};
pub use snapshot::{
    PluginSessionSnapshot, PluginSnapshotArtifact, PluginSnapshotEntry, PluginSnapshotMeta,
    SnapshotReader, SnapshotWriter,
};
pub use tool_catalog::{
    AbortTurnDirective, AfterToolCallPluginDirective, AfterTurnPluginDirective,
    BeforeToolCallPluginDirective, CheckpointApplication, EnqueueMessagesDirective, PluginAbort,
    PluginDirective, PrepareTurnRequest, ReplaceToolArgsDirective, ShortCircuitToolDirective,
    ToolCatalogContext, TurnFinalization, TurnPluginDirective, TurnPreparation,
};
pub(crate) use tool_catalog::{
    AmbientDirectiveAction, AmbientDirectiveError, PluginTerminalStrength,
    emit_plugin_runtime_events, interpret_ambient_directive, plugin_runtime_session_events,
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
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;
    use crate::{SessionSnapshot, ToolDefinition};

    struct MockToolProvider;

    #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
    struct TypedEchoArgs {
        value: String,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
    struct TypedEchoOutput {
        value: String,
        session_id: Option<String>,
    }

    struct TypedEchoOp;

    impl PluginOperation for TypedEchoOp {
        const NAME: &'static str = "mock.typed_echo";
        const DESCRIPTION: &'static str = "typed echo";
        const SESSION_PARAM: SessionParam = SessionParam::Optional;
        type Args = TypedEchoArgs;
        type Output = TypedEchoOutput;
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

        async fn execute(&self, call: crate::ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(call.args.clone())
        }
    }

    impl MockToolProvider {
        fn tool_definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition::raw(
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
            )]
        }
    }

    struct DuplicateNameToolProvider;

    #[async_trait::async_trait]
    impl ToolProvider for DuplicateNameToolProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            vec![
                ToolDefinition::raw(
                    "tool:different_id",
                    "mock_tool",
                    "duplicate model-facing name",
                    ToolDefinition::default_input_schema(),
                    json!({}),
                )
                .manifest(),
            ]
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
            None
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(json!("unreachable"))
        }
    }

    #[test]
    fn plugin_registrar_preserves_typed_duplicate_tool_name_refusal() {
        let mut registrar = PluginRegistrar::new();
        registrar.registering_plugin_id = Some("duplicate-law".to_string());
        registrar
            .tools()
            .provider(Arc::new(MockToolProvider))
            .expect("first provider registers");

        let error = registrar
            .tools()
            .provider(Arc::new(DuplicateNameToolProvider))
            .expect_err("the registrar refuses the duplicate before assembly");
        assert!(matches!(
            error,
            PluginError::Registration(ref message)
                if message == "duplicate plugin tool name `mock_tool`"
        ));
    }

    struct MockPluginFactory;

    impl PluginFactory for MockPluginFactory {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            Ok(Arc::new(MockPlugin {
                session_id: ctx.session_id.clone(),
            }))
        }
    }

    const TEST_EXTENSION_ID: &str = "test.extension";

    struct ExtensionPluginFactory;

    impl PluginFactory for ExtensionPluginFactory {
        fn id(&self) -> &'static str {
            "extension_resource"
        }

        fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
            vec![PluginExtensionContribution::from_value(
                TEST_EXTENSION_ID,
                json!({ "resource": "clock.alarm" }),
            )]
        }

        fn build(
            &self,
            _ctx: &PluginSessionContext,
        ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            Ok(Arc::new(ExtensionPlugin))
        }
    }

    struct ExtensionPlugin;

    impl SessionPlugin for ExtensionPlugin {
        fn id(&self) -> &'static str {
            "extension_resource"
        }

        fn register(&self, _reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            Ok(())
        }
    }

    struct MockPlugin {
        session_id: String,
    }

    use crate::testing::MockSessionManager;

    impl SessionPlugin for MockPlugin {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            reg.tools().provider(Arc::new(MockToolProvider))?;
            reg.prompt().contribute(Arc::new(|_ctx| {
                Box::pin(async move {
                    Ok(vec![
                        PromptContribution::guidance("Dynamic Note", "dynamic note")
                            .with_priority(1),
                        PromptContribution::guidance("Gated", "Shared guidance")
                            .requires_tool("mock_tool"),
                        PromptContribution::guidance("Always", "Shared guidance"),
                        PromptContribution::guidance("Plugin Prompt", "Structured plugin prompt"),
                    ])
                })
            }));
            let session_id = self.session_id.clone();
            reg.operations().query(
                PluginOperationSpec {
                    name: "mock.echo".to_string(),
                    description: "echo".to_string(),
                    session_param: SessionParam::Optional,
                    input_schema: json!({}),
                    output_schema: json!({}),
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

        fn snapshot(
            &self,
            _writer: &mut dyn SnapshotWriter,
        ) -> Result<PluginSnapshotMeta, PluginError> {
            Ok(PluginSnapshotMeta {
                plugin_id: self.id().to_string(),
                plugin_version: self.version().to_string(),
                revision: self.snapshot_revision(),
                state: Some(json!({"session_id": self.session_id})),
            })
        }
    }

    #[test]
    fn plugin_host_collects_factory_extension_contributions() {
        let host = PluginHost::new(vec![Arc::new(ExtensionPluginFactory)]);

        assert_eq!(
            host.extensions().payloads(TEST_EXTENSION_ID),
            &[json!({ "resource": "clock.alarm" })]
        );
        let session = host.build_session("root").expect("session");
        assert_eq!(
            session.extensions().payloads(TEST_EXTENSION_ID),
            &[json!({ "resource": "clock.alarm" })]
        );
    }

    #[test]
    fn declared_triggers_enter_session_catalog() {
        struct TriggerEventOnlyFactory;

        impl PluginFactory for TriggerEventOnlyFactory {
            fn id(&self) -> &'static str {
                "trigger_only"
            }

            fn build(
                &self,
                _ctx: &PluginSessionContext,
            ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
                Ok(Arc::new(TriggerEventOnlyPlugin))
            }
        }

        struct TriggerEventOnlyPlugin;

        impl SessionPlugin for TriggerEventOnlyPlugin {
            fn id(&self) -> &'static str {
                "trigger_only"
            }

            fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
                reg.triggers().declare(crate::TriggerEvent::new(
                    "Button",
                    "ui.button",
                    "pressed",
                    crate::LashSchema::any(),
                ))
            }
        }

        let host = PluginHost::new(vec![Arc::new(TriggerEventOnlyFactory)]);

        let session = host.build_session("root").expect("session");
        assert!(
            session
                .triggers()
                .get("Button", "ui.button", "pressed")
                .is_some()
        );
        let event = session
            .triggers()
            .get("Button", "ui.button", "pressed")
            .expect("button event");
        assert_eq!(event.source_type(), "ui.button.pressed");
    }

    #[tokio::test]
    async fn session_collects_tools_and_prompts() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host.build_session("root").expect("session");
        let tool_names = session
            .tools()
            .tool_manifests()
            .into_iter()
            .map(|manifest| manifest.name)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(tool_names.contains("mock_tool"));
        assert!(tool_names.contains("batch"));
        let contributions = session
            .collect_prompt_contributions(PromptHookContext {
                session_id: "root".to_string(),
                sessions: Arc::new(MockSessionManager::default()),
                state: SessionReadView::from_snapshot(&SessionSnapshot::new(
                    crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
                )),
                protocol_turn_options: ProtocolTurnOptions::default(),
                turn_context: crate::TurnContext::default(),
            })
            .await
            .expect("prompt contributions");
        assert_eq!(
            contributions,
            vec![
                PromptContribution::guidance("Dynamic Note", "dynamic note").with_priority(1),
                PromptContribution::guidance("Gated", "Shared guidance").requires_tool("mock_tool"),
                PromptContribution::guidance("Always", "Shared guidance"),
                PromptContribution::guidance("Plugin Prompt", "Structured plugin prompt"),
            ]
        );
    }

    #[tokio::test]
    async fn external_query_defaults_to_current_session_when_requested() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host.build_session("root").expect("session");
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
        let session = host.build_session("root").expect("session");

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
    fn plugin_operation_rejects_duplicate_names() {
        struct DuplicatePlugin;

        impl SessionPlugin for DuplicatePlugin {
            fn id(&self) -> &'static str {
                "duplicate"
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
                    .typed_query::<TypedEchoOp, _, _>(move |ctx, args| async move {
                        Ok(TypedEchoOutput {
                            value: args.value,
                            session_id: ctx.session_id,
                        })
                    })
            }
        }

        struct DuplicateFactory;
        impl PluginFactory for DuplicateFactory {
            fn id(&self) -> &'static str {
                "duplicate"
            }

            fn build(
                &self,
                _ctx: &PluginSessionContext,
            ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
                Ok(Arc::new(DuplicatePlugin))
            }
        }

        let err = match PluginHost::new(vec![Arc::new(DuplicateFactory)]).build_session("root") {
            Ok(_) => panic!("duplicate typed plugin operation should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("duplicate plugin operation name"));
    }

    fn kindless_operation_spec(name: &str) -> PluginOperationSpec {
        PluginOperationSpec {
            name: name.to_string(),
            description: "operation registered without naming a kind".to_string(),
            session_param: SessionParam::Forbidden,
            input_schema: json!({}),
            output_schema: json!({}),
        }
    }

    fn query_context() -> PluginOperationContext {
        PluginOperationContext::Query(PluginQueryContext {
            session_id: None,
            sessions: Arc::new(NoopSessionManager),
            processes: Arc::new(NoopSessionManager),
        })
    }

    /// A kind mismatch is unrepresentable because `PluginOperationSpec` has no
    /// kind to contradict: the stored discriminant comes from whichever
    /// registration constructor wrapped the handler, and nowhere else. This
    /// asserts that stamping for all three constructors -- changing or
    /// dropping any one of the three `PluginOperationDef::from_spec` kinds
    /// turns this test red.
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

    /// The one-map collapse keeps operation names globally unique across
    /// kinds: a task may not take a name a query already holds, and the
    /// refusal is the same one a same-kind collision gets.
    #[test]
    fn plugin_operation_rejects_cross_kind_duplicate_names() {
        struct EchoTaskOp;
        impl PluginOperation for EchoTaskOp {
            const NAME: &'static str = TypedEchoOp::NAME;
            const DESCRIPTION: &'static str = "task colliding with a query name";
            const SESSION_PARAM: SessionParam = SessionParam::Optional;
            type Args = TypedEchoArgs;
            type Output = TypedEchoOutput;
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

            fn build(
                &self,
                _ctx: &PluginSessionContext,
            ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
                Ok(Arc::new(CrossKindPlugin))
            }
        }

        let err = match PluginHost::new(vec![Arc::new(CrossKindFactory)]).build_session("root") {
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

    #[tokio::test]
    async fn typed_external_query_errors_on_invalid_output() {
        struct BadOp;
        impl PluginOperation for BadOp {
            const NAME: &'static str = "mock.echo";
            const DESCRIPTION: &'static str = "bad typed projection over raw op";
            const SESSION_PARAM: SessionParam = SessionParam::Optional;
            type Args = TypedEchoArgs;
            type Output = TypedEchoOutput;
        }
        impl PluginQuery for BadOp {}

        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host.build_session("root").expect("session");
        let (_plugin_id, output) = session
            .query_plugin(
                BadOp::NAME,
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
            .expect("raw query");
        let err = serde_json::from_value::<TypedEchoOutput>(output)
            .expect_err("raw output shape should not match typed output");
        assert!(err.to_string().contains("missing field"));
    }

    #[tokio::test]
    async fn plugin_session_queries_registered_session() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host.build_session("root").expect("session");

        let (_plugin_id, result) = session
            .query_plugin(
                "mock.echo",
                json!({"ok":true}),
                Some("root".to_string()),
                false,
                Arc::new(NoopSessionManager),
                Arc::new(NoopSessionManager),
            )
            .await
            .expect("invoke");
        assert_eq!(
            result.get("session_id").and_then(|v| v.as_str()),
            Some("root")
        );
        assert_eq!(
            result.get("plugin_session_id").and_then(|v| v.as_str()),
            Some("root")
        );
    }

    #[tokio::test]
    async fn plugin_session_queries_forked_session() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let root = host.build_session("root").expect("root");
        let child = root
            .fork_for_session("child", SessionCreationConfig::default())
            .expect("child");

        let (_plugin_id, result) = child
            .query_plugin(
                "mock.echo",
                json!({"ok":true}),
                Some("child".to_string()),
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
        let _session = host.build_session("root").expect("session");
        assert!(host.session("root").is_ok());
        host.unregister_session("root").expect("unregister");
        match host.session("root") {
            Err(PluginOperationInvokeError::UnknownSession(id)) => assert_eq!(id, "root"),
            Ok(_) => panic!("expected missing session"),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn snapshot_round_trip_preserves_plugin_entries() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host.build_session("root").expect("session");
        let snapshot = session.snapshot().expect("snapshot");
        assert!(snapshot.plugins.contains_key("mock"));
        let restored = host
            .rematerialize_session(
                "child",
                &snapshot,
                RecordedSessionConfig::new(ProtocolTurnOptions::default()),
            )
            .expect("restored");
        let restored_snapshot = restored.snapshot().expect("snapshot");
        assert!(restored_snapshot.plugins.contains_key("mock"));
    }

    #[test]
    fn runtime_services_are_backed_by_plugin_sessions() {
        let host = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
            "mock_tool",
            PluginSpec::new()
                .with_tool_provider(Arc::new(MockToolProvider) as Arc<dyn ToolProvider>),
        ))]);
        let services = RuntimeServices::new(host.build_session("root").expect("session"));
        assert_eq!(services.plugins.session_id(), "root");
        assert!(
            services
                .plugins
                .tools()
                .tool_manifests()
                .iter()
                .any(|tool| tool.name == "mock_tool")
        );
    }

    struct ProjectorPluginFactory {
        plugin_id: &'static str,
    }

    impl PluginFactory for ProjectorPluginFactory {
        fn id(&self) -> &'static str {
            self.plugin_id
        }

        fn build(
            &self,
            _ctx: &PluginSessionContext,
        ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            Ok(Arc::new(ProjectorPlugin {
                plugin_id: self.plugin_id,
            }))
        }
    }

    struct ProjectorPlugin {
        plugin_id: &'static str,
    }

    impl SessionPlugin for ProjectorPlugin {
        fn id(&self) -> &'static str {
            self.plugin_id
        }

        fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            reg.tool_results().projector(Arc::new(|ctx| {
                Box::pin(async move {
                    Ok(crate::ModelToolReturn::from_output(
                        ctx.call_id,
                        ctx.tool_name,
                        &ctx.output,
                    ))
                })
            }))
        }
    }

    #[test]
    fn duplicate_tool_result_projectors_are_rejected() {
        let host = PluginHost::new(vec![
            Arc::new(ProjectorPluginFactory {
                plugin_id: "projector-a",
            }),
            Arc::new(ProjectorPluginFactory {
                plugin_id: "projector-b",
            }),
        ]);
        let err = match host.build_session("root") {
            Ok(_) => panic!("duplicate projector"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("duplicate tool result projector"));
        assert!(err.to_string().contains("projector-a"));
        assert!(err.to_string().contains("projector-b"));
    }
}
