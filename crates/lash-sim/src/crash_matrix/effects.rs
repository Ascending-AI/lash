//! The store-local effects of the deployment's catalog: [`Tool::Spawn`]
//! starts a process and [`Tool::Poke`] signals one, each staged through the
//! production process executor a tool's realization runs on and committed
//! by the round with the call's outcome.
//!
//! [`Tool::Spawn`]: super::services::Tool::Spawn
//! [`Tool::Poke`]: super::services::Tool::Poke

use std::sync::Arc;

use lash_core_execution::runtime::ProcessLocalExecution;
use lash_core_execution::runtime::actor::round::StoreLocalEffect;
use lash_core_execution::{
    LifetimeDecision, NoProcessWork, ProcessEngineRegistration, ProcessEngineRegistry, ProcessId,
    ProcessInput, ProcessProvenance, ProcessRegistration, ProcessSignal, ProcessSignalIdentity,
    RuntimeEffectLocalExecutor, StartKey,
};
use lash_sansio::ToolCallId;

use super::engine::{KIND, SimProcessEngine, declared_event_types, ends_at_once, event_type, hold};
use super::world::World;

/// The signal [`Tool::Poke`](super::services::Tool::Poke) sends.
pub const POKE: &str = "poke";

/// The key the process `call` starts is registered under.
#[must_use]
pub fn spawn_key(call: &ToolCallId) -> StartKey {
    StartKey::for_host(format!("lash-sim-spawn/{call}"))
}

/// The signal `call` sends `target`.
///
/// # Errors
///
/// The identity is refused.
pub fn poke(target: &ProcessId, call: &ToolCallId) -> Result<ProcessSignal, String> {
    let identity = ProcessSignalIdentity::new(target.clone(), POKE, format!("lash-sim-{call}"))
        .map_err(|error| error.to_string())?;
    Ok(ProcessSignal::new(
        identity,
        serde_json::json!({ "poke": 1 }),
    ))
}

/// Register the process `tag` that [`Tool::Poke`](super::services::Tool::Poke)
/// signals: a holding process that declares the poke's signal.
///
/// # Errors
///
/// The registration is refused.
pub async fn register_target(world: &World, tag: &str) -> Result<ProcessId, String> {
    let mut event_types = declared_event_types().map_err(|error| error.to_string())?;
    event_types.push(event_type(&format!("signal.{POKE}")).map_err(|error| error.to_string())?);
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload: hold(tag),
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    )
    .with_execution_env_ref(Some(
        lash_core_execution::testing::process_execution_env_fixture_ref(),
    ))
    .with_extra_event_types(event_types);
    let process = world
        .backend()?
        .process_registry()
        .register_process(registration)
        .await
        .map_err(|error| error.to_string())?
        .id;
    world.track(
        lash_durable::ActorKey::process(process.as_str()).map_err(|error| error.to_string())?,
    );
    Ok(process)
}

/// The process executor a tool's realization runs on, over `world`'s
/// backend.
fn executor(world: &World) -> Result<ProcessLocalExecution, String> {
    let backend = world.backend()?;
    let registry = backend.process_registry();
    let env_store = backend.process_env_store();
    let engines = ProcessEngineRegistry::new().with_registration(
        ProcessEngineRegistration::accepting(Arc::new(SimProcessEngine)),
    );
    RuntimeEffectLocalExecutor::processes(
        Arc::clone(&registry),
        Arc::new(NoProcessWork::for_registry(registry)),
        engines,
        lash_core_execution::runtime::HostStartAdmission::default(),
    )
    .with_process_env_store(env_store)
    .into_process()
    .map_err(|error| error.to_string())
}

/// Stage the process `call` starts: the rows its outcome commits.
///
/// # Errors
///
/// The start is refused.
pub async fn spawn(world: &World, call: &ToolCallId) -> Result<Vec<StoreLocalEffect>, String> {
    let env = lash_core_execution::testing::process_execution_env_fixture(
        world.backend()?.process_env_store().as_ref(),
    )
    .await;
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload: ends_at_once("spawned"),
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    )
    .with_start_key(Some(spawn_key(call)))
    .with_execution_env_ref(Some(env))
    .with_extra_event_types(declared_event_types().map_err(|error| error.to_string())?);
    let staged = executor(world)?
        .stage_start(registration.into(), Vec::new(), Default::default())
        .await
        .map_err(|error| error.to_string())?;
    Ok(staged
        .rows
        .map(StoreLocalEffect::ProcessStart)
        .into_iter()
        .collect())
}

/// Stage `call`'s signal to `target`: the rows its outcome commits.
///
/// # Errors
///
/// The signal is refused.
pub async fn signal(
    world: &World,
    target: &ProcessId,
    call: &ToolCallId,
) -> Result<Vec<StoreLocalEffect>, String> {
    let effect = executor(world)?
        .stage_signal(&poke(target, call)?)
        .await
        .map_err(|error| error.to_string())?;
    Ok(vec![effect])
}
