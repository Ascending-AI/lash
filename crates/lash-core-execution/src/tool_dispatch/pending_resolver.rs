//! Arming the runtime-owned resolver a parked call named.
//!
//! A [`ToolOutcome::Pending`](crate::ToolOutcome::Pending) that carries a
//! [`PendingResolver`](crate::PendingResolver) makes the *runtime* responsible
//! for delivering the outcome, not an out-of-band actor. This module is the one
//! place that discharges that responsibility, and every park site calls it
//! immediately before parking so the arming and the park are adjacent in the
//! journal.
//!
//! Arming runs on the first park **and on every redrive of the parked turn**.
//! It has to: a recorded attempt body does not re-run when its turn is
//! re-driven, so the tool that named the resolver never gets a second chance to
//! arm it, and an in-process watcher does not survive a crash. Because the
//! declaration is journaled with the pending launch, the redrive re-derives the
//! identical arming from the identical bytes, and the boundary's own idempotence
//! — resolving a wait that is already resolved reports
//! [`ResolveOutcome::AlreadyResolved`](crate::ResolveOutcome::AlreadyResolved) —
//! makes the repetition harmless.

/// A call with no resolver is left exactly as it was: nothing is armed and the
/// caller parks on a key only an external actor can resolve. A failure to arm
/// is returned rather than swallowed — parking on a wait whose resolver was
/// never armed hangs the call for the lifetime of the turn.
pub async fn arm_pending_resolver(
    processes: &dyn crate::ProcessService,
    pending: &crate::PendingCompletion,
    key: &crate::AwaitEventKey,
    scope: crate::ProcessOpScope<'_>,
) -> Result<(), crate::PluginError> {
    match pending.resolved_by.as_ref() {
        None => Ok(()),
        Some(crate::PendingResolver::ProcessTerminal { process_ref }) => {
            processes
                .attach_process_terminal(process_ref, key, scope)
                .await
        }
    }
}
