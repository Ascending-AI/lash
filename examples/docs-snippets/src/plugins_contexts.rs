//! Facade-only evidence for plugin hook contexts and operation discovery.
//!
//! A plugin author names every type in this module through `lash::plugins`;
//! the module deliberately has no `lash_core` dependency.

use std::sync::{Arc, Mutex, OnceLock};

use lash::plugins::{
    AssistantStreamFinishReason, AssistantStreamFinishedContext, PluginCommand,
    PluginCommandContext, PluginError, PluginExtensionContribution, PluginExtensions,
    PluginFactory, PluginHost, PluginOperation, PluginOperationDef, PluginOperationFailure,
    PluginOperationKind, PluginOperationOutcome, PluginQuery, PluginQueryContext, PluginRegistrar,
    PluginSession, PluginSessionContext, PluginTask, PluginTaskContext, SessionParam,
    SessionPlugin, SessionReadyContext, SessionToolAccess, SubagentSessionContext,
    ToolCatalogContext, ToolCatalogContribution, ToolResultProjectionContext,
};

const SESSION: &str = "docs-plugin-contexts";

struct InspectQuery;

impl PluginOperation for InspectQuery {
    const NAME: &'static str = "docs.inspect_query";
    const DESCRIPTION: &'static str = "Inspect a plugin query definition.";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;

    type Args = ();
    type Output = ();
}

impl PluginQuery for InspectQuery {}

struct InspectCommand;

impl PluginOperation for InspectCommand {
    const NAME: &'static str = "docs.inspect_command";
    const DESCRIPTION: &'static str = "Inspect a plugin command definition.";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;

    type Args = ();
    type Output = ();
}

impl PluginCommand for InspectCommand {}

struct InspectTask;

impl PluginOperation for InspectTask {
    const NAME: &'static str = "docs.inspect_task";
    const DESCRIPTION: &'static str = "Inspect a plugin task definition.";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;

    type Args = ();
    type Output = ();
}

impl PluginTask for InspectTask {}

async fn inspect_query(_ctx: PluginQueryContext, _args: ()) -> Result<(), PluginOperationFailure> {
    Ok(())
}

async fn inspect_command(
    _ctx: PluginCommandContext,
    _args: (),
) -> Result<PluginOperationOutcome<()>, PluginOperationFailure> {
    Ok(PluginOperationOutcome::new(()))
}

async fn inspect_task(
    _ctx: PluginTaskContext,
    _args: (),
) -> Result<PluginOperationOutcome<()>, PluginOperationFailure> {
    Ok(PluginOperationOutcome::new(()))
}

struct ContextPlugin;

impl SessionPlugin for ContextPlugin {
    fn id(&self) -> &'static str {
        "docs-plugin-contexts"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.tool_catalog()
            .contribute(Arc::new(|ctx: ToolCatalogContext| {
                let access: &SessionToolAccess = &ctx.tool_access;
                let subagent: Option<&SubagentSessionContext> = ctx.subagent.as_ref();
                let extensions: &PluginExtensions = &ctx.extensions;

                assert_eq!(ctx.session_id, SESSION);
                let unresolved_contract = ctx
                    .resolve_contract
                    .as_ref()
                    .and_then(|resolve| resolve("docs.missing"));
                assert!(unresolved_contract.is_none());
                assert!(!ctx.tools.is_empty());
                assert!(access.tools.is_empty());
                assert!(access.hidden_tools.is_empty());
                assert!(!access.hides("docs.inspect_query"));
                assert!(subagent.is_none());
                assert!(extensions.payloads("docs.missing").is_empty());
                Ok(ToolCatalogContribution::default())
            }));
        reg.tool_results().projector(Arc::new(
            |ctx: ToolResultProjectionContext| {
                Box::pin(async move {
                    let projected = ctx.output.value_for_projection();
                    Err(PluginError::Session(format!(
                        "projection example: session={} call={} tool={} args={} output={} duration_ms={}",
                        ctx.session_id,
                        ctx.call_id,
                        ctx.tool_name,
                        ctx.args,
                        projected,
                        ctx.duration_ms,
                    )))
                })
            },
        ))?;
        reg.output()
            .stream_finished(Arc::new(|ctx: AssistantStreamFinishedContext| {
                Box::pin(async move {
                    let _observed_finish = (
                        ctx.session_id,
                        match ctx.reason {
                            AssistantStreamFinishReason::AttemptReset => "attempt-reset",
                            AssistantStreamFinishReason::Complete => "complete",
                            AssistantStreamFinishReason::Aborted => "aborted",
                            AssistantStreamFinishReason::Cancelled => "cancelled",
                            AssistantStreamFinishReason::ProviderError => "provider-error",
                        },
                    );
                    Ok(())
                })
            }));
        reg.operations()
            .typed_query::<InspectQuery, _, _>(inspect_query)?;
        reg.operations()
            .typed_command::<InspectCommand, _, _>(inspect_command)?;
        reg.operations()
            .typed_task::<InspectTask, _, _>(inspect_task)
    }

    fn session_ready(&self, ctx: SessionReadyContext) -> Result<(), PluginError> {
        *session_host()
            .lock()
            .expect("the docs session-host capture lock must be healthy") = Some(ctx.host);
        Ok(())
    }
}

struct ContextPluginFactory;

impl PluginFactory for ContextPluginFactory {
    fn id(&self) -> &'static str {
        "docs-plugin-contexts"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(ContextPlugin))
    }
}

fn facade_context_values_are_nameable() {
    let access = SessionToolAccess {
        tools: Vec::new(),
        hidden_tools: ["interactive".to_string()].into_iter().collect(),
    };
    assert!(SessionToolAccess::hides(&access, "interactive"));
    assert_eq!(access.tools.len(), 0);
    assert_eq!(access.hidden_tools.len(), 1);

    let subagent = SubagentSessionContext {
        parent_session_id: "parent".to_string(),
        capability: "review".to_string(),
        depth: 2,
        max_depth: 5,
    };
    assert_eq!(subagent.parent_session_id, "parent");
    assert_eq!(subagent.capability, "review");
    assert_eq!(subagent.depth, 2);
    assert_eq!(subagent.max_depth, 5);

    let mut extensions =
        PluginExtensions::from_contributions([PluginExtensionContribution::from_value(
            "docs.context",
            serde_json::json!({ "revision": 1 }),
        )]);
    extensions.insert(PluginExtensionContribution::from_value(
        "docs.context",
        serde_json::json!({ "revision": 2 }),
    ));
    let extension_id = "docs.context";
    assert_eq!(
        PluginExtensions::payloads(&extensions, extension_id).len(),
        2
    );
}

async fn inspect_plugin_session(session: &PluginSession) -> Result<(), PluginError> {
    let definitions: Vec<PluginOperationDef> = session.plugin_operations();

    for (name, expected_kind) in [
        (InspectQuery::NAME, PluginOperationKind::Query),
        (InspectCommand::NAME, PluginOperationKind::Command),
        (InspectTask::NAME, PluginOperationKind::Task),
    ] {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .expect("the registered operation must be discoverable");
        assert_eq!(definition.name, name);
        assert_eq!(PluginOperationDef::kind(definition), expected_kind);
        assert!(!definition.description.is_empty());
        assert_eq!(definition.session_param, SessionParam::Optional);
        assert!(definition.input_schema.is_object());
        assert!(definition.output_schema.is_object());
    }
    let kinds: Vec<PluginOperationKind> =
        definitions.iter().map(PluginOperationDef::kind).collect();
    assert!(kinds.contains(&PluginOperationKind::Query));
    assert!(kinds.contains(&PluginOperationKind::Command));
    assert!(kinds.contains(&PluginOperationKind::Task));

    let catalog = session.resolved_tool_catalog(SESSION)?;
    assert!(!catalog.tools.is_empty());

    let projection_error = session
        .project_tool_result(ToolResultProjectionContext {
            session_id: SESSION.to_string(),
            call_id: "call-docs".to_string(),
            tool_name: "docs.inspect_query".to_string(),
            args: serde_json::json!({ "goal": "ship the facade" }),
            output: lash::tools::ToolCallOutput::success("project me"),
            duration_ms: 7,
        })
        .await
        .expect_err("the example projector returns its observed context as an error");
    let rendered = projection_error.to_string();
    assert!(rendered.contains("session=docs-plugin-contexts"));
    assert!(rendered.contains("call=call-docs"));
    assert!(rendered.contains("tool=docs.inspect_query"));
    assert!(rendered.contains(r#"args={"goal":"ship the facade"}"#));
    assert!(rendered.contains("output=\"project me\""));
    assert!(rendered.contains("duration_ms=7"));
    Ok(())
}

fn core() -> lash::Result<lash::LashCore> {
    lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .configure_plugins(|plugins| {
            plugins.remove("tool_output_budget");
        })
        .provider(lash::provider::ProviderHandle::unconfigured())
        .model(
            lash::ModelSpec::builder("docs-plugin-contexts-model")
                .context_window_tokens(4_096)
                .build()
                .expect("valid plugin-contexts model"),
        )
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .without_queued_work()
        .plugin(Arc::new(ContextPluginFactory))
        .build(crate::example_process_owner())
}

async fn plugin_contexts_are_nameable() -> anyhow::Result<()> {
    facade_context_values_are_nameable();
    let core = core()?;
    let session = core.session(SESSION).open().await?;
    let host = session_host()
        .lock()
        .expect("the docs session-host capture lock must be healthy")
        .clone()
        .expect("opening the session must run the ready hook");
    let plugin_session = host.build_session(format!("{SESSION}-inspection"))?;
    inspect_plugin_session(&plugin_session).await?;
    session
        .plugin_operations()
        .query::<InspectQuery>(())
        .await?;
    Ok(())
}

fn session_host() -> &'static Mutex<Option<PluginHost>> {
    static SESSION_HOST: OnceLock<Mutex<Option<PluginHost>>> = OnceLock::new();
    SESSION_HOST.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn facade_names_hook_contexts_and_operation_definitions() {
        plugin_contexts_are_nameable()
            .await
            .expect("plugin context facade evidence must run");
    }
}
