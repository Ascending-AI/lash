//! The deployment's code cells: a [`TurnScript::Cell`] or
//! [`TurnScript::CellKilled`] session runs behind the lash facade, on the
//! production turn driver, the RLM worker path and the production tool
//! dispatch. Only the model and `ext_write`'s body are the simulator's.
//!
//! The scripted model answers a turn's first call with a TypeScript cell that
//! calls the `Once` tool `ext_write`, naming its session, and every call that
//! sees the tool's answer with [`FINAL`]. The body writes its entry to the
//! world's ledger before anything else; under [`TurnScript::CellKilled`] its
//! first entry holds forever, and the host kills the node that runs it.

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::{Arc, Weak};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_core::runtime::durable::session::TurnServices;
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolOutcome};
use lash_core_execution::Backend;
use lash_durable::domain::{ExecKey, OwnerKey};
use lash_durable_test::SimClock;
use lash_sansio::{SessionId, TurnId};

use super::cases::run_of;
use super::services::{EXT_WRITE, FINAL, TurnScript, kill_owner, session_id};
use super::world::{BodyEntry, World};

/// The model every cell session names.
const MODEL: &str = "lash-sim-cell-model";

/// The cell the model writes for `session`: one `Once` tool call naming it.
fn cell(session: &str) -> String {
    format!(
        "<typescript>\nconst written = await tools.{EXT_WRITE}({{ x: 7, session: \"{session}\" }});\nconsole.log(written);\n</typescript>"
    )
}

/// The run's core for its cell sessions, built once over `world`'s backend.
///
/// # Errors
///
/// The run has no backend yet, or the core does not build.
pub fn cell_core(world: &Arc<World>) -> Result<lash::LashCore, String> {
    if let Some(core) = world.cell_core().get() {
        return Ok(core.clone());
    }
    let built = core(world, &world.backend()?, &world.clock()?)?;
    Ok(world.cell_core().get_or_init(|| built).clone())
}

fn core(
    world: &Arc<World>,
    backend: &Backend,
    clock: &Arc<SimClock>,
) -> Result<lash::LashCore, String> {
    lash::LashCore::rlm_builder(
        backend.clone(),
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )
        .with_worker_service(untimed_workers(clock)),
    )
    .serve_sessions(false)
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .data_retention(lash::DataRetention::standard())
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
    .execution_budgets(lash::ExecutionBudgets::recommended())
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .serve_test_llm_profile(model(), metadata()?)
    .tools(ext_write(Arc::downgrade(world))?)
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new("lash-sim-deployment"),
        lash::persistence::LeaseIncarnationId::new("lash-sim-boot"),
    ))
    .map_err(|error| format!("build the cell core: {error}"))
}

/// The turn services `core`'s sessions run with.
#[must_use]
pub fn services(core: &lash::LashCore) -> Arc<dyn TurnServices> {
    lash::testing::session_turn_services(core)
}

/// Create `session` through `core`.
///
/// # Errors
///
/// The facade refused.
pub async fn create(
    core: &lash::LashCore,
    session: &SessionId,
) -> Result<lash::DurableSession, String> {
    core.session(session.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(8),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .map_err(|error| format!("create the cell session: {error}"))
}

/// Create `session` through `core` and send it the input `run` takes.
///
/// # Errors
///
/// The facade refused.
pub async fn send(core: &lash::LashCore, session: &SessionId, run: &TurnId) -> Result<(), String> {
    create(core, session)
        .await?
        .send(lash::TurnInput::text(format!("go {session}")))
        .id(run.clone())
        .await
        .map(drop)
        .map_err(|error| format!("send the cell turn: {error}"))
}

/// The default worker service with its run deadlines off the clock: a
/// held body keeps its cell waiting for as long as the host holds it. A
/// worker runs off the runtime, so each worker call holds `clock` while it
/// is in flight.
fn untimed_workers(clock: &Arc<SimClock>) -> lash::vm::WorkerService {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::vm::WorkerService::default().config().clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    let clock = Arc::clone(clock);
    lash::vm::WorkerService::new(config).with_call_hold(Arc::new(move || Box::new(clock.hold())))
}

fn metadata() -> Result<lash_core::LlmProfileMetadata, String> {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| error.to_string())
}

/// The scripted model: before the transcript holds its own call it answers
/// with the cell for the session its input names, after with [`FINAL`].
fn model() -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-cell")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| async move {
            let answered = request
                .messages
                .iter()
                .any(|message| message.role == LlmRole::Assistant);
            let text = if answered {
                FINAL.to_owned()
            } else {
                let rendered = serde_json::to_string(&request.messages).unwrap_or_default();
                let session = rendered
                    .split("go ")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap_or_default()
                    .to_owned();
                cell(&session)
            };
            Ok(streamed(&request, &text))
        })
        .build()
        .into_handle()
}

/// A text answer, streamed as one delta.
fn streamed(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        }));
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

/// `ext_write`: a `Once` tool whose body writes its ledger entry under the
/// cell that called it and answers what it wrote.
struct ExtWrite {
    /// Weak: the world holds the core that holds this tool.
    world: Weak<World>,
}

#[async_trait::async_trait]
impl StaticToolExecute for ExtWrite {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let Some(world) = self.world.upgrade() else {
            return ToolOutcome::ok(serde_json::json!({ "ok": false })).into();
        };
        let world = &world;
        let session = session_id(
            call.args
                .get("session")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("cell"),
        );
        let id = call.context.call_id().clone();
        let owner = cell_owner(world, &session);
        let first = !world
            .ledger()
            .entries()
            .contains_key(&(owner.clone(), id.clone()));
        let admitted = world.admitted(&owner, &id).await;
        world.ledger().enter(
            &owner,
            &id,
            BodyEntry {
                tool: EXT_WRITE.to_owned(),
                policy: ExecutionPolicy::Once,
                attempt: 1,
                at_ms: world.now_ms(),
                admitted,
            },
        );
        if TurnScript::of(&session) == Some(TurnScript::CellKilled) && first {
            kill_owner(world, &session);
            std::future::pending::<()>().await;
        }
        ToolOutcome::ok(serde_json::json!({ "ok": true, "wrote": call.args })).into()
    }
}

/// The run-record owner of `session`'s cell: the execution whose program the
/// tripwire saw entered.
fn cell_owner(world: &World, session: &SessionId) -> OwnerKey {
    world
        .tripwire()
        .counts()
        .vm_programs
        .into_keys()
        .find(|exec| matches!(exec, ExecKey::Cell(cell_session, _, _) if cell_session == session))
        .map_or_else(
            || OwnerKey::Turn(session.clone(), run_of(session)),
            |exec| exec.owner(),
        )
}

fn ext_write(world: Weak<World>) -> Result<Arc<dyn lash_core::ToolProvider>, String> {
    let definition = lash_core::ToolDefinition::raw(
        EXT_WRITE,
        EXT_WRITE,
        "Writes x to the outside world, once.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "x": { "type": "number" },
                "session": { "type": "string" }
            },
            "required": ["x", "session"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .map_err(|error| error.to_string())?
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], EXT_WRITE));
    Ok(Arc::new(StaticToolProvider::new(
        vec![definition],
        ExtWrite { world },
    )))
}
