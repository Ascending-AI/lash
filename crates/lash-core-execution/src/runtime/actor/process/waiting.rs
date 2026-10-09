//! Project everything a process is blocked on into its record: its parked
//! calls, and the key, process or instant its engine waits for, each with
//! the node that waits when the engine named one. A wait is entered when the
//! actor releases to wait on rows, and ended by the transition after which
//! it no longer holds.
use super::activation::append_event as append;
use super::driver::{Blocked, Driver};
use crate::runtime::actor::round::{MemberState, RunFold};
use crate::{ProcessId, ProcessRecord, StepRequest, WaitKind, WaitState};
use lash_durable::ActorTx;

/// What the engine itself waits for, beside its steps.
fn engine_blocker(driver: &Driver) -> Option<(WaitKind, Option<lash_sansio::EffectIdentity>)> {
    match driver.blocked.as_ref()? {
        Blocked::External { name, site, .. } => {
            Some((WaitKind::Key { name: name.clone() }, site.clone()))
        }
        Blocked::Process { process, site, .. } => Some((
            WaitKind::Process {
                process_id: process.clone(),
            },
            site.clone(),
        )),
        Blocked::Sleep { until, site } => {
            Some((WaitKind::Sleep { until_ms: *until }, site.clone()))
        }
        Blocked::Idle => None,
    }
}

/// Record every blocker of a releasing process that its record does not
/// list yet, as of `since_ms`, and end the listed waits that no longer hold.
pub(super) fn project(
    tx: &mut ActorTx,
    process: &ProcessId,
    record: &ProcessRecord,
    driver: &Driver,
    fold: &RunFold,
    since_ms: i64,
) {
    let mut blockers: Vec<(WaitKind, Option<lash_sansio::EffectIdentity>)> = fold
        .rounds()
        .flat_map(|round| round.members())
        .filter(|member| matches!(member.state(), MemberState::Waiting { .. }))
        .filter_map(|member| {
            let step = driver.steps.values().find(|step| {
                step.call == *member.call() && matches!(step.request, StepRequest::Tool { .. })
            })?;
            Some((
                WaitKind::Call {
                    call_id: member.call().clone(),
                    tool_id: member.draft().tool().clone(),
                },
                step.request.site().cloned(),
            ))
        })
        .collect();
    blockers.extend(engine_blocker(driver));
    let listed = |wait: &WaitState| blockers.contains(&(wait.kind.clone(), wait.site.clone()));
    for wait in record.waits().iter().filter(|wait| !listed(wait)) {
        append(
            tx,
            process,
            crate::ProcessEventAppendRequest::wait_cleared(process, wait),
        );
    }
    for (kind, site) in blockers {
        if record
            .waits()
            .iter()
            .any(|wait| wait.kind == kind && wait.site == site)
        {
            continue;
        }
        let wait = WaitState {
            kind,
            since_ms,
            site,
        };
        append(
            tx,
            process,
            crate::ProcessEventAppendRequest::wait_entered(process, &wait),
        );
    }
}

/// End the waits `record` lists that `driver`, after a transition, no
/// longer holds: a call stays while its step is in flight, and a key,
/// process or sleep while the engine still waits for it.
pub(super) fn settle(
    tx: &mut ActorTx,
    process: &ProcessId,
    record: &ProcessRecord,
    driver: &Driver,
) {
    let engine = engine_blocker(driver).map(|(kind, _)| kind);
    for wait in record.waits() {
        let holds = match &wait.kind {
            WaitKind::Call { call_id, .. } => {
                driver.steps.values().any(|step| step.call == *call_id)
            }
            kind => engine.as_ref() == Some(kind),
        };
        if !holds {
            append(
                tx,
                process,
                crate::ProcessEventAppendRequest::wait_cleared(process, wait),
            );
        }
    }
}

/// End every wait `record` lists, ahead of its terminal.
pub(super) fn end(tx: &mut ActorTx, process: &ProcessId, record: &ProcessRecord) {
    for wait in record.waits() {
        append(
            tx,
            process,
            crate::ProcessEventAppendRequest::wait_cleared(process, wait),
        );
    }
}
