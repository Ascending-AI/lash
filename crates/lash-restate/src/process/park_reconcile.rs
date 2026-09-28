//! Engine retry exhaustion becomes a process park (FIG-3675, R0c).
//!
//! A process segment's `run` invocation that keeps failing live stops after
//! its deployment's attempt bound and pauses, keeping its journal
//! ([`PROCESS_HANDLER_MAX_ATTEMPTS`](super::PROCESS_HANDLER_MAX_ATTEMPTS)).
//! Nothing in lash sees that happen: no lash code runs once the engine
//! stopped retrying. The reconcile reads the paused invocations back from
//! Restate and parks each one's process through the registry with
//! `ParkReason::EngineRetryExhausted`, carrying the invocation id as the
//! park's opaque engine handle. It writes no terminal evidence, and the
//! process keeps what it holds.
//!
//! This is the Restate half of the engine obligation "an exhausted process
//! parks with `EngineRetryExhausted` and writes no terminal evidence"
//! (L-E4/L-E6). Pause, the admin query, and resume are this engine's
//! mechanism, and the park is the engine-neutral fact.
//!
//! A paused segment of a process that is already terminal holds nothing: the
//! terminal is stored, and its publication to the process's waiters is the
//! `ProcessTerminal` obligation (ADR 0109 §3), which the relay delivers
//! through the root workflow's shared `complete_terminal` whether or not any
//! segment's `run` still exists. Once that publication is delivered the pass
//! kills the paused invocation; until then it leaves it, so a waiter is
//! always served before the invocation goes.
//!
//! A process already refusing on a divergence keeps that park and its
//! reason: its body refused first, and the pause only followed. A resume
//! ([`resume_parked_process`]) retries the paused invocation over its kept
//! journal. The first fact past the refusal (progress, or the terminal)
//! ends the park, and another exhaustion re-parks the same park.
//!
//! A segment's `run` that *finished* with a failure is the other engine stop
//! lash cannot see from inside: an operator's kill, which Restate cascades
//! from the invocation that started the run, ends it `409 killed` with no
//! lash code running. Restate runs a workflow key's `run` once, so no redrive
//! or sweep ever reaches the process again. The same pass reads those runs
//! back ([`end_lost_process_runs`]): it walks the registry's live processes
//! and asks the engine only about their current segments' runs, bounded by
//! the work lash still waits on rather than the engine's retained history,
//! so a killed run is found however many failed runs are kept since. Each
//! such process ends `Abandoned` with `ResumeRefused { SubstrateLost }`
//! (ADR 0110): its execution is lost as surely as a lost journal. The
//! terminal transaction arms the `ProcessTerminal` publication, so the
//! process's waiters are served.
//!
//! The same query finds the current segments Restate holds no run of at all:
//! a run purged, or Restate's state lost. The `ProcessStart` obligation was
//! delivered once and nothing re-arms it, so the pass resubmits the latest
//! segment itself, and that segment's admission ends a started process
//! `SubstrateLost`.

use std::sync::Arc;

use lash_core::store::{EnginePark, ObligationState, ParkReason, ProcessParkWrite};
use lash_core::{
    PluginError, ProcessContinuationStore, ProcessExecutionWriteAuthority, ProcessRecord,
    ProcessRegistry, ProcessSegmentKey,
};
use lash_sansio::ProcessId;

use crate::ingress::{
    RestateAdminClient, RestateIngressClient, RestateInvocationId, RestateInvocationStatus,
    RestatePausedInvocation,
};
use crate::services::LashService;

/// What one reconcile pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessParkReconcileReport {
    /// Processes this pass parked or re-parked for an exhausted invocation.
    pub parked: Vec<ProcessId>,
    /// Terminal processes whose paused segment this pass killed: their
    /// terminal publication was delivered, so the invocation held nothing.
    pub released: Vec<ProcessId>,
    /// Paused invocations left as they were: the process is gone, never
    /// started, already refusing on its own park, or terminal with its
    /// publication still owed.
    pub unchanged: usize,
}

/// Settle the paused `LashProcessWorkflow` invocations `paused` (ADR 0109
/// §3): park each one's live process on its kept journal, or release a
/// terminal one's invocation once its terminal publication is delivered.
/// The session-control pass feeds each paused invocation it listed through
/// here; `paused` is already the engine's listing, so this asks Restate
/// nothing.
///
/// Idempotent: a process whose park already refuses is left as it is, so a
/// repeated pass over the same pause writes nothing.
///
/// # Errors
/// When the registry or a Restate admin verb fails; a pass that failed
/// part-way is retried whole by the next one.
pub(crate) async fn reconcile_process_invocations(
    admin: &RestateAdminClient,
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn ProcessContinuationStore>,
    paused: Vec<RestatePausedInvocation>,
) -> Result<ProcessParkReconcileReport, PluginError> {
    let mut report = ProcessParkReconcileReport::default();
    for invocation in paused
        .into_iter()
        .filter(|invocation| invocation.target_handler_name == "run")
    {
        let Some((record, segment_ordinal)) = segment_process(registry, &invocation).await? else {
            report.unchanged += 1;
            continue;
        };
        if record.is_terminal() {
            if release_terminal_segment(admin, registry, &record, &invocation).await? {
                report.released.push(record.id);
            } else {
                report.unchanged += 1;
            }
            continue;
        }
        if record.is_refusing_park() {
            report.unchanged += 1;
            continue;
        }
        let Some(authority) = execution_authority(&record) else {
            report.unchanged += 1;
            continue;
        };
        // The park carries the generation of the build whose checkpoint it
        // resumes (FIG-3795 S8): the paused segment's recorded admission
        // stamp, never the reconciling build's own.
        let build_generation =
            segment_checkpoint_generation(&record, continuations, segment_ordinal).await?;
        let park = ProcessParkWrite {
            reason: exhausted_reason(&invocation),
            engine: Some(EnginePark::new(invocation.id.clone())),
            build_generation,
        };
        let parked = registry
            .park_process_with_authority(&record.id, park, &authority)
            .await?;
        let reason = lash_core::store::ParkReasonCode::EngineRetryExhausted;
        lash_core::operational_metrics::record_work_parked("process", reason.as_str());
        tracing::warn!(
            event = "process.parked",
            process_id = record.id.as_str(),
            reason_code = reason.as_str(),
            invocation_id = invocation.id.as_str(),
            attempts = parked.park.as_deref().map_or(0, |park| park.attempts),
            "a process whose engine retries ran out is parked"
        );
        report.parked.push(record.id);
    }
    Ok(report)
}

/// What one [`end_lost_process_runs`] pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LostRunPass {
    /// Processes this pass ended `SubstrateLost`.
    pub ended: Vec<ProcessId>,
    /// Processes whose current segment Restate no longer held, resubmitted
    /// by this pass: the segment's admission ends a started one
    /// `SubstrateLost` and starts one that never started.
    pub resubmitted: Vec<ProcessId>,
    /// Failed runs left as they were: the process is gone, already
    /// terminal or refusing in its park, or a later segment carries it.
    pub unchanged: usize,
    /// Runs this pass could not settle, with why: a failed run by its
    /// invocation id, a missing one by its workflow key.
    pub failed: Vec<(String, String)>,
}

/// End every live process whose current segment's `run` the engine finished
/// with a failure, and resubmit every one whose current segment Restate no
/// longer holds.
///
/// The scan is bounded by the live processes lash still waits on, never by
/// the engine's retained history: it reads the registry's non-terminal
/// records in pages of `limit`, and asks Restate once per page about those
/// processes' current segments' `run` invocations. A segment's external
/// reference names its workflow key (a handover's reference carries no
/// invocation id — the key is the owner), and a key's `run` executes once,
/// so the key identifies the one run lash waits on.
///
/// A process whose park refuses is left to the park: its body refused, and
/// the refused run ended by design. A failed run ends any other process
/// `SubstrateLost` here. A reference exists
/// only once Restate accepted a run for it, so a key Restate holds no run of
/// was purged or lost with its journal (ADR 0110). The pass resubmits the
/// process's latest segment, as the start delivered it: under a key Restate
/// no longer holds the submission runs the segment's admission, which ends a
/// started segment whose journal is gone `SubstrateLost`, starts one that
/// never started, and ignores one that already handed over. A key the query
/// missed only because Restate had not yet applied its run coalesces onto
/// that run, so a resubmission is never a second execution. At most `limit`
/// processes are resubmitted per pass; the rest wait for the next one.
///
/// Idempotent: a process this pass ended is terminal, so the next pass that
/// reads the same run leaves it. One run that fails to settle never fails
/// the pass.
///
/// # Errors
/// When the registry page read or Restate's admin query fails.
pub(crate) async fn end_lost_process_runs(
    admin: &RestateAdminClient,
    ingress: &RestateIngressClient,
    namespace: &crate::RestateNamespace,
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn ProcessContinuationStore>,
    limit: std::num::NonZeroUsize,
) -> Result<LostRunPass, PluginError> {
    let starts = super::RestateProcessIngressRunner::over_ingress(
        ingress.clone(),
        namespace.clone(),
        Arc::clone(registry),
        Arc::clone(continuations),
    );
    let mut pass = LostRunPass::default();
    let mut continuation = None;
    loop {
        let page = registry
            .list_non_terminal_processes_page(limit, continuation)
            .await?;
        continuation = page.continuation;
        let segments: Vec<(String, &ProcessRecord)> = page
            .records
            .iter()
            .filter(|record| !record.input.is_externally_owned() && !record.is_refusing_park())
            .filter_map(|record| {
                let reference = record.external_ref.as_ref()?;
                (reference.backend == "restate").then(|| {
                    (
                        super::process_segment_workflow_key(
                            &record.id,
                            reference.segment_ordinal(),
                        ),
                        record,
                    )
                })
            })
            .collect();
        let segment_keys: Vec<String> = segments.iter().map(|(key, _)| key.clone()).collect();
        let runs = admin
            .segment_runs(namespace, &segment_keys)
            .await
            .map_err(|error| {
                PluginError::Session(format!("read process runs from Restate: {error}"))
            })?;
        for run in runs.iter().filter(|run| run.completed_with_failure()) {
            match end_lost_run(registry, continuations, run).await {
                Ok(Some(process_id)) => pass.ended.push(process_id),
                Ok(None) => pass.unchanged += 1,
                Err(error) => pass.failed.push((run.id.clone(), error.to_string())),
            }
        }
        for (key, record) in segments {
            let held = runs
                .iter()
                .any(|run| run.target_service_key.as_deref() == Some(key.as_str()));
            if held || pass.resubmitted.len() >= limit.get() {
                continue;
            }
            match starts.submit_record(record).await {
                Ok(()) => pass.resubmitted.push(record.id.clone()),
                Err(error) => pass.failed.push((key, error.to_string())),
            }
        }
        if continuation.is_none() {
            return Ok(pass);
        }
    }
}

/// End the process of failed segment `run`, when the run was its current
/// segment and it is still live. The process it ended, if any.
async fn end_lost_run(
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn ProcessContinuationStore>,
    run: &RestateInvocationStatus,
) -> Result<Option<ProcessId>, PluginError> {
    let Some((record, segment_ordinal)) =
        segment_process_of_key(registry, run.target_service_key.as_deref()).await?
    else {
        return Ok(None);
    };
    // A refusing park's run failed by design: the park holds the process
    // until the drain's re-send or an operator's verb resumes it.
    if record.is_terminal() || record.is_refusing_park() {
        return Ok(None);
    }
    // A segment that handed over is carried by its successor's run.
    let latest = continuations
        .latest_segment_handover(&record.id)
        .await?
        .map_or(0, |handover| handover.segment_ordinal);
    if latest > segment_ordinal {
        return Ok(None);
    }
    let proposed = lash_core::ProcessAwaitOutput::Abandoned {
        evidence: Box::new(lash_core::AbandonEvidence {
            writer: lash_core::AbandonWriter::ResumeRefused {
                reason: lash_core::ProcessResumeRefusal::SubstrateLost,
            },
            owner: record
                .first_started
                .as_deref()
                .map(|started| started.owner.clone()),
            epoch_ms: super::restate_now_ms(),
        }),
        control: None,
    };
    let authority = lash_core::ProcessCompletionAuthority::WorkflowKeyRecovery {
        workflow_key: record.id.to_string(),
        segment_ordinal,
    };
    match registry
        .complete_process(&record.id, proposed, authority)
        .await
    {
        Ok(_) => {
            tracing::warn!(
                event = "process.run_lost",
                process_id = record.id.as_str(),
                invocation_id = run.id.as_str(),
                segment_ordinal,
                completion_failure = run.completion_failure.as_deref().unwrap_or_default(),
                "a process segment's run ended without its terminal; the process ends substrate-lost"
            );
            Ok(Some(record.id))
        }
        Err(PluginError::ProcessHandedOver { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Kill the paused segment `invocation` of terminal `record` once its
/// terminal publication is delivered. `false` while the publication is still
/// owed: the relay delivers it first.
async fn release_terminal_segment(
    admin: &RestateAdminClient,
    registry: &Arc<dyn ProcessRegistry>,
    record: &ProcessRecord,
    invocation: &RestatePausedInvocation,
) -> Result<bool, PluginError> {
    let delivered = registry
        .terminal_publication(&record.id)
        .await?
        .is_none_or(|publication| publication.state == ObligationState::Delivered);
    if !delivered {
        return Ok(false);
    }
    admin
        .kill_invocation(&invocation.invocation_id())
        .await
        .map_err(|error| {
            PluginError::Session(format!(
                "kill terminal process `{}`'s paused invocation `{}`: {error}",
                record.id, invocation.id
            ))
        })?;
    tracing::info!(
        event = "process.paused_segment_released",
        process_id = record.id.as_str(),
        invocation_id = invocation.id.as_str(),
        "a terminal process's paused segment was killed after its terminal was published"
    );
    Ok(true)
}

/// Resume the paused invocation holding `process_id`'s park: a fresh retry
/// loop over its kept journal. The park stays until that retry gets past the
/// refusal. Its first fact of progress, or its terminal, ends the park, and
/// a retry that fails again re-parks it.
///
/// # Errors
/// When the process is not parked, no paused invocation holds it, or
/// Restate refuses the resume.
pub async fn resume_parked_process(
    admin: &RestateAdminClient,
    namespace: &crate::RestateNamespace,
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Result<RestateInvocationId, PluginError> {
    let record = registry
        .get_process(process_id)
        .await?
        .ok_or_else(|| lash_core::runtime::registry_transitions::unknown_process(process_id))?;
    let Some(park) = record.park.as_deref() else {
        return Err(PluginError::Session(format!(
            "process `{process_id}` is not parked"
        )));
    };
    let invocation = match &park.engine {
        Some(engine) => RestateInvocationId::new(engine.as_str().to_string()),
        None => paused_invocation_of(admin, namespace, process_id)
            .await?
            .ok_or_else(|| {
                PluginError::Session(format!(
                    "no paused invocation holds process `{process_id}`'s park"
                ))
            })?,
    };
    admin
        .resume_invocation(&invocation)
        .await
        .map_err(|error| {
            PluginError::Session(format!(
                "resume process `{process_id}`'s invocation `{invocation}`: {error}"
            ))
        })?;
    Ok(invocation)
}

/// The paused `run` invocation of any of `process_id`'s segments.
async fn paused_invocation_of(
    admin: &RestateAdminClient,
    namespace: &crate::RestateNamespace,
    process_id: &ProcessId,
) -> Result<Option<RestateInvocationId>, PluginError> {
    let paused = admin
        .paused_invocations(&namespace.stable(LashService::ProcessWorkflow).name())
        .await
        .map_err(|error| {
            PluginError::Session(format!(
                "read paused process invocations from Restate: {error}"
            ))
        })?;
    Ok(paused
        .iter()
        .filter(|invocation| invocation.target_handler_name == "run")
        .find(|invocation| {
            invocation
                .target_service_key
                .as_deref()
                .is_some_and(|key| segment_key_names(key, process_id))
        })
        .map(RestatePausedInvocation::invocation_id))
}

/// Whether workflow key `key` is one of `process_id`'s segments:
/// `process_segment_workflow_key` spells segment 0 as the id and a later one
/// as `<id>#<ordinal>`.
fn segment_key_names(key: &str, process_id: &ProcessId) -> bool {
    key == process_id.as_str()
        || key
            .strip_prefix(process_id.as_str())
            .and_then(|rest| rest.strip_prefix('#'))
            .is_some_and(|ordinal| ordinal.parse::<u64>().is_ok())
}

/// The process a paused segment invocation runs and the segment the key
/// names, read by its workflow key.
async fn segment_process(
    registry: &Arc<dyn ProcessRegistry>,
    invocation: &RestatePausedInvocation,
) -> Result<Option<(ProcessRecord, u64)>, PluginError> {
    segment_process_of_key(registry, invocation.target_service_key.as_deref()).await
}

/// The process a segment workflow `key` runs and the segment it names.
async fn segment_process_of_key(
    registry: &Arc<dyn ProcessRegistry>,
    key: Option<&str>,
) -> Result<Option<(ProcessRecord, u64)>, PluginError> {
    let Some(key) = key else {
        return Ok(None);
    };
    // A later segment's key is `<id>#<ordinal>`; segment 0's is the id. A
    // minted id carries no `#`, so the split is exact, and a key that names
    // no minted id is no process of this registry.
    let (id, segment_ordinal) = match key.split_once('#') {
        Some((id, ordinal)) => match ordinal.parse::<u64>() {
            Ok(ordinal) => (id, ordinal),
            Err(_) => return Ok(None),
        },
        None => (key, 0),
    };
    let Ok(process_id) = ProcessId::parse(id) else {
        return Ok(None);
    };
    Ok(read(registry, &process_id)
        .await?
        .map(|record| (record, segment_ordinal)))
}

/// The generation of the build whose checkpoint the paused segment's park
/// resumes (FIG-3795 S8): segment 0's is the retained start's stamp, a later
/// segment's is the marker its admission recorded. `None` when the record
/// carries no stamp — a missing stamp is never derived.
async fn segment_checkpoint_generation(
    record: &ProcessRecord,
    continuations: &Arc<dyn ProcessContinuationStore>,
    segment_ordinal: u64,
) -> Result<Option<lash_core::engine::BuildGeneration>, PluginError> {
    if segment_ordinal == 0 {
        return Ok(record
            .first_started
            .as_deref()
            .and_then(|started| started.build_generation.clone()));
    }
    Ok(continuations
        .segment_start(&ProcessSegmentKey::new(record.id.clone(), segment_ordinal))
        .await?
        .and_then(|marker| marker.build_generation))
}

async fn read(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Result<Option<ProcessRecord>, PluginError> {
    match registry.get_process(process_id).await {
        Ok(record) => Ok(record),
        Err(PluginError::ProcessNoLongerRetained { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The execution identity the paused invocation ran under: the segment
/// admission's recorded start, whose execution the engine stopped. The
/// reconcile writes the park on that execution's behalf, which is what the
/// registry's same-execution fence checks.
pub(crate) fn execution_authority(
    record: &ProcessRecord,
) -> Option<ProcessExecutionWriteAuthority> {
    let started = record.first_started.as_deref()?;
    let execution_id = started.owner.engine_process_execution_id(&record.id)?;
    Some(
        ProcessExecutionWriteAuthority::invocation(record.id.clone(), execution_id.to_string())
            .bind_attempt(started.attempt),
    )
}

pub(crate) fn exhausted_reason(invocation: &RestatePausedInvocation) -> ParkReason {
    let attempts = invocation
        .retry_count
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or(0);
    ParkReason::EngineRetryExhausted {
        attempts,
        last_failure_code: invocation.last_failure_error_code.clone(),
        message: invocation
            .last_failure
            .clone()
            .unwrap_or_else(|| format!("the engine stopped retrying after {attempts} attempts")),
    }
}

#[cfg(test)]
mod tests {
    use super::segment_key_names;
    use lash_sansio::ProcessId;

    #[test]
    fn a_segment_key_names_its_process_and_no_other() {
        let process = ProcessId::fixture("proc");
        let other = ProcessId::fixture("other");
        let id = process.as_str();
        assert!(segment_key_names(id, &process));
        assert!(segment_key_names(&format!("{id}#3"), &process));
        assert!(!segment_key_names(&format!("{id}#x"), &process));
        assert!(!segment_key_names(&format!("{id}0"), &process));
        assert!(!segment_key_names(&format!("{other}#1"), &process));
    }
}
