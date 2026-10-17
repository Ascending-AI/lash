//! A process's effect occurrences: each settled step's effect recorded
//! once in the process's event log, and the occurrences counted past the
//! per-node cap recorded with its terminal.

use lash_durable::domain::ProcessWrite;
use lash_durable::{ActorTx, DomainWrite};

use super::driver::{Driver, InFlight};
use crate::ProcessId;
use crate::runtime::process::engine_state::StepRequest;
use lash_core_store::tool_run::SettledOutput;

/// Record `step`'s settled `outcome` on `tx` as an occurrence of the effect
/// node it runs for, in the transaction that hands it to the engine: once,
/// under its call, so a recomputed pass appends nothing new. An occurrence
/// past the per-node cap is counted instead, and the counts are recorded
/// with the terminal. A step that names no node records nothing.
pub(super) fn record_effect(
    tx: &mut ActorTx,
    process: &ProcessId,
    driver: &mut Driver,
    step: &InFlight,
    outcome: &SettledOutput,
    fleet: crate::FleetFormat,
) {
    let Some(site) = step.request.site() else {
        return;
    };
    let Some((class, code)) = effect_class(outcome) else {
        return;
    };
    // The cap is per node, whichever of the node's sites each occurrence
    // ran at: a site's own occurrence number says nothing about it.
    let recorded = driver
        .recorded_effects
        .get(site.site.to_string().as_str())
        .copied()
        .unwrap_or_default();
    if recorded >= driver.effect_occurrence_cap {
        driver
            .omitted_effects
            .entry(site.site.to_string())
            .or_default()
            .record(class);
        return;
    }
    driver
        .recorded_effects
        .insert(site.site.to_string(), recorded + 1);
    let operation = step.request.operation();
    let mut occurrence = crate::runtime::process::ProcessEffectOccurrence::new(
        site.clone(),
        operation,
        class,
        code,
        format!("process:{process}:effect:{}", step.call),
        fleet,
    );
    occurrence.call_id = match &step.request {
        StepRequest::Tool { .. } => Some(step.call.clone()),
        StepRequest::Engine { .. } => None,
    };
    append_event(tx, process, occurrence.append_request());
}

/// How a settled step's effect ended, by the tool output it settled with or
/// the answer every reader gives a stopped step; `None` for a park, which
/// settles nothing.
fn effect_class(
    outcome: &SettledOutput,
) -> Option<(
    crate::runtime::process::ProcessEffectOutcomeClass,
    Option<crate::FailureCode>,
)> {
    use crate::runtime::process::ProcessEffectOutcomeClass as Class;
    let output = match outcome {
        SettledOutput::Waiting(_) => return None,
        SettledOutput::Completed(output) => {
            serde_json::from_str::<crate::ToolCallOutput>(output.payload()).ok()
        }
        SettledOutput::Failed(failure) => {
            serde_json::from_str::<crate::ToolCallOutput>(failure.payload()).ok()
        }
        stopped => stopped.stopped_answer(),
    };
    Some(match output.map(|output| output.outcome) {
        Some(crate::ToolCallOutcome::Success(_)) => (Class::Success, None),
        Some(crate::ToolCallOutcome::Failure(failure)) => (
            Class::Failure,
            Some(crate::runtime::process::tool_failure_code(&failure)),
        ),
        Some(crate::ToolCallOutcome::Cancelled(_)) => (Class::Cancelled, None),
        // A payload that is no tool output still says which way it went.
        None if matches!(outcome, SettledOutput::Completed(_)) => (Class::Success, None),
        None => (Class::Failure, None),
    })
}

/// Record the statement starts the engine step `step` counted on `tx`, the
/// transaction that hands its settled output to the engine: once, under the
/// step, so a recomputed pass appends nothing new (FIG-6370).
pub(super) fn record_ran(
    tx: &mut ActorTx,
    process: &ProcessId,
    step: &crate::StepName,
    ran: Vec<(u32, u64)>,
    fleet: crate::FleetFormat,
) {
    if ran.is_empty() {
        return;
    }
    let ran = crate::runtime::process::ProcessRan::new(ran, fleet);
    append_event(
        tx,
        process,
        ran.append_request(format!("process:{process}:ran:{}", step.0)),
    );
}

/// Append `request` to `process`'s event log on `tx`, exactly once under
/// its replay key: it commits with the transaction, or not at all.
pub(super) fn append_event(
    tx: &mut ActorTx,
    process: &ProcessId,
    request: crate::ProcessEventAppendRequest,
) {
    tx.write(DomainWrite::Process(ProcessWrite::AppendEvent {
        process: process.clone(),
        event_type: request.fact.event_type().to_owned(),
        payload_json: request.fact.payload().to_string(),
        replay_key: request.replay.map(|replay| replay.key).unwrap_or_default(),
    }));
}

/// Record the effect occurrences `driver` counted past the per-node cap on
/// `tx`, the process's terminal transaction, before its terminal.
pub(super) fn record_omitted_effects(
    tx: &mut ActorTx,
    process: &ProcessId,
    driver: &Driver,
    fleet: crate::FleetFormat,
) {
    if driver.omitted_effects.is_empty() {
        return;
    }
    let omissions = crate::runtime::process::ProcessEffectOmissions::new(
        driver.omitted_effects.clone(),
        driver.effect_occurrence_cap,
        fleet,
    );
    append_event(
        tx,
        process,
        omissions.append_request(format!("process:{process}:effect-omissions")),
    );
}
