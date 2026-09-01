//! A plugin that carries operations, authored against the `lash` facade alone.
//!
//! Nothing in this module imports `lash_core`: the query / command / task
//! vocabulary a plugin author needs is re-exported from [`lash::plugins`]
//! (ADR 0051, FIG-1921).
//! `facade_only_plugin_authoring::example_plugins_need_no_lash_core_import`
//! in `lib.rs` holds every in-tree example plugin to that.
//!
//! This module mirrors no docs page and carries no `docs:start:` regions, like
//! `effect_groups` and the `fig*` regression modules: it exists as executable
//! evidence for the facade rule and as the coverage anchor for the plugin
//! authoring surface. The prose it would otherwise duplicate lives in ADR 0051,
//! and a second copy in an HTML page is a second copy to keep in sync.

use std::sync::Arc;

use lash::plugins::{
    CodeExecutionDisposition, CodeExecutorPlugin, ExecRequest, ExecResponse, PluginCommand,
    PluginCommandContext, PluginError, PluginFactory, PluginOperation, PluginOperationFailure,
    PluginOperationInvokeError, PluginOperationOutcome, PluginOperationReceipt, PluginOwned,
    PluginQuery, PluginQueryContext, PluginRegistrar, PluginRuntimeDirective, PluginRuntimeEvent,
    PluginSessionContext, PluginSnapshotMeta, PluginTask, PluginTaskContext, ProcessReadService,
    RuntimeExecutionContext, SessionParam, SessionPlugin, SessionReadService, SessionReadyContext,
    SnapshotReader, SnapshotWriter,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const PLUGIN_ID: &str = "docs-plan";
const SESSION: &str = "docs-plugin-operations";
const PLAN_BLOB: &str = "plan.json";

/// Minimal stateful executor shape showing the response-handoff contract that
/// a protocol plugin uses to retain a cell checkpoint until Lash settles it.
#[derive(Default)]
struct DocsCodeExecutor {
    last_disposition: std::sync::Mutex<Option<CodeExecutionDisposition>>,
}

#[async_trait::async_trait]
impl CodeExecutorPlugin for DocsCodeExecutor {
    async fn execute_code(
        &self,
        ctx: RuntimeExecutionContext<'_>,
        _request: ExecRequest,
    ) -> Result<ExecResponse, lash::SessionError> {
        if ctx.is_cancelled() {
            return Err(lash::SessionError::Protocol(
                "documentation executor was cancelled".to_string(),
            ));
        }
        Ok(ExecResponse {
            observations: Vec::new(),
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            degraded_bindings: Vec::new(),
            terminal_finish: None,
        })
    }

    async fn settle_code_execution(
        &self,
        disposition: CodeExecutionDisposition,
    ) -> Result<(), lash::SessionError> {
        match disposition {
            CodeExecutionDisposition::Accepted
            | CodeExecutionDisposition::Discarded
            | CodeExecutionDisposition::Cancelled => {
                *self
                    .last_disposition
                    .lock()
                    .expect("executor disposition mutex") = Some(disposition);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct PlanArgs {
    goal: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
struct PlanView {
    steps: Vec<String>,
}

/// A read-only operation: `PluginQuery` runs without touching durable state.
struct ReadPlan;

impl PluginOperation for ReadPlan {
    const NAME: &'static str = "docs.read_plan";
    const DESCRIPTION: &'static str = "Project the plan a session is following.";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;

    type Args = PlanArgs;
    type Output = PlanView;
}

impl PluginQuery for ReadPlan {}

/// A durable operation: `PluginCommand` may append state and ask the runtime
/// to queue follow-up work through a [`PluginRuntimeDirective`].
struct RecordPlan;

impl PluginOperation for RecordPlan {
    const NAME: &'static str = "docs.record_plan";
    const DESCRIPTION: &'static str = "Record the plan and queue the turn that executes it.";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;

    type Args = PlanArgs;
    type Output = PlanView;
}

impl PluginCommand for RecordPlan {}

/// A long-running operation: `PluginTask` is handed a cancellation token.
struct ReviewPlan;

impl PluginOperation for ReviewPlan {
    const NAME: &'static str = "docs.review_plan";
    const DESCRIPTION: &'static str = "Review a recorded plan under host cancellation.";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;

    type Args = PlanArgs;
    type Output = usize;
}

impl PluginTask for ReviewPlan {}

/// The query context hands a plugin the runtime's read services; both are
/// traits, so a facade-only plugin has to be able to name them.
async fn read_plan(
    ctx: PluginQueryContext,
    args: PlanArgs,
) -> Result<PlanView, PluginOperationFailure> {
    let sessions: Arc<dyn SessionReadService> = Arc::clone(&ctx.sessions);
    // A query also holds the runtime's process reader. Listing needs an effect
    // scope, which only command and task handlers are handed.
    let _processes: Arc<dyn ProcessReadService> = Arc::clone(&ctx.processes);
    let session_id = ctx.session_id.clone().unwrap_or_default();
    let catalog = sessions
        .tool_catalog(&session_id)
        .await
        .map_err(PluginOperationFailure::from)?;
    // The rest of the read service: durable snapshots for this session or any
    // other, the shared catalog projection, and tool enable/disable state. A
    // runtime that cannot answer one of these refuses rather than guessing, so
    // a query decides for itself whether the answer is required.
    let _current = sessions.snapshot_current().await;
    let _other = sessions.snapshot_session(&session_id).await;
    let _shared = sessions.shared_tool_catalog(&session_id).await;
    let _tool_state = sessions.tool_state(&session_id).await;
    Ok(PlanView {
        steps: vec![format!("{} ({} catalog tools)", args.goal, catalog.len())],
    })
}

/// A command handler returns a [`PluginOperationOutcome`]: the typed output
/// plus the events and directives the runtime should apply for the plugin.
async fn record_plan(
    ctx: PluginCommandContext,
    args: PlanArgs,
) -> Result<PluginOperationOutcome<PlanView>, PluginOperationFailure> {
    let session_id = ctx.session_id.clone().unwrap_or_default();
    // A command is handed the durable services a query is not: read-through
    // session state, the lifecycle verbs, the graph appender, and processes.
    let _sessions = Arc::clone(&ctx.sessions);
    let _lifecycle = Arc::clone(&ctx.session_lifecycle);
    let _graph = Arc::clone(&ctx.session_graph);
    let _processes = Arc::clone(&ctx.processes);
    let view = PlanView {
        steps: vec![args.goal.clone()],
    };
    Ok(PluginOperationOutcome::new(view)
        .with_events(vec![PluginRuntimeEvent::Status {
            key: "plan".to_string(),
            label: "recorded".to_string(),
            detail: Some(session_id),
        }])
        .with_directives(vec![PluginRuntimeDirective::QueueTurn {
            input: lash::TurnInput::text(format!("execute plan: {}", args.goal)),
            source_key: Some("docs-plan-queue".to_string()),
        }]))
}

/// A task handler sees the same durable services as a command, plus the host's
/// cancellation token and an effect scope, and returns the same outcome shape.
async fn review_plan(
    ctx: PluginTaskContext,
    args: PlanArgs,
) -> Result<PluginOperationOutcome<usize>, PluginOperationFailure> {
    if ctx.cancellation_token.is_cancelled() {
        return Err(PluginOperationFailure::new("review cancelled"));
    }
    // A task carries a command's services plus its own effect scope, so the
    // effects it runs are journaled under an identity the runtime owns.
    let _session_id = ctx.session_id.clone();
    let _sessions = Arc::clone(&ctx.sessions);
    let _lifecycle = Arc::clone(&ctx.session_lifecycle);
    let _graph = Arc::clone(&ctx.session_graph);
    let _processes = Arc::clone(&ctx.processes);
    let _scope = ctx.scoped_effect_controller.clone();
    Ok(PluginOperationOutcome::new(args.goal.split(' ').count()))
}

/// The plugin itself: it registers the three operations and persists the plan
/// it accumulated so a reloaded session picks up where the last one stopped.
#[derive(Default)]
struct PlanPlugin {
    plan: std::sync::Mutex<Vec<String>>,
}

impl SessionPlugin for PlanPlugin {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.execution()
            .code_executor(Arc::new(DocsCodeExecutor::default()))?;
        reg.operations().typed_query::<ReadPlan, _, _>(read_plan)?;
        reg.operations()
            .typed_command::<RecordPlan, _, _>(record_plan)?;
        reg.operations().typed_task::<ReviewPlan, _, _>(review_plan)
    }

    fn snapshot(&self, writer: &mut dyn SnapshotWriter) -> Result<PluginSnapshotMeta, PluginError> {
        let plan = self.plan.lock().expect("plan mutex").clone();
        let encoded = serde_json::to_vec(&plan)
            .map_err(|err| PluginError::Session(format!("encode plan: {err}")))?;
        writer.write_blob(PLAN_BLOB.to_string(), encoded);
        Ok(PluginSnapshotMeta {
            plugin_id: self.id().to_string(),
            plugin_version: self.version().to_string(),
            revision: self.snapshot_revision(),
            state: None,
        })
    }

    fn snapshot_revision(&self) -> u64 {
        1
    }

    fn restore(
        &self,
        _meta: &PluginSnapshotMeta,
        reader: &dyn SnapshotReader,
    ) -> Result<(), PluginError> {
        let Some(bytes) = reader.read_blob(PLAN_BLOB) else {
            return Ok(());
        };
        let plan: Vec<String> = serde_json::from_slice(bytes)
            .map_err(|err| PluginError::Session(format!("decode plan: {err}")))?;
        *self.plan.lock().expect("plan mutex") = plan;
        Ok(())
    }

    fn session_ready(&self, _ctx: SessionReadyContext) -> Result<(), PluginError> {
        Ok(())
    }
}

struct PlanPluginFactory;

impl PluginFactory for PlanPluginFactory {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(PlanPlugin::default()))
    }
}

/// A host-side blob store standing in for whatever the embedder persists
/// plugin artifacts to. Snapshot writing and restore reading are two halves of
/// one contract, so the example implements both against the same bytes.
#[derive(Default)]
struct PlanBlobs {
    blobs: Vec<(String, Vec<u8>)>,
}

impl SnapshotWriter for PlanBlobs {
    fn write_blob(&mut self, name: String, data: Vec<u8>) {
        self.blobs.push((name, data));
    }
}

impl SnapshotReader for PlanBlobs {
    fn read_blob(&self, name: &str) -> Option<&[u8]> {
        self.blobs
            .iter()
            .find(|(blob, _)| blob == name)
            .map(|(_, data)| data.as_slice())
    }
}

/// Round-trip a plugin's durable state the way the runtime does at reload.
fn plan_survives_a_snapshot_restore_round_trip() {
    let plugin = PlanPlugin::default();
    plugin
        .plan
        .lock()
        .expect("plan mutex")
        .push("ship the facade".to_string());
    let mut blobs = PlanBlobs::default();
    let meta: PluginSnapshotMeta = plugin.snapshot(&mut blobs).expect("plan snapshots");
    assert_eq!(meta.plugin_id, PLUGIN_ID);
    assert_eq!(meta.plugin_version, "1");
    assert_eq!(meta.revision, 1);
    assert!(meta.state.is_none());

    let reloaded = PlanPlugin::default();
    reloaded.restore(&meta, &blobs).expect("plan restores");
    assert_eq!(
        *reloaded.plan.lock().expect("plan mutex"),
        vec!["ship the facade".to_string()]
    );
}

fn core() -> lash::Result<lash::LashCore> {
    lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(lash::provider::ProviderHandle::unconfigured())
        .model(
            lash::ModelSpec::builder("docs-plugin-operations-model")
                .context_window_tokens(4_096)
                .build()
                .expect("valid plugin-operations model"),
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
        .plugin(Arc::new(PlanPluginFactory))
        .build(crate::example_process_owner())
}

/// Drive all three operation kinds through the host-side entry point and
/// observe what each one hands back.
async fn plugin_operations_round_trip() -> anyhow::Result<()> {
    let core = core()?;
    let session = core.session(SESSION).open().await?;
    let operations = session.plugin_operations();

    let view = operations
        .query::<ReadPlan>(PlanArgs {
            goal: "ship the facade".to_string(),
        })
        .await?;
    assert_eq!(
        view.steps,
        vec!["ship the facade (1 catalog tools)".to_string()]
    );

    let receipt: PluginOperationReceipt<PlanView> = operations
        .run_command::<RecordPlan>(PlanArgs {
            goal: "ship the facade".to_string(),
        })
        .await?;
    assert_eq!(receipt.output.steps, vec!["ship the facade".to_string()]);
    let owned: &PluginOwned<PluginRuntimeEvent> =
        receipt.events.first().expect("the command emits an event");
    assert_eq!(owned.plugin_id, PLUGIN_ID);
    assert!(matches!(
        &owned.value,
        PluginRuntimeEvent::Status { key, label, .. } if key == "plan" && label == "recorded"
    ));
    let queued = receipt
        .pending_turn_inputs
        .first()
        .expect("the QueueTurn directive queues one turn input");
    assert_eq!(queued.source_key.as_deref(), Some("docs-plan-queue"));

    let review: PluginOperationReceipt<usize> = operations
        .run_task::<ReviewPlan>(PlanArgs {
            goal: "ship the facade".to_string(),
        })
        .await?;
    assert_eq!(review.output, 3);

    let unknown = operations
        .query_raw("docs.no_such_operation", serde_json::json!({}))
        .await
        .expect_err("an unregistered operation name is refused");
    assert!(matches!(
        unknown,
        lash::EmbedError::Control(PluginOperationInvokeError::Unknown(ref name))
            if name == "docs.no_such_operation"
    ));
    plan_survives_a_snapshot_restore_round_trip();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_plugin_operations_round_trip() {
        plugin_operations_round_trip()
            .await
            .expect("plugin-operations snippet must run");
    }
}
