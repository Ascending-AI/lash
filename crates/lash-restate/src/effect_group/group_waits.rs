//! The durable-wait writes the effect-group index issues: resolving a group's
//! own wake waits, closing a cancel-decided child's completion key, and
//! releasing a cancel-decided wait child's wait.
//!
//! Split from the index handlers so the handler file keeps its line budget;
//! both are calls from an index handler into `LashDurableWaitRegistry`.

use super::*;
use crate::durable_wait::RestateDurableWaitCancelDecidedRequest;

pub(super) fn wait_resolution(
    value: EffectGroupWaitResolution,
) -> Result<Resolution, TerminalError> {
    serde_json::to_value(value)
        .map(Resolution::Ok)
        .map_err(|error| TerminalError::new(format!("serialize effect-group wake: {error}")))
}

pub(super) async fn resolve_group_wait(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    scope: &ExecutionScope,
    group_key: &str,
    kind: EffectGroupWaitKind<'_>,
    value: EffectGroupWaitResolution,
) -> Result<(), TerminalError> {
    resolve_group_waits(ctx, namespace, scope, group_key, [kind], value).await
}

/// Resolves each of `kinds` with `value`, every call issued before any is
/// awaited: one suspension covers the lot rather than one per wait, whatever
/// the group's width (FIG-4088). The calls are issued and awaited in `kinds`'
/// order, so the journal is the same on every replay.
pub(super) async fn resolve_group_waits<'k>(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    scope: &ExecutionScope,
    group_key: &str,
    kinds: impl IntoIterator<Item = EffectGroupWaitKind<'k>>,
    value: EffectGroupWaitResolution,
) -> Result<(), TerminalError> {
    let mut calls = Vec::new();
    for kind in kinds {
        let key = group_wait_key(scope, group_key, kind)?;
        let replay_key = key.key_id.clone();
        let address = RestateDurableWaitAddress::for_key(&key);
        calls.push(
            namespace
                .durable_wait_registry(ctx, durable_wait_index_object_key(&address))
                .resolve(RestateDurableWaitResolveRequest {
                    key,
                    resolution: wait_resolution(value.clone())?,
                })
                .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                .call(),
        );
    }
    for call in calls {
        call.await?;
    }
    Ok(())
}

/// Closes the completion key of every child in `positions` that is a
/// deferrable tool child, because its cancel decision is about to be recorded
/// (ADR 0099 §4, W17): completion delivery is one of the sinks the fence
/// covers, so a late resolve of the key is refused, typed.
///
/// Called by the index handler that decides the children, before it stores the
/// decision. The handler is exclusive on the group, so no child's final can
/// commit in between, and a resolve that lands before the fence is simply
/// earlier than the decision. The key is named by the child's retained request
/// — its admitted scope and call id — because the minting authority the key's
/// id also binds is not the index's to know.
/// Seals the cancel decisions an index handler, a close or a retirement, is
/// about to record: closes each decided tool child's completion key (§4, W17)
/// and releases each decided wait child's wait (§12, FIG-3630). Both happen
/// before the handler interrupts any child invocation, so neither depends on
/// how far a child got.
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
    fence_cancel_decided_completions(ctx, namespace, group_key, &membership, positions).await?;
    release_cancel_decided_waits(ctx, namespace, group_key, &membership, positions).await
}

async fn fence_cancel_decided_completions(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    group_key: &str,
    membership: &EffectGroupMembership,
    positions: &[usize],
) -> Result<(), TerminalError> {
    for &position in positions {
        let envelope = membership.envelope(group_key, position)?;
        let Some((scope, wait)) = envelope.command.group_child_completion_wait() else {
            continue;
        };
        namespace
            .durable_wait_registry(ctx, durable_wait_index_key_for_scope(&scope))
            .fence_cancel_decided(RestateDurableWaitCancelDecidedRequest { scope, wait })
            .call()
            .await?;
    }
    Ok(())
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
