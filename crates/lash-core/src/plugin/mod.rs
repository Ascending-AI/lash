use std::future::Future;
use std::sync::Arc;

use crate::runtime::AssembledTurn;
use crate::{
    MessageRole, ProtocolTurnOptions, SessionPolicy, ToolDefinition, ToolManifest, ToolProvider,
    ToolResult, TurnInput,
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
pub(crate) mod session_types;
mod snapshot;
mod tool_catalog;
mod trigger_registry;

pub(crate) use actions::{
    ErasedPluginCommandOutcome, ErasedPluginTaskOutcome, PluginCommandHandler,
    PluginCommandInvokeFuture, PluginQueryHandler, PluginQueryInvokeFuture, PluginTaskHandler,
    PluginTaskInvokeFuture, RegisteredPluginCommand, RegisteredPluginQuery, RegisteredPluginTask,
};
pub use actions::{
    PluginCommand, PluginCommandContext, PluginCommandOutcome, PluginCommandReceipt,
    PluginOperation, PluginOperationDef, PluginOperationFailure, PluginOperationFuture,
    PluginOperationKind, PluginQuery, PluginQueryContext, PluginRuntimeDirective, PluginTask,
    PluginTaskContext, PluginTaskOutcome, PluginTaskReceipt, ProcessReadService, SessionParam,
    SessionReadService, plugin_operation_def,
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
    ToolResultProjectionContext, ToolResultProjector, TurnHookContext, TurnResultHookContext,
    TurnResultSummary,
};
pub use protocol::{
    AssistantProseProjectorPlugin, CodeExecutorPlugin, PluginOptions, ProtocolBeforeLlmCallContext,
    ProtocolDriverPlugin, ProtocolLlmCallAction, ProtocolRuntimeContext, ProtocolSessionContext,
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
    PluginExtensionContribution, PluginExtensions, PluginFactory, PluginSessionContext, PluginSpec,
    PluginSpecBuilder, PluginSpecFactory, ProcessEngineContributionContext, SessionPlugin,
    SessionReadyContext, StaticPluginFactory,
};
pub use runtime_host::{
    AppendSessionNodesRequest, AppendSessionNodesResult, DirectCompletion, DirectLlmCompletion,
    SessionGraphService, SessionLifecycleService, SessionStateService, SessionTurnInput,
    SessionTurnRequest,
};
pub use runtime_impl::{PluginHost, SessionAuthorityContext};
#[cfg(any(test, feature = "testing"))]
pub(crate) use services::NoopSessionManager;
pub use services::{PersistentRuntimeServices, PluginOperationInvokeError, RuntimeServices};
pub use session_obj::PluginSession;
pub use session_types::{
    AgentFrameAssignment, AgentFrameId, AgentFrameReason, AgentFrameRecord, OpenAgentFrameRequest,
    OpenAgentFrameResult, PluginOwned, SessionContextOverlay, SessionCreateRequest, SessionHandle,
    SessionObservedProcessOutcome, SessionObservedProcessResult, SessionPluginSource,
    SessionRelation, SessionSnapshot, SessionStartPoint, SessionToolAccess, SubagentSessionContext,
};
pub(crate) use snapshot::{InMemorySnapshotReader, InMemorySnapshotWriter};
pub use snapshot::{
    PluginSessionSnapshot, PluginSnapshotArtifact, PluginSnapshotEntry, PluginSnapshotMeta,
    SnapshotReader, SnapshotWriter,
};
pub use tool_catalog::{
    CheckpointApplication, PluginAbort, PluginDirective, PrepareTurnRequest, ToolCatalogContext,
    TurnFinalization, TurnPreparation,
};
pub(crate) use tool_catalog::{emit_plugin_runtime_events, plugin_runtime_session_events};
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

        async fn execute(&self, call: crate::ToolCall<'_>) -> ToolResult {
            ToolResult::ok(call.args.clone())
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
                        PromptContribution::guidance("Plugin Prompt", "Structured plugin prompt"),
                        PromptContribution::guidance("Dynamic Note", "dynamic note")
                            .with_priority(1),
                    ])
                })
            }));
            let session_id = self.session_id.clone();
            reg.operations().query(
                PluginOperationDef {
                    name: "mock.echo".to_string(),
                    description: "echo".to_string(),
                    kind: PluginOperationKind::Query,
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
        let session = host.build_session("root", None).expect("session");
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

        let session = host.build_session("root", None).expect("session");
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
        let session = host.build_session("root", None).expect("session");
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
                state: SessionReadView::from_snapshot(&SessionSnapshot::default()),
                protocol_turn_options: ProtocolTurnOptions::default(),
                turn_context: crate::TurnContext::default(),
            })
            .await
            .expect("prompt contributions");
        assert_eq!(
            contributions,
            vec![
                PromptContribution::guidance("Plugin Prompt", "Structured plugin prompt"),
                PromptContribution::guidance("Dynamic Note", "dynamic note").with_priority(1),
            ]
        );
    }

    #[tokio::test]
    async fn external_query_defaults_to_current_session_when_requested() {
        let host = PluginHost::new(vec![Arc::new(MockPluginFactory)]);
        let session = host.build_session("root", None).expect("session");
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
        let session = host.build_session("root", None).expect("session");

        let def = session
            .plugin_operations()
            .into_iter()
            .find(|def| def.name == TypedEchoOp::NAME)
            .expect("typed op definition");
        assert_eq!(def.kind, PluginOperationKind::Query);
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

        let err =
            match PluginHost::new(vec![Arc::new(DuplicateFactory)]).build_session("root", None) {
                Ok(_) => panic!("duplicate typed plugin operation should fail"),
                Err(err) => err,
            };
        assert!(err.to_string().contains("duplicate plugin operation name"));
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
        let session = host.build_session("root", None).expect("session");
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
        let session = host.build_session("root", None).expect("session");

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
        let root = host.build_session("root", None).expect("root");
        let child = root.fork_for_session("child").expect("child");

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
        let _session = host.build_session("root", None).expect("session");
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
        let session = host.build_session("root", None).expect("session");
        let snapshot = session.snapshot().expect("snapshot");
        assert!(snapshot.plugins.contains_key("mock"));
        let restored = host
            .build_session("child", Some(&snapshot))
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
        let services = RuntimeServices::new(host.build_session("root", None).expect("session"));
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
        let err = match host.build_session("root", None) {
            Ok(_) => panic!("duplicate projector"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("duplicate tool result projector"));
        assert!(err.to_string().contains("projector-a"));
        assert!(err.to_string().contains("projector-b"));
    }
}
