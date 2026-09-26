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
//! A process already refusing on a divergence keeps that park and its
//! reason: its body refused first, and the pause only followed. A resume
//! ([`resume_parked_process`]) retries the paused invocation over its kept
//! journal. The first fact past the refusal (progress, or the terminal)
//! ends the park, and another exhaustion re-parks the same park.

use std::sync::Arc;

use lash_core::store::{EnginePark, ParkReason, ProcessParkWrite};
use lash_core::{PluginError, ProcessExecutionWriteAuthority, ProcessRecord, ProcessRegistry};
use lash_sansio::ProcessId;

use crate::ingress::{RestateAdminClient, RestateInvocationId, RestatePausedInvocation};
use crate::services::LashService;

/// What one reconcile pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessParkReconcileReport {
    /// Processes this pass parked or re-parked for an exhausted invocation.
    pub parked: Vec<ProcessId>,
    /// Paused invocations left as they were: the process is terminal, gone,
    /// never started, or already refusing on its own park.
    pub unchanged: usize,
}

/// Park the process of every paused `LashProcessWorkflow` segment.
///
/// Idempotent: a process whose park already refuses is left as it is, so a
/// repeated pass over the same pause writes nothing.
///
/// # Errors
/// When Restate's admin query or the registry fails; a pass that failed
/// part-way is retried whole by the next one.
pub async fn reconcile_process_parks(
    admin: &RestateAdminClient,
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<ProcessParkReconcileReport, PluginError> {
    let paused = admin
        .paused_invocations(LashService::ProcessWorkflow.name())
        .await
        .map_err(|error| {
            PluginError::Session(format!(
                "read paused process invocations from Restate: {error}"
            ))
        })?;
    reconcile_process_invocations(registry, paused).await
}

pub(crate) async fn reconcile_process_invocations(
    registry: &Arc<dyn ProcessRegistry>,
    paused: Vec<RestatePausedInvocation>,
) -> Result<ProcessParkReconcileReport, PluginError> {
    let mut report = ProcessParkReconcileReport::default();
    for invocation in paused
        .into_iter()
        .filter(|invocation| invocation.target_handler_name == "run")
    {
        let Some(record) = segment_process(registry, &invocation).await? else {
            report.unchanged += 1;
            continue;
        };
        if record.is_terminal() || record.is_refusing_park() {
            report.unchanged += 1;
            continue;
        }
        let Some(authority) = execution_authority(&record) else {
            report.unchanged += 1;
            continue;
        };
        let park = ProcessParkWrite {
            reason: exhausted_reason(&invocation),
            engine: Some(EnginePark::new(invocation.id.clone())),
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
        None => paused_invocation_of(admin, process_id)
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
    process_id: &ProcessId,
) -> Result<Option<RestateInvocationId>, PluginError> {
    let paused = admin
        .paused_invocations(LashService::ProcessWorkflow.name())
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

/// The process a paused segment invocation runs, read by its workflow key.
async fn segment_process(
    registry: &Arc<dyn ProcessRegistry>,
    invocation: &RestatePausedInvocation,
) -> Result<Option<ProcessRecord>, PluginError> {
    let Some(key) = invocation.target_service_key.as_deref() else {
        return Ok(None);
    };
    // A later segment's key is `<id>#<ordinal>`; segment 0's is the id. A
    // minted id carries no `#`, so the split is exact, and a key that names
    // no minted id is no process of this registry.
    let id = match key.split_once('#') {
        Some((id, ordinal)) if ordinal.parse::<u64>().is_ok() => id,
        Some(_) => return Ok(None),
        None => key,
    };
    let Ok(process_id) = ProcessId::parse(id) else {
        return Ok(None);
    };
    read(registry, &process_id).await
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
fn execution_authority(record: &ProcessRecord) -> Option<ProcessExecutionWriteAuthority> {
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
