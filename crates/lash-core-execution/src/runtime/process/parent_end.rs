//! Applying a parent-end plan (ADR 0094, FIG-3822): one engine-neutral body
//! that every engine runs, on the live path as a recorded step right after
//! the ending, and from the reconcile pass for a plan a dead execution left
//! unapplied.
//!
//! A plan is a ledger row and nothing else: its work is the query for the
//! parent's live `Cancel` children that carry no cancel request yet. Applying
//! it delivers `ParentEnded` to each of them, then marks the row settled.
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
    CancelOrigin, CancelRequest, ParentScope, PluginError, ProcessId, ProcessRef, ProcessRegistry,
    ProcessWorkSubstrate,
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
/// storage identity. `Host` never ends, so it has no requester.
pub fn parent_end_requester(parent: &ParentScope) -> Option<String> {
    parent.storage_id()
}

/// The engine delivery key of one child's parent-end cancel: unique per
/// ended scope and child, so a re-run of the application names the delivery
/// the first run made.
pub fn parent_end_delivery_key(parent: &ParentScope, child: &ProcessId) -> Option<String> {
    let id = parent.storage_id()?;
    Some(delivery_key(parent, &id, child))
}

/// The key of a scope whose storage id is `id`, length-framed so no two
/// (scope, child) pairs share one.
fn delivery_key(parent: &ParentScope, id: &str, child: &ProcessId) -> String {
    format!(
        "parent-end:{}:{}:{}:{}",
        parent.storage_kind(),
        id.len(),
        id,
        child
    )
}

/// Apply `parent`'s plan: deliver `ParentEnded` to each live `Cancel` child
/// with no cancel request yet, record the request, then settle the row.
///
/// `delivery` is the engine's process port, which makes each child's running
/// execution observe the cancel. A scope with no ledger row applies nothing
/// and reports `planned: false`.
pub async fn apply_parent_end_plan(
    registry: &dyn ProcessRegistry,
    delivery: &dyn ProcessWorkSubstrate,
    parent: &ParentScope,
    now_ms: u64,
) -> Result<ParentEndApplication, PluginError> {
    let Some(requester) = parent_end_requester(parent) else {
        return Err(PluginError::Session(
            "the host parent scope never ends and has no parent-end plan".to_string(),
        ));
    };
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
            let child_ref = ProcessRef::from_record(child);
            let key = delivery_key(parent, &requester, &child.id);
            delivery.deliver_cancel(&child_ref, &request, &key).await?;
            match registry
                .request_process_cancel(
                    &child_ref,
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
/// sink's body). Recording is idempotent and keeps the first `ended_at_ms`.
pub async fn end_parent_scope(
    registry: &dyn ProcessRegistry,
    delivery: &dyn ProcessWorkSubstrate,
    parent: &ParentScope,
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
        let parent = ParentScope::turn(session.clone(), root.clone());
        let application = end_parent_scope(registry, delivery, &parent, now_ms).await?;
        total.delivered = total.delivered.saturating_add(application.delivered);
        total.planned |= application.planned;
    }
    Ok(total)
}

/// What one reconcile pass over unapplied plans did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParentEndReconcileReport {
    /// Plans this pass applied and settled.
    pub applied: Vec<ParentScope>,
    /// Children those applications delivered to.
    pub delivered: u32,
    /// Plans left pending, with why; the next pass retries them.
    pub deferred: Vec<(ParentScope, String)>,
}

/// One bounded, idempotent pass over plans that are recorded but not
/// applied: the ending's execution died between the record and the apply,
/// or the ending was written off any execution (a never-started child
/// folded Cancelled, an externally owned abandon). The engine-neutral
/// reconcile pass (ADR 0104 O2, decision 70) calls it on every tick; it is
/// not a background actor of its own.
///
/// One failing plan never aborts the page: it stays pending for the next
/// pass, so one unreachable child cannot starve every other parent.
pub async fn reconcile_parent_end_plans(
    registry: &dyn ProcessRegistry,
    delivery: &dyn ProcessWorkSubstrate,
    page: NonZeroUsize,
    now_ms: u64,
) -> Result<ParentEndReconcileReport, PluginError> {
    let mut report = ParentEndReconcileReport::default();
    for plan in registry.list_pending_parent_end_plans(page).await? {
        match apply_parent_end_plan(registry, delivery, &plan.parent, now_ms).await {
            Ok(application) => {
                report.delivered = report.delivered.saturating_add(application.delivered);
                report.applied.push(plan.parent);
            }
            Err(error) => {
                tracing::warn!(
                    parent_kind = plan.parent.storage_kind(),
                    parent_id = plan.parent.storage_id().unwrap_or_default(),
                    error = %error,
                    "parent-end plan stays pending for the next reconcile pass",
                );
                report.deferred.push((plan.parent, error.to_string()));
            }
        }
    }
    Ok(report)
}
