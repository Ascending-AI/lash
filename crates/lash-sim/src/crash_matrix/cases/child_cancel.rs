//! A turn cancel that reaches a running effect-group tool child (ADR 0105 §4,
//! FIG-3904).
//!
//! One input's root calls a tool whose attempt never answers and never
//! watches its token, so only the child's own cancel fact ends it: the
//! attempt's body races a live watch of that fact, and the body the watch
//! drops leaves the typed cancel as the attempt's recorded outcome. The host
//! cancels the root while the attempt runs; the turn's close decides the
//! child's cancel.
//!
//! The mid-journal cell kills the deployment at a seeded command the child
//! stores after its cancel (its final commit or its output): the replayed
//! child serves the recorded attempt and never runs the tool again. The
//! during-delivery cell kills it as the cancelled attempt's outcome reaches
//! the engine: that outcome was never recorded, so the replayed attempt runs
//! once more, its watch ends it at once, and the child still settles. Either
//! way the child replays the journal it left, without a mismatch, and the
//! root ends cancelled once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::{ScopeId, SessionId, TurnId};
use lash_restate_test::{CrashPoint as EngineCut, CrashRule};

use super::{Staged, TRIP_WAIT, crash_and_restart, send, session_name};
use crate::crash_matrix::invariants::{self, CustomCheck, Expected};
use crate::crash_matrix::world::{CoreBuild, CrashWorld};
use crate::crash_matrix::{CrashPoint, Seam};

const TOOL: &str = "stuck";

/// The root the host cancels while its tool child runs.
const ROOT: &str = "cancel-0";

/// The service every tool child runs on, before its build's lane suffix.
const DISPATCH: &str = "EffectGroupDispatch";

/// A tool whose attempt counts itself and then never answers, whatever its
/// token says.
struct StuckTool {
    executions: Arc<AtomicUsize>,
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Never answers.",
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        serde_json::json!({"type": "object"}),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for StuckTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

/// The scripted model of this seam: a `cancel-` root calls the stuck tool,
/// and every other input is answered as the standard model answers it.
fn tool_provider() -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("crash-matrix-child-cancel")
        .complete(|request: LlmRequest| async move {
            let latest_user = request
                .messages
                .iter()
                .rev()
                .find(|message| matches!(message.role, lash_core::llm::types::LlmRole::User))
                .and_then(|message| serde_json::to_string(message).ok())
                .unwrap_or_default();
            let roots = invariants::input_roots(&latest_user);
            let parts = if roots.iter().any(|root| root.starts_with("cancel-")) {
                vec![LlmOutputPart::ToolCall {
                    call_id: "call-1".into(),
                    tool_name: TOOL.to_owned(),
                    input_json: "{}".to_owned(),
                    replay: None,
                }]
            } else {
                vec![LlmOutputPart::Text {
                    text: roots
                        .iter()
                        .map(|root| invariants::answer_text(root))
                        .collect(),
                    response_meta: None,
                }]
            };
            Ok::<_, LlmTransportError>(LlmResponse {
                parts,
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn tool_core(executions: Arc<AtomicUsize>) -> CoreBuild {
    Arc::new(move |backend, owner| {
        let model = lash_core::ModelSpec::builder("crash-matrix-model")
            .context_window_tokens(200_000)
            .build()
            .map_err(|error| format!("model spec: {error}"))?;
        lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .recovery_lease(super::recovery_lease())
            .provider(tool_provider())
            .model(model)
            .tools(Arc::new(StuckTool {
                executions: Arc::clone(&executions),
            }) as Arc<dyn lash_core::ToolProvider>)
            .build(owner)
            .map_err(|error| format!("build the lash core: {error}"))
    })
}

/// The root's tool child invocation, once its attempt runs.
async fn running_child(
    world: &CrashWorld,
    executions: &AtomicUsize,
) -> Result<crate::crash_matrix::engine::EngineInvocation, String> {
    let deadline = tokio::time::Instant::now() + TRIP_WAIT;
    loop {
        if executions.load(Ordering::SeqCst) > 0
            && let Some(child) = world.invocations().await.into_iter().find(|view| {
                view.target.starts_with(DISPATCH)
                    && view.target.ends_with("/child")
                    && view.status != "completed"
            })
        {
            return Ok(child);
        }
        if tokio::time::Instant::now() > deadline {
            return Err("the root's tool child never ran its attempt".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The child's command count once the engine stored its running attempt's
/// command. The live engine lists notifications beside commands; a cut counts
/// commands only.
async fn stored_through_attempt(world: &CrashWorld, child: &str) -> Result<usize, String> {
    let deadline = tokio::time::Instant::now() + TRIP_WAIT;
    loop {
        let commands: Vec<String> = world
            .engine()
            .journal_names(child)
            .await
            .into_iter()
            .filter(|entry| !entry.contains("Notification"))
            .collect();
        if commands
            .last()
            .is_some_and(|entry| entry.contains(":attempt:"))
        {
            return Ok(commands.len());
        }
        if tokio::time::Instant::now() > deadline {
            return Err(format!(
                "the engine never stored the child's running attempt: {commands:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Cancel `root` through the live deployment, as the host's own work. A
/// retryable refusal is retried, as a host retries it.
async fn cancel_root(world: &CrashWorld, session: &SessionId, root: &str) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..20 {
        let core = world.core()?;
        let session = session.clone();
        let root = TurnId::from(root);
        match world
            .host_op(async move {
                // The durable session: a cancel needs no runtime of its own
                // beside the one driving the root.
                let session = core.session(session).durable().await?;
                session
                    .cancel(lash::CancelTarget::Root(root))
                    .reason("crash-matrix cancel")
                    .await
                    .map(|_| ())
            })
            .await
        {
            None | Some(Ok(())) => return Ok(()),
            Some(Err(error)) if error.is_retryable() => {
                last = error.to_string();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Some(Err(error)) => return Err(format!("the cancel was refused: {error}")),
        }
    }
    Err(format!("the cancel stayed refused retryably: {last}"))
}

/// The child's end: the root ended cancelled, the tool ran at most
/// `max_executions` times, and the child's invocation completed without a
/// journal mismatch.
fn child_settled(
    session: SessionId,
    child: String,
    executions: Arc<AtomicUsize>,
    max_executions: usize,
) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let session = session.clone();
        let child = child.clone();
        let executions = Arc::clone(&executions);
        Box::pin(async move {
            let mut violations = Vec::new();
            match world
                .backend()
                .session_store_factory()
                .root_terminal(&session, &TurnId::from(ROOT))
                .await
            {
                Ok(Some(terminal))
                    if terminal.kind == lash_core::store::RootTerminalKind::Cancelled => {}
                Ok(other) => violations.push(format!(
                    "root `{ROOT}` of `{session}` ended {:?}, not cancelled",
                    other.map(|terminal| (terminal.kind, terminal.cause))
                )),
                Err(error) => violations.push(format!("read the root's terminal: {error}")),
            }
            let ran = executions.load(Ordering::SeqCst);
            if ran == 0 || ran > max_executions {
                violations.push(format!(
                    "the tool ran {ran} time(s); at most {max_executions} is allowed"
                ));
            }
            match world
                .invocations()
                .await
                .into_iter()
                .find(|view| view.id == child)
            {
                Some(view)
                    if view.status == "completed"
                        && !view
                            .last_failure
                            .as_deref()
                            .is_some_and(|failure| failure.starts_with("570")) => {}
                Some(view) => violations.push(format!(
                    "tool child {} is {} after {} attempt(s); last failure {:?}",
                    view.target, view.status, view.attempts, view.last_failure
                )),
                None => violations.push(format!("tool child `{child}` is gone")),
            }
            violations
        })
    })
}

pub(super) async fn stage(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let executions = Arc::new(AtomicUsize::new(0));
    let world = CrashWorld::new(seed, tool_core(Arc::clone(&executions)), false).await?;
    world.restart().await?;
    let session = session_name(Seam::ChildCancel, seed);
    send(&world, &session, ROOT).await?;
    let child = running_child(&world, &executions).await?;
    // The child runs on its build's dispatch lane, a service of its own.
    let lane = child
        .target
        .split('/')
        .next()
        .map(str::to_owned)
        .ok_or_else(|| format!("tool child {} names no service", child.target))?;
    // The attempt runs once the engine stored its run command: every command
    // the child stores from there on is one it stores after its cancel.
    let stored = stored_through_attempt(&world, &child.id).await?;
    let (cut, max_executions, note) = match point {
        CrashPoint::MidJournalStep => {
            let index = stored + world.draw(0..2) as usize;
            (
                EngineCut::BeforeCommand { index },
                1,
                format!("cut=command {index} of the child (stored {stored})"),
            )
        }
        CrashPoint::DuringEngineDelivery => (
            EngineCut::BeforeRunResult { name: None },
            2,
            "cut=the cancelled attempt's run result".to_owned(),
        ),
        other => return Err(format!("the child-cancel seam has no {other:?} cell")),
    };
    world.crash_on(CrashRule::new(cut).service(lane).handler("child").times(1));
    cancel_root(&world, &session, ROOT).await?;
    let origin_ms = crash_and_restart(&world).await?;
    Ok(Staged {
        world,
        notes: vec![note],
        expected: Expected {
            closed_scopes: vec![ScopeId::turn(session.clone(), TurnId::from(ROOT))],
            live_sessions: vec![session.clone()],
            custom: vec![(
                "child_cancel",
                child_settled(session, child.id, executions, max_executions),
            )],
            ..Expected::default()
        },
        origin_ms,
    })
}
