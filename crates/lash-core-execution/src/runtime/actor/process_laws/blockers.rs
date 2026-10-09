//! The laws of what a process is blocked on (FIG-5553): its record lists
//! every sleep, awaited process and parked call it waits on, each with the
//! node that waits, and its observed state carries its actor's park reason
//! beside its lifecycle. None of them hands a reader a key that would
//! resolve a wait.

use super::*;
use crate::runtime::process::{ObservedProcess, ProcessWorkObserver};
use crate::{ProcessParkReason, ProcessStatus, WaitKind, WaitState};

/// `process` as a host observes it through `backend`.
async fn observed(backend: &Backend, process: &ProcessId) -> Result<ObservedProcess, LawBroken> {
    ProcessWorkObserver::new(backend.process_registry())
        .with_actor_parks(Arc::clone(backend.durable()))
        .process(process)
        .await
        .map_err(|error| LawBroken(error.to_string()))?
        .ok_or_else(|| LawBroken(format!("process {process} is not observed")))
}

/// The kind and site of each wait, in the order the record lists them.
fn blockers(waits: &[WaitState]) -> Vec<(WaitKind, Option<lash_sansio::WorkflowOccurrence>)> {
    waits
        .iter()
        .map(|wait| (wait.kind.clone(), wait.site.clone()))
        .collect()
}

/// A process whose engine sleeps releases as `waiting`, and both its record
/// and its observed state list the sleep with the instant it ends at and
/// the node that sleeps.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_sleeping_process_reads_waiting_on_its_sleep_and_its_site(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let until_ms = backend
            .durable()
            .now()
            .await?
            .after_millis(i64::try_from(LONG.as_millis()).unwrap_or(i64::MAX))
            .0;
        let mut script = payload(&tag("sleep"), "sleep");
        script["until_ms"] = json!(until_ms);
        let process = root(&backend, script).await?;
        eventually(SETTLE, "the sleeping process released waiting", || {
            settled_waiting(&backend, &process)
        })
        .await?;
        let sleeping = record(&backend, &process).await?;
        let expected = vec![(WaitKind::Sleep { until_ms }, Some(law_site(SLEEP_NODE)))];
        ensure!(
            sleeping.status() == ProcessStatus::Waiting && blockers(sleeping.waits()) == expected,
            "a sleeping process reads {:?} on {:?}",
            sleeping.status(),
            sleeping.waits()
        );
        let seen = observed(&backend, &process).await?;
        ensure!(
            seen.status() == ProcessStatus::Waiting
                && blockers(seen.waits()) == expected
                && seen.park == crate::ProcessParkState::NotParked,
            "a sleeping process is observed {:?} on {:?}, parked {:?}",
            seen.lifecycle,
            seen.waits(),
            seen.park
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}

/// A process whose engine awaits another process releases as `waiting` on
/// that process, named by its id.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_process_awaiting_a_child_reads_waiting_on_that_process(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let awaited = root(&backend, payload(&tag("awaited"), "hold")).await?;
        let mut script = payload(&tag("awaiter"), "await");
        script["await"] = json!(awaited.as_str());
        let process = root(&backend, script).await?;
        eventually(SETTLE, "the awaiting process released waiting", || {
            settled_waiting(&backend, &process)
        })
        .await?;
        let awaiting = record(&backend, &process).await?;
        let expected = vec![(
            WaitKind::Process {
                process_id: awaited.clone(),
            },
            None,
        )];
        ensure!(
            awaiting.status() == ProcessStatus::Waiting && blockers(awaiting.waits()) == expected,
            "a process awaiting {awaited} reads {:?} on {:?}",
            awaiting.status(),
            awaiting.waits()
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}

/// A process with two parked calls lists both, each with its own node, and
/// neither its record nor its observed state carries a completion key or a
/// wait id of either.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_process_with_two_parked_calls_lists_both_without_their_keys(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let process = root(&backend, payload(&tag("park-two"), "park_two")).await?;
        eventually(SETTLE, "the process released on both parked calls", || {
            settled_waiting(&backend, &process)
        })
        .await?;
        let parked = record(&backend, &process).await?;
        let mut sites = Vec::new();
        for wait in parked.waits() {
            ensure!(
                matches!(&wait.kind, WaitKind::Call { tool_id, .. } if tool_id.as_str() == LAW_PARK),
                "a parked call is recorded as {:?}",
                wait.kind
            );
            sites.push(wait.site.clone());
        }
        sites.sort_by_key(|site| site.as_ref().map(|site| site.site.node_id.clone()));
        ensure!(
            parked.status() == ProcessStatus::Waiting
                && sites == PARK_TWO.map(|step| Some(law_site(step))),
            "a process with two parked calls reads {:?} on {:?}",
            parked.status(),
            parked.waits()
        );
        let seen = observed(&backend, &process).await?;
        ensure!(
            seen.waits() == parked.waits(),
            "the observed waits {:?} are not the record's {:?}",
            seen.waits(),
            parked.waits()
        );
        let pinned = round::parked(
            &backend,
            round::CallOwner::Process(process.clone()),
        )
        .await?;
        ensure!(
            pinned.len() == 2,
            "the process has {} pinned completion waits, not two",
            pinned.len()
        );
        let reads = [
            serde_json::to_string(&parked).map_err(|error| LawBroken(error.to_string()))?,
            serde_json::to_string(&seen).map_err(|error| LawBroken(error.to_string()))?,
        ];
        for call in &pinned {
            ensure!(
                reads.iter().all(|read| !read.contains(call.key.as_str())),
                "a read of process {process} carries the completion key of call {}",
                call.call_id
            );
        }
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}

/// The kind of a process no law backend serves an engine for.
const ABSENT_ENGINE_KIND: &str = "law-absent";

/// A process claimed by a node with no engine of its kind parks: here a
/// node whose claim filter admits a kind its build serves no engine for.
/// Its observed state says so with the typed reason while its lifecycle
/// stays what its record says: a park is its actor's fact, not a lifecycle
/// state.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_process_parked_on_an_unknown_engine_shows_its_park_reason_beside_its_lifecycle(
    backend: &Backend,
) -> LawResult {
    let backend = law_backend(backend)?;
    let serving = serve_decoding(
        &backend,
        NODE,
        vec![lash_durable::FormatSet::unstarted_process(
            ABSENT_ENGINE_KIND,
        )],
    );
    let result = async {
        let process = register(
            &backend,
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: ABSENT_ENGINE_KIND.to_owned(),
                    payload: json!({}),
                },
                ProcessProvenance::host(),
                LifetimeDecision::Detached,
            )
            .with_execution_env_ref(Some(crate::testing::process_execution_env_fixture_ref())),
        )
        .await?;
        eventually(SETTLE, "the process parked", || async {
            Ok(actor_state(&backend, &process).await? == Some(ActorState::Parked))
        })
        .await?;
        let seen = observed(&backend, &process).await?;
        ensure!(
            seen.park
                == crate::ProcessParkState::Parked(ProcessParkReason::UnknownEngine {
                    kind: ABSENT_ENGINE_KIND.to_owned(),
                })
                && seen.status() == ProcessStatus::Running
                && seen.waits().is_empty(),
            "a process parked on an unknown engine is observed {:?} on {:?}, parked {:?}",
            seen.lifecycle,
            seen.waits(),
            seen.park
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}
