//! Applying a parent-end plan (ADR 0094, FIG-3822): one engine-neutral body
//! that every engine runs — on the live path as a recorded step right after
//! the ending, and as the delivery its `ParentEnd` obligation carries when a
//! reconcile pass relays it (ADR 0109: the ledger row is the obligation, and
//! the relay claims, delivers and settles it through the generic machinery).
//!
//! A plan is a ledger row and nothing else: its work is the query for the
//! parent's live `Until` children that carry no cancel request yet. Applying
//! it delivers `ParentEnded` to each of them, then marks the row settled —
//! and settling also marks a still-`due` obligation `delivered`, since the
//! apply is the delivery that obligation owes.
//!
//! **Delivery precedes the registry write.** The children query stops
//! returning a child once its cancel request is recorded. If the registry
//! write came first, a crash between it and the engine delivery would drop
//! the child from every later pass, and its running execution would never
//! learn of the cancel. Delivered first, a re-run still finds the child and
//! delivers again under the same key, which the engine dedupes and the
//! child's cancel handler treats as a no-op.
//!
//! Every part is idempotent: the delivery by its key, the registry request
//! by origin and requester (ADR 0094: the first request wins, and a retry's
//! timestamp is never compared), and the settle marker. So a crash anywhere
//! in the body is healed by running it again, and each child is cancelled
//! exactly once.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::{
    CancelOrigin, CancelRequest, PluginError, ProcessId, ProcessRegistry, ProcessWorkSubstrate,
    ScopeId,
};

/// Page size of the children query.
const CHILD_PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(255);

/// What one application of a plan did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentEndApplication {
    /// Children this application delivered `ParentEnded` to.
    pub delivered: u32,
    /// Whether the scope had a ledger row to settle. `false` for a scope
    /// that has not ended yet: a queue drain whose receipt is still owed.
    pub planned: bool,
}

/// The requester a parent-end cancel names: the ended scope's collision-free
/// storage identity.
#[must_use]
pub fn parent_end_requester(parent: &ScopeId) -> String {
    parent.storage_id()
}

/// The engine delivery key of one child's parent-end cancel: unique per
/// ended scope and child, so a re-run of the application names the delivery
/// the first run made.
///
/// The key is `parent-end:{kind}:{len}:{scope}:{child}`: the scope's storage
/// discriminant and its length-framed storage identity, then the child, so
/// no two (scope, child) pairs share one.
#[must_use]
pub fn parent_end_delivery_key(parent: &ScopeId, child: &ProcessId) -> String {
    let id = parent.storage_id();
    format!(
        "parent-end:{}:{}:{}:{}",
        parent.storage_kind(),
        id.len(),
        id,
        child
    )
}

/// Apply `parent`'s plan: deliver `ParentEnded` to each live `Until` child
/// with no cancel request yet, record the request, then settle the row.
///
/// `delivery` is the engine's process port, which makes each child's running
/// execution observe the cancel. A scope with no ledger row applies nothing
/// and reports `planned: false`.
pub async fn apply_parent_end_plan(
    registry: &dyn ProcessRegistry,
    delivery: &dyn ProcessWorkSubstrate,
    parent: &ScopeId,
    now_ms: u64,
) -> Result<ParentEndApplication, PluginError> {
    let requester = parent_end_requester(parent);
    if registry.get_parent_end_plan(parent).await?.is_none() {
        return Ok(ParentEndApplication::default());
    }
    let request = CancelRequest::new(CancelOrigin::ParentEnded, requester.clone(), now_ms);
    let mut delivered = 0_u32;
    let mut after: Option<ProcessId> = None;
    loop {
        let children = registry
            .list_parent_end_children(parent, after.as_ref(), CHILD_PAGE)
            .await?;
        let Some(last) = children.last() else { break };
        after = Some(last.id.clone());
        for child in &children {
            let key = parent_end_delivery_key(parent, &child.id);
            // A refused-for-good deliver (ADR 0109 §2: the record itself
            // refuses the obligation — an invalid record, a parked child, a
            // terminal row) is still applied state: the child will never
            // watch the registry for a request that was never written, so
            // the request is recorded to keep "a cancel is owed until it is
            // recorded" true. A retryable error ends the apply so the
            // obligation backs off and retries the page.
            match delivery.deliver_cancel(&child.id, &request, &key).await {
                Ok(()) => {}
                Err(error) if error.is_terminal() => {}
                Err(error) => return Err(error),
            }
            match registry
                .request_process_cancel(
                    &child.id,
                    CancelOrigin::ParentEnded,
                    requester.clone(),
                    None,
                )
                .await
            {
                Ok(_) => {}
                // A child that ended, or took another requester's cancel,
                // between the page read and this write needs nothing more:
                // the first request stands (ADR 0094).
                Err(
                    PluginError::ProcessAlreadyTerminal { .. }
                    | PluginError::ProcessCancelConflict { .. },
                ) => {}
                Err(error) => return Err(error),
            }
            delivered = delivered.saturating_add(1);
        }
    }
    registry.settle_parent_end_plan(parent).await?;
    Ok(ParentEndApplication {
        delivered,
        planned: true,
    })
}

/// Record `parent`'s end and apply its plan: what closing a scope the
/// registry does not end itself (a turn root) runs, inside the recorded
/// root-close step after the root's terminal evidence (the scope-close
/// sink's body when the host installs one that applies). Recording is
/// idempotent and keeps the first `ended_at_ms`.
pub async fn end_parent_scope(
    registry: &dyn ProcessRegistry,
    delivery: &dyn ProcessWorkSubstrate,
    parent: &ScopeId,
    now_ms: u64,
) -> Result<ParentEndApplication, PluginError> {
    registry.record_parent_end(parent).await?;
    apply_parent_end_plan(registry, delivery, parent, now_ms).await
}

/// End the scopes of `roots`, which a session's close ended together:
/// what closing a session runs for its roots, each exactly as its own root
/// close would (the scope-close sink's `close_session_scope` body). The
/// first failure stops the pass; every end already made is idempotent, so a
/// retry of the close resumes it.
pub async fn end_session_roots(
    registry: &dyn ProcessRegistry,
    delivery: &dyn ProcessWorkSubstrate,
    session: &crate::SessionId,
    roots: &[crate::TurnId],
    now_ms: u64,
) -> Result<ParentEndApplication, PluginError> {
    let mut total = ParentEndApplication::default();
    for root in roots {
        let parent = ScopeId::turn(session.clone(), root.clone());
        let application = end_parent_scope(registry, delivery, &parent, now_ms).await?;
        total.delivered = total.delivered.saturating_add(application.delivered);
        total.planned |= application.planned;
    }
    Ok(total)
}
