//! Starts by definition id (ADR 0113 §3.6).
//!
//! A host publishes a Lashlang module and the immutable definition of its
//! process under a pin it minted, then starts that definition by id, with a
//! host start key, from a handler of its own. The deployment dies inside the
//! start. On the double the handler replays on the next deployment, as
//! Restate retries a host service's invocation; on a live server the host's
//! job dies with the deployment, and the start's `ProcessStart` obligation is
//! what starts the process. Once the start has registered, only its own
//! referrers hold the definition: the host releases its pin.
//!
//! - [`CrashPoint::DuringEngineDelivery`]: the deployment died as the start's
//!   registration step reached the engine, before the engine stored it (the
//!   crash table's "before start admission"). The dead attempt may have done
//!   nothing, held the closure under `Start(key)` only, or registered. On the
//!   double the replayed handler stages again and admits the one process.
//!   Live, a start that registered is started by its obligation; one that did
//!   not admitted no process, and whatever it held under `Start(key)` is
//!   released with the dead start, so once the host drops its pin the
//!   definition is reclaimed. The host keeps its pin until the start has
//!   settled either way: an id alone retains nothing, and this row's claim is
//!   admission, not availability.
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
//! In the last two exactly one process was started from the definition, it
//! runs to its end, it names the definition, and its record holds the
//! descriptor after the pin, released as soon as the new deployment is up,
//! is gone. In the first at most one was, and never two.

use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::sync::Arc;

use lash_core::ProcessId;
use lash_core::sync::MutexExt;
use lash_restate_test::{CrashPoint as EngineCut, CrashRule};
use lashlang::testing::ast_builders as b;

use super::Staged;
use crate::crash_matrix::CrashPoint;
use crate::crash_matrix::catalog_audit;
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
    let response = lash_vm_client::service::Service::default()
        .request(lash_vm_client::service::Request::LinkAst {
            source: String::new(),
            program,
            environment: lashlang::LashlangHostEnvironment::new(
                lashlang::LashlangHostCatalog::new(),
                lashlang::LashlangAbilities::default(),
            ),
        })
        .map_err(|error| format!("link the process: {error:?}"))?;
    let lash_vm_client::service::Response::Module(linked) = response else {
        return Err(format!("worker refused the process: {response:?}"));
    };
    let core = world.core()?;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    artifacts
        .publish_module(&pin, &linked.artifact)
        .await
        .map_err(|error| format!("publish the module: {error}"))?;
    let identity = linked
        .artifact
        .definition_identity(PROCESS)
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

async fn start_request(
    world: &CrashWorld,
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
    .with_env_ref(
        lash_core::publish_process_execution_env(
            world.backend().process_env_store().as_ref(),
            &lash_core::testing::host_pin_claim_for_testing(),
            &(lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: super::process::model_spec()?,
                    ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
            )),
        )
        .await
        .map_err(|error| format!("publish captured environment: {error}"))?,
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_host_start_key(format!("crash-matrix-definition-start-{seed:016x}")))
}

fn start_key(seed: u64) -> lash_core::StartKey {
    lash_core::StartKey::for_host(format!("crash-matrix-definition-start-{seed:016x}"))
}

/// The process the start's key names, if any, and every process started
/// from `id`.
async fn started_from(
    world: &CrashWorld,
    id: &lash_core::ProcessDefinitionId,
    seed: u64,
) -> Result<(Option<lash_core::ProcessRecord>, Vec<ProcessId>), String> {
    let registry = world.backend().process_registry();
    let keyed = registry
        .get_process_by_start_key(&start_key(seed))
        .await
        .map_err(|error| format!("read the start key: {error}"))?;
    let started = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..Default::default()
        })
        .await
        .map_err(|error| format!("list processes: {error}"))?
        .into_iter()
        .filter(|record| record.identity.definition_id.as_ref() == Some(id))
        .map(|record| record.id)
        .collect();
    Ok((keyed, started))
}

/// A start cut before the engine stored its registration step: at most one
/// process under the key, and on the double, whose handler replays, exactly
/// one. The host releases its pin once the start has settled — its process
/// ran to its end, or no host job of it is left to admit one. Then the
/// definition is held exactly when a process was admitted: by its record,
/// and otherwise by nothing, `Start(key)` included.
fn admitted_at_most_once(published: Published, seed: u64) -> CustomCheck {
    let id = published.id;
    let pin = Arc::new(std::sync::Mutex::new(Some(published.pin)));
    Arc::new(move |world: &CrashWorld| {
        let id = id.clone();
        let pin = Arc::clone(&pin);
        Box::pin(async move {
            let (keyed, started) = match started_from(world, &id, seed).await {
                Ok(read) => read,
                Err(error) => return vec![error],
            };
            let mut violations = Vec::new();
            match &keyed {
                Some(record) => {
                    if record.identity.definition_id.as_ref() != Some(&id) {
                        violations.push(format!(
                            "process `{}` names definition {:?}, not `{id}`",
                            record.id, record.identity.definition_id
                        ));
                    }
                    if started != [record.id.clone()] {
                        violations.push(format!(
                            "the definition started {started:?}, not exactly `{}`",
                            record.id
                        ));
                    }
                    if !record.is_terminal() {
                        return vec![format!(
                            "process `{}` is {:?}, not terminal",
                            record.id, record.status
                        )];
                    }
                }
                None => {
                    if !started.is_empty() {
                        violations.push(format!(
                            "the definition started {started:?} under no start key"
                        ));
                    }
                    if matches!(
                        world.engine(),
                        crate::crash_matrix::engine::Engine::Double(_)
                    ) {
                        return vec!["the replayed host start admitted no process".to_owned()];
                    }
                    let open_host_jobs = world
                        .invocations()
                        .await
                        .into_iter()
                        .filter(|view| {
                            view.status != "completed"
                                && view
                                    .target
                                    .starts_with(crate::crash_matrix::engine::HANDLER_HOST)
                        })
                        .count();
                    if open_host_jobs > 0 {
                        return vec![format!(
                            "{open_host_jobs} host job(s) of the start are still open"
                        )];
                    }
                }
            }
            let released = pin.lock_recover().take();
            if let Some(released) = released {
                let released = match world.core() {
                    Ok(core) => core.host_artifacts().release(released).await,
                    Err(error) => return vec![format!("release the pin: {error}")],
                };
                if let Err(error) = released {
                    return vec![format!("release the pin: {error}")];
                }
            }
            match (
                world
                    .backend()
                    .definition_store()
                    .get_process_definition(&id)
                    .await,
                &keyed,
            ) {
                (Ok(Some(_)), None) => violations.push(format!(
                    "definition `{id}` is still stored with no process and no pin: the dead \
                     start holds it"
                )),
                (Ok(None), Some(record)) => violations.push(format!(
                    "definition `{id}` was reclaimed while process `{}` holds it",
                    record.id
                )),
                (Ok(_), _) => {}
                (Err(error), _) => violations.push(format!("read definition `{id}`: {error}")),
            }
            violations
        })
    })
}

/// The start's end state: the key names one process, the only one started
/// from the definition; it ran to its terminal, its identity names the
/// definition, and its record holds the descriptor now that the pin is gone.
fn started_once(id: lash_core::ProcessDefinitionId, seed: u64) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let id = id.clone();
        Box::pin(async move {
            let (keyed, started) = match started_from(world, &id, seed).await {
                Ok((Some(keyed), started)) => (keyed, started),
                Ok((None, _)) => return vec!["no process holds the start key".to_owned()],
                Err(error) => return vec![error],
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
            if started != [keyed.id.clone()] {
                violations.push(format!(
                    "the definition started {started:?}, not exactly `{}`",
                    keyed.id
                ));
            }
            match world
                .backend()
                .definition_store()
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
    // The audit reads the published world once before the crash, where the
    // descriptor is certain to be stored: a start cut before admission may
    // end with the definition reclaimed and nothing left to read.
    let catalog_names = catalog_audit::CatalogAudit::default();
    let before_crash = catalog_names.read(&world).await;
    if !before_crash.is_empty() {
        return Err(format!(
            "the catalog audit before the crash: {before_crash:?}"
        ));
    }
    let cut = match point {
        CrashPoint::DuringEngineDelivery => EngineCut::BeforeRunEnding {
            suffix: "process-start-register:v1".to_owned(),
        },
        CrashPoint::MidJournalStep => EngineCut::BeforeRunResultEnding {
            suffix: "process-start-register:v1".to_owned(),
        },
        CrashPoint::AfterStateCommit => EngineCut::BeforeRunResultEnding {
            suffix: "process-start-claim:v1".to_owned(),
        },
        other => return Err(format!("definition start has no {other:?} cell")),
    };
    world.crash_on(
        CrashRule::new(cut)
            .service(crate::crash_matrix::engine::HANDLER_HOST)
            .within_attempts(1),
    );
    let request = start_request(&world, &published.id, seed).await?;
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
            Some(tripped.at_ms)
        }
        None => None,
    };
    let definition_start = if point == CrashPoint::DuringEngineDelivery {
        admitted_at_most_once(published, seed)
    } else {
        if origin_ms.is_some() {
            // Only the start's own referrers hold the definition from here.
            world
                .core()?
                .host_artifacts()
                .release(published.pin)
                .await
                .map_err(|error| format!("release the pin: {error}"))?;
        }
        started_once(published.id, seed)
    };
    let expected = Expected {
        custom: vec![("definition_start", definition_start)],
        audits: vec![("catalog_names", catalog_names.check())],
        ..Expected::default()
    };
    Ok(Staged {
        world,
        notes,
        expected,
        origin_ms,
    })
}
