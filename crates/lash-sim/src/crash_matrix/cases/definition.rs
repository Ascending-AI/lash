//! Starts by definition id (ADR 0113 §3.6).
//!
//! A host publishes a Lashlang module and the immutable definition of its
//! process under a pin it minted, then starts that definition by id, with a
//! host start key, from a handler of its own. The deployment dies inside the
//! start. On the double the handler replays on the next deployment, as
//! Restate retries a host service's invocation; on a live server the host's
//! job dies with the deployment, and the start's `ProcessStart` obligation is
//! what starts the process. The host releases its pin as soon as the new
//! deployment is up, so only the start's own referrers hold the definition.
//!
//! - [`CrashPoint::MidJournalStep`]: the start's registration committed and
//!   the deployment died before the engine stored the registration step's
//!   result (the crash table's "after registration, before result
//!   recording"). A replay runs the step again and the key's retained binding
//!   answers the same minted process; without one the start's obligation,
//!   still due, starts it.
//! - [`CrashPoint::AfterStateCommit`]: the registration's result is in the
//!   journal and the deployment died at the start's next step, the claim of
//!   its obligation (the table's "after recorded start"). A replay reads the
//!   recorded receipt and never registers again; without one the claim the
//!   dead attempt took lapses and the relay starts the process.
//!
//! Either way exactly one process was started from the definition, it runs
//! to its end, it names the definition, and its record holds the descriptor
//! after the pin is gone.

use std::sync::Arc;

use lash_core::ProcessId;
use lash_restate_test::{CrashPoint as EngineCut, CrashRule};
use lashlang::testing::ast_builders as b;

use super::Staged;
use crate::crash_matrix::CrashPoint;
use crate::crash_matrix::invariants::{CustomCheck, Expected};
use crate::crash_matrix::world::CrashWorld;

const PROCESS: &str = "main";

/// A published definition and the pin that holds it.
struct Published {
    id: lash_core::ProcessDefinitionId,
    pin: lash::process::HostArtifactPin,
}

/// Publish `process main() { finish 7 }` and its definition under a fresh
/// pin, as a host does before it starts a definition by id.
async fn publish_definition(world: &CrashWorld) -> Result<Published, String> {
    let program = b::module(
        vec![b::process(
            PROCESS,
            Vec::new(),
            b::block(vec![b::finish(b::num(7.0))]),
        )],
        Vec::new(),
    );
    let linked = lashlang::LinkedModule::link(
        program,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::default(),
        ),
    )
    .map_err(|error| format!("link the process: {error:?}"))?;
    let core = world.core()?;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    artifacts
        .publish_module(&pin, &linked.artifact)
        .await
        .map_err(|error| format!("publish the module: {error}"))?;
    let identity =
        lashlang::ProcessDefinitionIdentity::from_artifact_export(&linked.artifact, PROCESS)
            .ok_or_else(|| "the module exports no process".to_owned())?;
    let draft = lash_core::ProcessDefinitionDraft::new(
        lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
        identity.to_process_value(),
        [lash_core::ArtifactName {
            store: lash_core::ArtifactStoreId::LashlangModule,
            artifact_ref: linked.artifact.module_ref().as_str().to_owned(),
        }],
    )
    .map_err(|error| format!("the definition draft: {error}"))?;
    let definition = artifacts
        .publish_definition(&pin, &draft)
        .await
        .map_err(|error| format!("publish the definition: {error}"))?;
    Ok(Published {
        id: definition.id,
        pin,
    })
}

fn start_request(
    id: &lash_core::ProcessDefinitionId,
    seed: u64,
) -> Result<lash_core::ProcessStartRequest, String> {
    Ok(lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Definition {
            signature_claim: None,
            definition_id: id.clone(),
            args: serde_json::Map::new(),
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: super::process::model_spec()?,
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    ))
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_host_start_key(format!("crash-matrix-definition-start-{seed:016x}")))
}

/// The start's end state: the key names one process, the only one started
/// from the definition; it ran to its terminal, its identity names the
/// definition, and its record holds the descriptor now that the pin is gone.
fn started_once(id: lash_core::ProcessDefinitionId, seed: u64) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let id = id.clone();
        Box::pin(async move {
            let registry = world.backend().process_registry();
            let key =
                lash_core::StartKey::for_host(format!("crash-matrix-definition-start-{seed:016x}"));
            let keyed = match registry.get_process_by_start_key(&key).await {
                Ok(Some(record)) => record,
                Ok(None) => return vec!["no process holds the start key".to_owned()],
                Err(error) => return vec![format!("read the start key: {error}")],
            };
            let mut violations = Vec::new();
            if keyed.identity.definition_id.as_ref() != Some(&id) {
                violations.push(format!(
                    "process `{}` names definition {:?}, not `{id}`",
                    keyed.id, keyed.identity.definition_id
                ));
            }
            if !keyed.is_terminal() {
                violations.push(format!(
                    "process `{}` is {:?}, not terminal",
                    keyed.id, keyed.status
                ));
            }
            match registry
                .list_processes(&lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                })
                .await
            {
                Ok(records) => {
                    let started: Vec<ProcessId> = records
                        .into_iter()
                        .filter(|record| record.identity.definition_id.as_ref() == Some(&id))
                        .map(|record| record.id)
                        .collect();
                    if started != [keyed.id.clone()] {
                        violations.push(format!(
                            "the definition started {started:?}, not exactly `{}`",
                            keyed.id
                        ));
                    }
                }
                Err(error) => violations.push(format!("list processes: {error}")),
            }
            match world
                .backend()
                .process_definitions()
                .get_process_definition(&id)
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => violations.push(format!(
                    "definition `{id}` was reclaimed while process `{}` holds it",
                    keyed.id
                )),
                Err(error) => violations.push(format!("read definition `{id}`: {error}")),
            }
            violations
        })
    })
}

pub(super) async fn stage(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let world = CrashWorld::new(seed, super::process::rlm_core(), true).await?;
    world.restart().await?;
    world.replay_host_handlers();
    let published = publish_definition(&world).await?;
    let suffix = match point {
        CrashPoint::MidJournalStep => "process-start-register:v1",
        CrashPoint::AfterStateCommit => "process-start-claim:v1",
        other => return Err(format!("definition start has no {other:?} cell")),
    };
    world.crash_on(
        CrashRule::new(EngineCut::BeforeRunResultEnding {
            suffix: suffix.to_owned(),
        })
        .service(crate::crash_matrix::engine::HANDLER_HOST)
        .within_attempts(1),
    );
    let request = start_request(&published.id, seed)?;
    let answered = crate::chaos_soak::host::start_process(
        &world,
        request,
        &format!("crash-matrix-definition-start-{seed:016x}"),
    )
    .await;
    let notes = vec![format!("answered_before_crash={}", answered.is_some())];
    let origin_ms = match world.trip().wait(std::time::Duration::from_secs(20)).await {
        Some(tripped) => {
            world.crash_and_restart().await?;
            // Only the start's own referrers hold the definition from here.
            world
                .core()?
                .host_artifacts()
                .release(published.pin)
                .await
                .map_err(|error| format!("release the pin: {error}"))?;
            Some(tripped.at_ms)
        }
        None => None,
    };
    let expected = Expected {
        custom: vec![("definition_start", started_once(published.id, seed))],
        ..Expected::default()
    };
    Ok(Staged {
        world,
        notes,
        expected,
        origin_ms,
    })
}
