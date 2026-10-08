//! Project a process's committed deferred call when its actor releases.
use super::activation::append_event as append;
use super::driver::Driver;
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
    let Some(member) = member else {
        return;
    };
    let kind = WaitKind::Call {
        call_id: member.call().clone(),
        tool_id: member.draft().tool().clone(),
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
