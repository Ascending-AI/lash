//! What a live child turn that runs inside a process carries about it: the
//! attempt-bound invocation authority and the process's lineage.
//!
//! A [`crate::TurnContext`] holds one runtime correlation, so both facts ride
//! one private value: attaching either keeps the other.

use lash_sansio::ProcessId;

#[derive(Clone)]
pub(super) struct ProcessInvocationCorrelation {
    pub(super) process_id: ProcessId,
    pub(super) authority: crate::ProcessExecutionWriteAuthority,
}

#[derive(Clone, Default)]
struct ProcessTurnCorrelation {
    invocation: Option<ProcessInvocationCorrelation>,
    /// The lineage of the process the turn runs inside (FIG-3607 R1): what a
    /// start made in that turn records above its starter.
    lineage: Option<crate::ProcessLineage>,
}

fn update(turn_context: &mut crate::TurnContext, edit: impl FnOnce(&mut ProcessTurnCorrelation)) {
    let mut correlation = turn_context
        .runtime_correlation::<ProcessTurnCorrelation>()
        .cloned()
        .unwrap_or_default();
    edit(&mut correlation);
    if correlation.invocation.is_none() && correlation.lineage.is_none() {
        turn_context.clear_runtime_correlation::<ProcessTurnCorrelation>();
    } else {
        turn_context.set_runtime_correlation(correlation);
    }
}

/// Carries attempt-bound process invocation authority into one live child turn.
///
/// The private input type prevents an ordinary [`crate::TurnContext`] caller
/// from fabricating the correlation with a string. The runtime revalidates the
/// process and attempt when it reads the value.
pub(crate) fn attach_process_invocation_correlation(
    turn_context: &mut crate::TurnContext,
    process_id: &ProcessId,
    authority: &crate::ProcessExecutionWriteAuthority,
) {
    let invocation = authority
        .attempt_for(process_id)
        .map(|_| ProcessInvocationCorrelation {
            process_id: process_id.clone(),
            authority: authority.clone(),
        });
    update(turn_context, |correlation| {
        correlation.invocation = invocation
    });
}

/// Carries the lineage of the process a live child turn runs inside.
pub(crate) fn attach_process_lineage(
    turn_context: &mut crate::TurnContext,
    lineage: crate::ProcessLineage,
) {
    update(turn_context, |correlation| {
        correlation.lineage = Some(lineage);
    });
}

pub(crate) fn clear_process_invocation_correlation(turn_context: &mut crate::TurnContext) {
    turn_context.clear_runtime_correlation::<ProcessTurnCorrelation>();
}

pub(super) fn process_invocation_of(
    turn_context: &crate::TurnContext,
) -> Option<&ProcessInvocationCorrelation> {
    turn_context
        .runtime_correlation::<ProcessTurnCorrelation>()?
        .invocation
        .as_ref()
}

/// The lineage a turn context carries, when its turn runs inside a process.
pub(crate) fn process_lineage_of(
    turn_context: &crate::TurnContext,
) -> Option<crate::ProcessLineage> {
    turn_context
        .runtime_correlation::<ProcessTurnCorrelation>()?
        .lineage
        .clone()
}
