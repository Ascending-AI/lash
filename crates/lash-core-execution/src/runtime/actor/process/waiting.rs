//! Project what a process waits on when its actor releases: its committed
//! deferred call, or the key its engine awaits.
use super::activation::append_event as append;
use super::driver::{Blocked, Driver};
use crate::runtime::actor::round::{MemberState, RunFold};
use crate::{ProcessId, ProcessRecord, StepRequest, WaitKind, WaitState};
use lash_durable::ActorTx;

pub(super) fn project(
    tx: &mut ActorTx,
    process: &ProcessId,
    record: &ProcessRecord,
    driver: &Driver,
    fold: &RunFold,
    since_ms: u64,
) {
    // Prefer the standing call while it remains pending. If several calls
    // park, the first admitted one is the process's current waiting descriptor.
    let parked: Vec<_> = fold
        .rounds()
        .flat_map(|round| round.members())
        .filter(|member| {
            matches!(member.state(), MemberState::Waiting { .. })
                && driver.steps.values().any(|step| {
                    step.call == *member.call() && matches!(step.request, StepRequest::Tool { .. })
                })
        })
        .collect();
    let member = parked
        .iter()
        .find(|member| {
            record
                .wait()
                .is_some_and(|wait| wait.key() == member.call().as_str())
        })
        .or_else(|| parked.first());
    let kind = match (member, &driver.blocked) {
        (Some(member), _) => WaitKind::Call {
            call_id: member.call().clone(),
            tool_id: member.draft().tool().clone(),
        },
        (None, Some(Blocked::External { name, .. })) => WaitKind::Key { name: name.clone() },
        (None, _) => return,
    };
    if record.wait().is_some_and(|wait| wait.kind == kind) {
        return;
    }
    if let Some(wait) = record.wait() {
        append(
            tx,
            process,
            crate::ProcessEventAppendRequest::wait_cleared(process, wait),
        );
    }
    let wait = WaitState { kind, since_ms };
    append(
        tx,
        process,
        crate::ProcessEventAppendRequest::wait_entered(process, &wait),
    );
}
