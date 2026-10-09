//! The matrix's cases, one per seam, and what they share: seeding a
//! session's turn or a process, and reading back what the run left.
//!
//! A case seeds what producers outside the deployment put there before it
//! runs (a session's admitted turn, a registered process) straight into the
//! database, uncut; what the host does while the deployment runs (a cancel,
//! a resolve, a cancel, a close request) goes through the host's producer
//! store, so the matrix cuts it like any node's write.

pub mod cancel;
pub mod cell;
pub mod close;
pub mod command;
pub mod compaction;
pub mod drain;
pub mod effects;
pub mod pressure;
pub mod process;
pub mod prompt;
pub mod round;
pub mod turn;

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::{
    Ancestry, LifetimeDecision, PendingTurnInputDraft, ProcessId, ProcessInput, ProcessProvenance,
    ProcessRegistration, ScopeGrant, ScopeId, TurnInput, TurnInputIngress,
};
use lash_durable::domain::TurnEnd;
use lash_durable::{ActorKey, ActorState};
use lash_durable_test::SimNodes;
use lash_sansio::{SessionId, TurnId};
use serde_json::Value;

use super::engine::KIND;
use super::services::{TurnScript, turn_id};
use super::world::World;

/// The turn every session of the matrix runs.
#[must_use]
pub fn run_of(session: &SessionId) -> TurnId {
    turn_id(&format!("{session}-turn"))
}

/// A session's actor.
///
/// # Errors
///
/// The id is not an actor id.
pub fn session_actor(session: &SessionId) -> Result<ActorKey, String> {
    ActorKey::session(session.as_str()).map_err(|error| error.to_string())
}

/// A process's actor.
///
/// # Errors
///
/// The id is not an actor id.
pub fn process_actor(process: &ProcessId) -> Result<ActorKey, String> {
    ActorKey::process(process.as_str()).map_err(|error| error.to_string())
}

/// Admit a session of `script` named by `tag` with its one turn, seeded
/// uncut the way a host sends it: a cell or prompt session created and sent
/// through the run's core for it, a scripted one created in the catalog at
/// its creation head with its input's row, whose commit wakes its actor.
///
/// # Errors
///
/// A seed write is refused.
pub async fn admit_turn(
    world: &Arc<World>,
    script: TurnScript,
    tag: &str,
) -> Result<SessionId, String> {
    let session = script.session(tag);
    let run = run_of(&session);
    if matches!(script, TurnScript::Cell | TurnScript::CellKilled) {
        let core = super::cells::cell_core(world)?;
        super::cells::send(&core, &session, &run).await?;
    } else if script == TurnScript::Prompt {
        let core = super::prompts::prompt_core(world)?;
        super::prompts::send(&core, &session, &run).await?;
    } else {
        let catalog: Arc<dyn lash_core::store::RuntimeStore> =
            world.backend()?.session_store_factory();
        let request = lash_core_store::testing::store_fixtures::root_session_request(&session);
        through_contention(|| catalog.admit_session(&request))
            .await
            .map_err(|error| format!("admit the turn's session: {error}"))?;
        let input = PendingTurnInputDraft::new(
            session.clone(),
            TurnInputIngress::NextTurn,
            TurnInput::text("go"),
        )
        .with_source_key(run.as_str());
        through_contention(|| catalog.enqueue_pending_turn_input(input.clone()))
            .await
            .map_err(|error| format!("send the turn's input: {error}"))?;
    }
    world.track(session_actor(&session)?);
    Ok(session)
}

/// How long a host keeps re-sending a write its store answers `Contended`:
/// past the longest lock-timeout storm a soak holds the writer fence for.
const CONTENDED_FOR: Duration = Duration::from_secs(60);
/// The host's pause between two sends of a contended write.
const CONTENDED_RETRY: Duration = Duration::from_millis(50);

/// Run a host's store write as a host does: while another session holds
/// the writer fence (a soak's lock-timeout storm), the store answers
/// `Contended` and asks for the identical write again. The storm ends in
/// wall time, and the seeding host blocks the driver that moves virtual
/// time, so the host waits in wall time too.
async fn through_contention<T, F, Fut>(mut write: F) -> Result<T, lash_core::StoreError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, lash_core::StoreError>>,
{
    let started = std::time::Instant::now();
    loop {
        match write().await {
            Err(lash_core::StoreError::Contended) if started.elapsed() < CONTENDED_FOR => {
                tokio::time::sleep(CONTENDED_RETRY).await;
            }
            answer => return answer,
        }
    }
}

/// Register a process of the simulator's engine starting from `payload`,
/// living `Until` `scope` or detached: its registry row and its actor,
/// ready, seeded uncut.
///
/// # Errors
///
/// The registration is refused.
pub async fn register(
    world: &World,
    payload: Value,
    scope: Option<ScopeId>,
) -> Result<ProcessId, String> {
    let lifetime = match &scope {
        Some(scope) => LifetimeDecision::Until {
            scope: scope.clone(),
            grant: ScopeGrant::Ancestor,
        },
        None => LifetimeDecision::Detached,
    };
    let mut registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload,
        },
        ProcessProvenance::host(),
        lifetime,
    )
    .with_execution_env_ref(Some(
        lash_core_execution::testing::process_execution_env_fixture_ref(),
    ));
    if let Some(scope) = scope {
        registration.ancestry = Ancestry::from_scopes([scope]);
    }
    let process = world
        .backend()?
        .process_registry()
        .register_process(registration)
        .await
        .map_err(|error| error.to_string())?
        .id;
    world.track(process_actor(&process)?);
    Ok(process)
}

/// How `session`'s turn ended, once it has.
pub async fn turn_end(nodes: &SimNodes, session: &SessionId) -> Option<TurnEnd> {
    nodes
        .database()
        .turn_end(session, &run_of(session))
        .await
        .ok()
        .flatten()
}

/// `actor`'s state now.
pub async fn state(nodes: &SimNodes, actor: &ActorKey) -> Option<ActorState> {
    nodes
        .database()
        .actor(actor)
        .await
        .ok()
        .flatten()
        .map(|snapshot| snapshot.state)
}

/// Whether `session`'s turn ended and its actor released with nothing left
/// to do.
pub async fn turn_settled(nodes: &SimNodes, session: &SessionId) -> bool {
    let Ok(actor) = session_actor(session) else {
        return false;
    };
    turn_end(nodes, session).await.is_some()
        && matches!(
            state(nodes, &actor).await,
            Some(ActorState::Idle | ActorState::Terminal)
        )
}

/// Whether every one of `processes` is terminal.
pub async fn ended(nodes: &SimNodes, processes: &[ProcessId]) -> bool {
    for process in processes {
        let Ok(actor) = process_actor(process) else {
            return false;
        };
        if state(nodes, &actor).await != Some(ActorState::Terminal) {
            return false;
        }
    }
    true
}

/// `process`'s outcome as its awaiters read it, once it ended.
pub async fn outcome(world: &World, process: &ProcessId) -> Option<Value> {
    let record = world
        .backend()
        .ok()?
        .process_registry()
        .get_process(process)
        .await
        .ok()??;
    record
        .terminal()
        .and_then(|terminal| serde_json::to_value(terminal.clone().into_await_output()).ok())
}

/// The types of `process`'s events, oldest first.
pub async fn event_types(world: &World, process: &ProcessId) -> Vec<String> {
    let Ok(backend) = world.backend() else {
        return Vec::new();
    };
    let mut events = backend
        .process_registry()
        .recent_events(process, 64)
        .await
        .unwrap_or_default();
    events.sort_by_key(|event| event.sequence);
    events
        .into_iter()
        .map(|event| event.fact.event_type().to_owned())
        .collect()
}

/// The first value under `key` anywhere in `value`.
#[must_use]
pub fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(entries) => entries
            .get(key)
            .or_else(|| entries.values().find_map(|nested| find(nested, key))),
        Value::Array(items) => items.iter().find_map(|nested| find(nested, key)),
        _ => None,
    }
}

/// How often a host task looks again, and for how many looks at most.
pub const LOOK_EVERY: Duration = Duration::from_millis(100);
pub const LOOKS: usize = 1_200;
