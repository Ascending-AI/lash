//! Waking and detaching subscriptions when a wait ends.
use super::*;

/// Fire a gate entry because the turn-control wait it guards has settled.
///
/// The wait's own `Resolution` is not forwarded whole: a gate entry only ever
/// guards a turn-control address, so the waiter needs to know that the turn
/// was asked to stop and in which mode, and nothing more.
pub(super) fn resolve_durable_wait_awakeable(
    ctx: &ObjectContext<'_>,
    request: &RestateDurableWaitAwakeableRequest,
    resolution: &Resolution,
) {
    ctx.resolve_awakeable(
        &request.awakeable_id,
        Json(RestateTurnCancelWake::for_gate_resolution(resolution)),
    );
}

/// Fire a gate entry because the whole session was revoked out from under it.
pub(super) fn revoke_durable_wait_awakeable(
    ctx: &ObjectContext<'_>,
    request: &RestateDurableWaitAwakeableRequest,
) {
    ctx.resolve_awakeable(
        &request.awakeable_id,
        Json(RestateTurnCancelWake::SessionRevoked),
    );
}

/// Wake and drop every awakeable entry watching a wait `ended` selects,
/// because that wait has ended with `resolution`. Answers whether an entry
/// was dropped, so the caller stores the metadata it changed.
///
/// An entry is owed its wake by every way its wait can end: a resolve, a
/// settle, its owning group child's cancel decision and a cancellation of
/// the scope's waits. A watcher left unwoken outlives the wait it watches;
/// a process attach would then hold its terminal read for a caller that is
/// gone.
pub(super) fn wake_ended_waits(
    namespace: &crate::RestateNamespace,
    ctx: &ObjectContext<'_>,
    metadata: &mut RestateDurableWaitIndexMetadata,
    resolution: &Resolution,
    mut ended: impl FnMut(&AwaitEventKey) -> bool,
) -> bool {
    let before = metadata.awakeables.len();
    metadata.awakeables.retain(|entry| {
        let ended = ended(&entry.key);
        if ended {
            resolve_durable_wait_awakeable(ctx, entry, resolution);
        }
        !ended
    });
    let detached = process_terminal::detach(namespace, ctx, metadata, ended);
    metadata.awakeables.len() != before || detached
}

pub(super) fn mirror_resolve_outcome(
    ctx: &ObjectContext<'_>,
    writer: StoredValueWriter,
    key: &AwaitEventKey,
    address: &RestateDurableWaitAddress,
    accepted_terminal: Resolution,
    outcome: &ResolveOutcome,
) {
    let terminal = match outcome {
        ResolveOutcome::AlreadyResolved { terminal } => terminal.clone(),
        ResolveOutcome::Accepted => accepted_terminal,
        ResolveOutcome::UnknownOrRevoked => return,
    };
    store_indexed_wait(ctx, writer, key, address, Some(terminal));
}
