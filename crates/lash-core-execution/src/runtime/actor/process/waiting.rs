//! Project everything a process is blocked on into its record: its parked
//! calls, and the key, process or instant its engine waits for, each with
//! the node that waits when the engine named one. A wait is entered when the
//! actor releases to wait on rows, and ended by the transition after which
//! it no longer holds. A wait the transition that delivered the process's
//! cancel ends was cut short, never resumed (FIG-6821).
use super::driver::{Blocked, Driver};
use super::effects::append_event as append;
use crate::runtime::actor::round::{MemberState, RunFold};
use lash_durable::ActorTx;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::StepRequest;
use lash_execution_contracts::process::WaitKind;
use lash_execution_contracts::process::WaitState;
use lash_vocabulary::identity::ProcessId;

/// Why a transition ends the waits it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ending {
    /// What the process waited for arrived, or its engine moved on.
    Resumed,
    /// The transition delivered the process's cancel, or forced its
    /// cancelled terminal: the waits it ends never resumed.
    Cancelled,
}

impl Ending {
    /// The ending of the waits a transition for `event` ends.
    pub(super) fn of(event: &lash_execution_contracts::process::EngineEvent) -> Self {
        match event {
            lash_execution_contracts::process::EngineEvent::Cancelled { .. } => Self::Cancelled,
            _ => Self::Resumed,
        }
    }

    fn event(
        self,
        process: &ProcessId,
        wait: &WaitState,
    ) -> lash_execution_contracts::process::ProcessEventAppendRequest {
        use lash_execution_contracts::process::ProcessEventAppendRequest;
        match self {
            Self::Resumed => ProcessEventAppendRequest::wait_cleared(process, wait),
            Self::Cancelled => ProcessEventAppendRequest::wait_cancelled(process, wait),
        }
    }
}

/// What the engine itself waits for, beside its steps.
fn engine_blocker(
    driver: &Driver,
) -> Option<(WaitKind, Option<lash_kernel_value::sites::EffectLocation>)> {
    match driver.blocked.as_ref()? {
        Blocked::External { name, site, .. } => Some((
            WaitKind::Key { name: name.clone() },
            site.clone()
                .map(lash_kernel_value::sites::EffectLocation::authored),
        )),
        Blocked::Process { process, site, .. } => Some((
            WaitKind::Process {
                process_id: process.clone(),
            },
            site.clone()
                .map(lash_kernel_value::sites::EffectLocation::authored),
        )),
        Blocked::Sleep { until, site } => Some((
            WaitKind::Sleep { until_ms: *until },
            site.clone()
                .map(lash_kernel_value::sites::EffectLocation::authored),
        )),
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
    let mut blockers: Vec<(WaitKind, Option<lash_kernel_value::sites::EffectLocation>)> = fold
        .rounds()
        .flat_map(|round| round.members())
        .filter(|member| matches!(member.state(), MemberState::Waiting { .. }))
        .filter_map(|member| {
            let MemberState::Waiting { source, .. } = member.state() else {
                return None;
            };
            let step = driver.steps.values().find(|step| {
                step.call == *member.call() && matches!(step.request, StepRequest::Tool { .. })
            })?;
            Some((
                WaitKind::Call {
                    call_id: member.call().clone(),
                    tool_id: member.draft().tool().clone(),
                    resume_at_ms: source.named().timer.as_ref().map(|timer| timer.deadline_ms),
                },
                step.request.location().cloned(),
            ))
        })
        .collect();
    blockers.extend(engine_blocker(driver));
    let listed = |wait: &WaitState| blockers.contains(&(wait.kind.clone(), wait.site.clone()));
    for wait in record.waits().iter().filter(|wait| !listed(wait)) {
        append(
            tx,
            process,
            lash_execution_contracts::process::ProcessEventAppendRequest::wait_cleared(
                process, wait,
            ),
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
            lash_execution_contracts::process::ProcessEventAppendRequest::wait_entered(
                process, &wait,
            ),
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
    ending: Ending,
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
            append(tx, process, ending.event(process, wait));
        }
    }
}

/// End every wait `record` lists, ahead of its terminal.
pub(super) fn end(tx: &mut ActorTx, process: &ProcessId, record: &ProcessRecord, ending: Ending) {
    for wait in record.waits() {
        append(tx, process, ending.event(process, wait));
    }
}
