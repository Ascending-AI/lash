//! The deployment's code cells: a [`TurnScript::Cell`] or
//! [`TurnScript::CellKilled`] session runs behind the lash facade, on the
//! production turn driver, the code mode worker path and the production tool
//! dispatch. Only the model and `ext_write`'s body are the simulator's.
//!
//! The scripted model answers a turn's first call with a TypeScript cell that
//! calls the `Once` tool `ext_write`, naming its session, and every call that
//! sees the tool's answer with [`FINAL`]. The body writes its entry to the
//! world's ledger before anything else; under [`TurnScript::CellKilled`] its
//! first entry holds forever, and the host kills the node that runs it.
//!
//! A [`TurnScript::Cell`] session's cell reaches its first save within its
//! first slice, so that save commits the model response that asked for the
//! cell (`model.done+cell.snapshot+admit`, FIG-6000). Its call races a sleep
//! longer than the matrix horizon: the call wins, leaving the sleep pending,
//! so its end must commit on its own (`cell.snapshot`). A [`TurnScript::CellKilled`]
//! session's cell first computes past its slice, so the response commits on
//! its own (`model.done`) and the cell's first save is
//! `cell.snapshot+admit`; and it is served with a host plugin's checkpoint
//! callback ([`Watch`]). With no pending work when the cell ends, its end
//! commits with `turn.commit`, even with that callback installed (FIG-6162).

use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use std::sync::{Arc, Weak};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::LlmOutputPart;
use lash_core::ToolCall;
use lash_core::ToolOutcome;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_core::runtime::durable::session::TurnServices;
use lash_durable::domain::{ExecKey, OwnerKey};
use lash_durable_test::SimClock;
use lash_execution_contracts::backend::Backend;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use lash_vocabulary::tool_contract::ExecutionPolicy;

use super::cases::run_of;
use super::services::{EXT_WRITE, FINAL, TurnScript, kill_owner, session_id};
use super::world::{BodyEntry, World};

/// The model every cell session names.
const MODEL: &str = "lash-sim-cell-model";

/// The charge units one slice of a [`TurnScript::CellKilled`] cell's run
/// may spend.
const KILLED_SLICE: u64 = 20_000;

/// The cell the model writes for `session`: one `Once` tool call naming it,
/// which a [`TurnScript::CellKilled`] session's cell makes after a stretch
/// many times [`KILLED_SLICE`] long. The regular cell races its call against
/// a sleep that cannot elapse within the matrix horizon, so its end still
/// has pending work and must commit a separate snapshot.
fn cell(session: &str) -> String {
    let killed = session.starts_with(&format!("{}-", TurnScript::CellKilled.name()));
    let stretch = if killed {
        "let n = 0;\nfor (let i = 0; i < 20000; i++) { n = n + i; }\n"
    } else {
        ""
    };
    let call = format!("tools.extWrite({{ x: 7, session: \"{session}\" }})");
    let awaited = if killed {
        call
    } else {
        format!(
            "Promise.race([{call}, sleep({})])",
            super::HORIZON.as_millis() * 2
        )
    };
    format!(
        "<typescript>\n{stretch}const written = await {awaited};\nconsole.log(written);\n</typescript>"
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
    let built = core(world, &world.backend()?, &world.clock()?, false)?;
    Ok(world.cell_core().get_or_init(|| built).clone())
}

/// The run's core for `script`'s cell sessions: [`cell_core`], or for
/// [`TurnScript::CellKilled`] the core whose sessions a host plugin
/// watches ([`Watch`]), built once over `world`'s backend.
///
/// # Errors
///
/// The run has no backend yet, or the core does not build.
pub fn cell_core_of(world: &Arc<World>, script: TurnScript) -> Result<lash::LashCore, String> {
    if script != TurnScript::CellKilled {
        return cell_core(world);
    }
    if let Some(core) = world.watched_cell_core().get() {
        return Ok(core.clone());
    }
    let built = core(world, &world.backend()?, &world.clock()?, true)?;
    Ok(world.watched_cell_core().get_or_init(|| built).clone())
}

/// The host plugin a [`TurnScript::CellKilled`] session is served with: a
/// checkpoint callback that contributes nothing. A callback outside the
/// session's protocol does not split the cell's end commit (FIG-6162).
const WATCH: &str = "lash-sim-watch";

#[derive(Clone)]
struct Watch;

impl Watch {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(WATCH)
    }
}
impl lash_core::plugin::PluginMetadata for Watch {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

impl lash_core::plugin::PluginFactory for Watch {
    fn id(&self) -> &'static str {
        WATCH
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let mut reg = lash_core::plugin::PluginRegistrar::<Watch>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        Watch::declare(&mut reg)?;
        let id = self.id();
        Ok(lash_core::plugin::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(self.as_ref().clone())),
        ))
    }
}

impl lash_core::plugin::SessionPlugin for Watch {
    fn id(&self) -> &'static str {
        WATCH
    }
}
impl Watch {
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        reg.turn()
            .checkpoint(lash_core::hook_key!("watch"), move |_this, _state| {
                Ok(Arc::new(|_| {
                    Box::pin(async { Ok(lash_core::plugin::TurnContributions::default()) })
                }))
            })?;
        Ok(())
    }
}

fn core(
    world: &Arc<World>,
    backend: &Backend,
    clock: &Arc<SimClock>,
    watched: bool,
) -> Result<lash::LashCore, String> {
    let builder = lash::LashCore::rlm_builder(
        backend.clone(),
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )
        .with_worker_service(untimed_workers(clock, watched.then_some(KILLED_SLICE))),
    )
    .serve_sessions(false)
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .data_retention(lash::DataRetention::standard())
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .execution_budgets(lash::ExecutionBudgets::recommended())
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .serve_test_llm_profile(model(Arc::downgrade(world)), metadata()?)
    .plugin(Arc::new({
        let tools = ext_write(Arc::downgrade(world))?;
        lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("tools"),
            lash::plugins::PluginSpec::new()
                .with_package_provider(
                    lash::tool_provider_key!("tools"),
                    Arc::new(lash::package::ToolPackageProvider::new(
                        "tools",
                        lash::tool_provider_key!("tools"),
                        [lash::package::Segment::new("tools")
                            .map_err(|error| error.to_string())?]
                        .into(),
                        Arc::clone(&tools),
                    )),
                )
                .with_tool_provider(lash::tool_provider_key!("tools"), tools),
        )
    }));
    let builder = if watched {
        builder.plugin(Arc::new(Watch))
    } else {
        builder
    };
    builder
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
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
            lash_core::NoProgressBudget::bounded(12),
        )))
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

/// The default worker service with its deadlines off the clock: a held
/// body keeps its cell waiting for as long as the host holds it. A worker
/// runs off the runtime, so each worker call holds `clock` while it is in
/// flight. The checkout deadline and the no-response watchdog are wall
/// time too: on a loaded host they fail a cell's setup retryably, and the
/// retried attempt enters the program again with no cut to account for it.
/// With `slice`, a run returns to its broker every `slice` charge units.
fn untimed_workers(clock: &Arc<SimClock>, slice: Option<u64>) -> lash::vm::WorkerService {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::vm::WorkerService::default().config().clone();
    if let Some(slice) = slice {
        config.tuning.slice = slice;
    }
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    config.deadlines.execution_wall = OFF_THE_CLOCK;
    config.deadlines.checkout = OFF_THE_CLOCK;
    config.protocol.no_response_watchdog = OFF_THE_CLOCK;
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

/// The prefix of what the scripted model notes in the world for each call
/// it answers after a session's cell: then the session, a space, and the
/// names the call's prompt binds, comma-separated.
pub const BOUND_AFTER_CELL: &str = "bound-after-cell:";

/// The scripted model: before the transcript holds its own call it answers
/// with the cell for the session its input names, after with [`FINAL`],
/// noting in `world` the names that call's prompt binds
/// ([`BOUND_AFTER_CELL`]).
fn model(world: Weak<World>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-cell")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let world = world.clone();
            async move {
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == LlmRole::Assistant);
                let rendered = serde_json::to_string(&request.messages).unwrap_or_default();
                let session = rendered
                    .split("go ")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap_or_default()
                    .to_owned();
                let text = if answered {
                    if let Some(world) = world.upgrade() {
                        world.note(format!(
                            "{BOUND_AFTER_CELL}{session} {}",
                            bound_names(&request).join(",")
                        ));
                    }
                    FINAL.to_owned()
                } else {
                    cell(&session)
                };
                Ok(streamed(&request, &text))
            }
        })
        .build()
        .into_handle()
}

/// The names `request`'s last `BOUND VARIABLES` section binds, but the
/// read-only `history`.
fn bound_names(request: &LlmRequest) -> Vec<String> {
    fn texts(value: &serde_json::Value, into: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    match value {
                        serde_json::Value::String(text) if key == "text" => {
                            into.push(text.clone());
                        }
                        value => texts(value, into),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|item| texts(item, into)),
            _ => {}
        }
    }
    let mut all = Vec::new();
    texts(
        &serde_json::to_value(&request.messages).unwrap_or_default(),
        &mut all,
    );
    let Some(section) = all.iter().rev().find_map(|text| {
        text.split_once("=== BOUND VARIABLES ===")
            .map(|(_, rest)| rest)
    }) else {
        return Vec::new();
    };
    section
        .split("\n===")
        .next()
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- `"))
        .filter(|entry| !entry.contains("not bound:"))
        .filter_map(|entry| entry.split_once('`').map(|(name, _)| name.to_owned()))
        .filter(|name| name != "history")
        .collect()
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
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        EXT_WRITE,
        format!("tools.{}", EXT_WRITE),
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
    .with_execution_policy(ExecutionPolicy::Once);
    Ok(Arc::new(StaticToolProvider::new(
        vec![definition],
        ExtWrite { world },
    )))
}
