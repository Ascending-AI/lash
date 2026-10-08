//! A store-local process start commits with its tool outcome.

use std::sync::Arc;

use lash_core_execution::runtime::ProcessLocalExecution;
use lash_core_execution::runtime::actor::round::StoreLocalEffect;
use lash_core_execution::{
    LifetimeDecision, NoProcessWork, ProcessEngineRegistration, ProcessEngineRegistry,
    ProcessInput, ProcessProvenance, ProcessRegistration, RuntimeEffectLocalExecutor, StartKey,
};
use lash_sansio::ToolCallId;

use super::engine::{KIND, SimProcessEngine, ends_at_once};
use super::world::World;

/// The key the process `call` starts is registered under.
#[must_use]
pub fn spawn_key(call: &ToolCallId) -> StartKey {
    StartKey::for_host(format!("lash-sim-spawn/{call}"))
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
    .with_execution_env_ref(Some(env));
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
