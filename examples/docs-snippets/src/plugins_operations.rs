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
    CodeExecutionDisposition, CodeExecutorPlugin, ExecRequest, ExecResponse, KeyRejection,
    PluginCommand, PluginCommandContext, PluginError, PluginFactory, PluginHost, PluginOperation,
    PluginOperationFailure, PluginOperationInvokeError, PluginOperationOutcome,
    PluginOperationReceipt, PluginOwned, PluginQuery, PluginQueryContext, PluginRegistrar,
    PluginRuntimeDirective, PluginRuntimeEvent, PluginSessionContext, PluginStateEdit,
    PluginStateError, PluginStateStore, PluginTask, PluginTaskContext, ProcessReadService,
    RecordedSessionConfig, RuntimeExecutionContext, SessionParam, SessionPlugin,
    SessionReadService, SessionReadyContext,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const PLUGIN_ID: &str = "docs-plan";
const SESSION: &str = "docs-plugin-operations";
const PLAN_KEY: &str = "plan";

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
    Ok(
        PluginOperationOutcome::new(args.goal.split(' ').count()).with_events(vec![
            PluginRuntimeEvent::Status {
                key: "review".into(),
                label: "completed".into(),
                detail: Some(args.goal),
            },
        ]),
    )
}

/// The plugin itself: it registers the three operations and persists the plan
/// it accumulated so a reloaded session picks up where the last one stopped.
#[derive(Default)]
struct PlanPlugin {
    state: std::sync::OnceLock<PluginStateStore>,
}

impl SessionPlugin for PlanPlugin {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let _state = reg.state();
        reg.execution()
            .code_executor(Arc::new(DocsCodeExecutor::default()))?;
        reg.operations().typed_query::<ReadPlan, _, _>(read_plan)?;
        reg.operations()
            .typed_command::<RecordPlan, _, _>(record_plan)?;
        reg.operations().typed_task::<ReviewPlan, _, _>(review_plan)
    }

    fn session_ready(&self, ctx: SessionReadyContext) -> Result<(), PluginError> {
        assert_eq!(ctx.state.plugin_id(), self.id());
        assert_eq!(ctx.state.session_id(), ctx.session_id);
        if ctx.state.get(PLAN_KEY).is_none() {
            ctx.state
                .set_as(PLAN_KEY, &vec!["ship the facade".to_string()])?;
            state_operations(ctx.state.clone())?;
        }
        self.state.set(ctx.state).expect("ready once");
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

// A state-only host never runs turns, but still declares its protocol capability.
struct StateOnlyProtocol;
impl lash::plugins::ProtocolSessionPlugin for StateOnlyProtocol {}
impl lash::plugins::ProtocolDriverPlugin for StateOnlyProtocol {
    fn build_preamble(
        &self,
        _: lash::plugins::ProtocolBuildInput,
    ) -> lash::plugins::TurnDriverPreamble {
        unreachable!("state-only witness never runs a turn")
    }
}
impl PluginFactory for StateOnlyProtocol {
    fn id(&self) -> &'static str {
        "state-only-protocol"
    }
    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(Self))
    }
}
impl SessionPlugin for StateOnlyProtocol {
    fn id(&self) -> &'static str {
        "state-only-protocol"
    }
    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.protocol().session(Arc::new(Self))?;
        reg.protocol().protocol_driver(Arc::new(Self))
    }
}

/// JSON state survives reconstruction and a child inherits an independent copy.
fn plan_survives_a_state_round_trip() {
    let host = PluginHost::new(vec![
        Arc::new(PlanPluginFactory),
        Arc::new(StateOnlyProtocol),
    ]);
    let session = host.build_session("plan-parent").expect("session");
    let state = session.export_state();
    assert_eq!(
        state.plugins[PLUGIN_ID].values[PLAN_KEY],
        serde_json::json!(["ship the facade"])
    );
    let restored = host
        .rematerialize_session(
            "plan-restored",
            &state,
            RecordedSessionConfig::new(Default::default()),
        )
        .expect("restored");
    assert_eq!(restored.export_state(), state);
}

/// A cloned capability observes writes immediately; guard failure is atomic.
fn state_operations(state: PluginStateStore) -> Result<(), PluginStateError> {
    let clone = state.clone();
    let generation = state.set("count", serde_json::json!(1))?;
    assert_eq!(clone.get("count"), Some(serde_json::json!(1)));
    assert_eq!(state.get_as::<u64>("count")?, Some(1));
    assert!(state.keys().contains(&"count".to_string()));
    state.apply_guarded(
        generation,
        vec![PluginStateEdit::Set {
            key: "count".into(),
            value: serde_json::json!(2),
        }],
    )?;
    assert!(
        matches!(state.apply_guarded(generation, vec![]), Err(PluginStateError::GenerationConflict { expected, actual }) if expected == generation && actual == generation + 1)
    );
    state.apply(vec![PluginStateEdit::Remove {
        key: "count".into(),
    }])?;
    assert_eq!(state.remove("count")?, state.generation());
    assert!(matches!(
        state.set("", serde_json::Value::Null),
        Err(PluginStateError::InvalidKey {
            reason: KeyRejection::Empty,
            ..
        })
    ));
    Ok(())
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
    assert_eq!(review.events.len(), 1);
    assert_eq!(review.events[0].plugin_id, PLUGIN_ID);
    assert!(matches!(&review.events[0].value,
        PluginRuntimeEvent::Status { key, label, detail }
        if key == "review" && label == "completed" && detail.as_deref() == Some("ship the facade")));
    assert!(review.pending_turn_inputs.is_empty());

    let cancel = lash::CancellationToken::new();
    cancel.cancel();
    let error = operations
        .run_task_with_cancel::<ReviewPlan>(
            PlanArgs {
                goal: "cancel amber 739 review".into(),
            },
            cancel,
        )
        .await
        .expect_err("cancelled review has no success receipt");
    assert!(matches!(error,
        lash::EmbedError::Control(PluginOperationInvokeError::Failed(ref message))
        if message == "review cancelled"));

    let unknown = operations
        .query_raw("docs.no_such_operation", serde_json::json!({}))
        .await
        .expect_err("an unregistered operation name is refused");
    assert!(matches!(
        unknown,
        lash::EmbedError::Control(PluginOperationInvokeError::Unknown(ref name))
            if name == "docs.no_such_operation"
    ));
    plan_survives_a_state_round_trip();
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
