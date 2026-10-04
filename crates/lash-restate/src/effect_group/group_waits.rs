//! Releases cancel-decided event waits before the group interrupts their invocations.

use super::*;
use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitResolveRequest, durable_wait_index_object_key,
};
use lash_core::Resolution;

/// Release each decided wait child before its cancellation is recorded.
pub(super) async fn seal_cancel_decisions(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    group_key: &str,
    positions: &[usize],
) -> Result<(), TerminalError> {
    // The membership is read only when there is a decision to seal: a close
    // that decides nothing leaves the group's envelopes unread (FIG-4068).
    if positions.is_empty() {
        return Ok(());
    }
    let membership = load_membership(ctx).await?;
    release_cancel_decided_waits(ctx, namespace, group_key, &membership, positions).await
}

/// Releases the wait of every `AwaitEvent` child in `positions`, because the
/// calling index handler, a close or a retirement, is recording its cancel
/// decision (ADR 0099 §12, FIG-3630).
///
/// The deciding handler is the one owner of this release. The close runs it
/// before it interrupts the child's invocation, so it does not depend on how
/// far the child got: a child the Restate cancel stops while it awaits its admission,
/// or before it runs at all, never reaches a release arm of its own. The
/// release is first-writer-wins, so a wait that already holds a terminal keeps
/// it, and a redrive of the close answers the same way.
async fn release_cancel_decided_waits(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    group_key: &str,
    membership: &EffectGroupMembership,
    positions: &[usize],
) -> Result<(), TerminalError> {
    for &position in positions {
        let envelope = membership.envelope(group_key, position)?;
        let lash_core::RuntimeEffectCommand::AwaitEvent { key } = envelope.command else {
            continue;
        };
        let replay_key = key.key_id.clone();
        let address = RestateDurableWaitAddress::for_key(&key);
        namespace
            .durable_wait_registry(ctx, durable_wait_index_object_key(&address))
            .resolve(RestateDurableWaitResolveRequest {
                key,
                resolution: Resolution::Cancelled,
            })
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
            .call()
            .await?;
    }
    Ok(())
}
