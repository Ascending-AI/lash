use std::num::NonZeroUsize;

use crate::{ParentEndPlan, ParentScope, PluginError, ProcessId, ProcessRecord};

use super::TestLocalProcessRegistry;

/// The ledger key: the scope's storage kind and id.
///
/// `Host` has no id and never ends, so it is refused rather than keyed.
fn ledger_key(parent: &ParentScope) -> Result<(String, String), PluginError> {
    parent
        .storage_id()
        .map(|id| (parent.storage_kind().to_string(), id))
        .ok_or_else(|| {
            PluginError::Session(
                "a host parent scope never ends and cannot have a parent-end ledger row"
                    .to_string(),
            )
        })
}

/// Drop settled ledger rows the retention horizon has passed and no live child
/// still names.
///
/// The SQL tiers run the same anti-join inside their prune transaction. The
/// row has to outlive its scope — it is what refuses a late `Cancel` child —
/// so retention, not the sweep, is what reclaims it; without this the ledger
/// grows by one row per committed turn forever. Takes only the ledger and
/// `managed` locks: the caller already holds the transaction lock.
pub(super) async fn reclaim_settled_plans_locked(
    registry: &TestLocalProcessRegistry,
    cutoff_epoch_ms: u64,
) {
    let managed = registry.managed.lock().await;
    let mut plans = registry.parent_end_plans.lock().await;
    plans.retain(|key, plan| {
        let reclaimable = plan
            .settled_at_ms
            .is_some_and(|settled_at_ms| settled_at_ms < cutoff_epoch_ms)
            && !managed.values().any(|managed| {
                managed.record.status.is_live()
                    && ledger_key(&managed.record.lifecycle.parent)
                        .is_ok_and(|child_key| child_key == *key)
            });
        !reclaimable
    });
}

/// The scope a terminal process row ends, written in the same critical section
/// as the terminal append so the sweep can never see the terminal fact without
/// the ledger row. Takes only the ledger lock: the caller already holds the
/// transaction and `managed` locks.
pub(super) async fn record_terminal_locked(
    registry: &TestLocalProcessRegistry,
    record: &ProcessRecord,
) {
    let parent = ParentScope::Process {
        process_id: record.id.clone(),
        incarnation: record.incarnation,
    };
    let Ok(key) = ledger_key(&parent) else {
        return;
    };
    let ended_at_ms = registry.clock.timestamp_ms();
    registry
        .parent_end_plans
        .lock()
        .await
        .entry(key)
        .or_insert(ParentEndPlan {
            parent,
            ended_at_ms,
            settled_at_ms: None,
        });
}

pub(super) async fn record(
    registry: &TestLocalProcessRegistry,
    parent: &ParentScope,
) -> Result<(), PluginError> {
    let key = ledger_key(parent)?;
    let _transaction = registry.transaction.lock().await;
    let ended_at_ms = registry.clock.timestamp_ms();
    registry
        .parent_end_plans
        .lock()
        .await
        .entry(key)
        .or_insert_with(|| ParentEndPlan {
            parent: parent.clone(),
            ended_at_ms,
            settled_at_ms: None,
        });
    Ok(())
}

pub(super) async fn list_pending(
    registry: &TestLocalProcessRegistry,
    limit: NonZeroUsize,
) -> Result<Vec<ParentEndPlan>, PluginError> {
    let _transaction = registry.transaction.lock().await;
    let plans = registry.parent_end_plans.lock().await;
    let mut pending = plans
        .iter()
        .filter(|(_, plan)| plan.settled_at_ms.is_none())
        .collect::<Vec<_>>();
    // Both SQL tiers order this page `ended_at_ms, parent_kind, parent_id`.
    // Ordering by the ledger key instead would hand a limited page a different
    // subset on this tier than on those, which is exactly the prefix-vs-SQL
    // `ORDER BY` divergence the cross-backend laws exist to catch.
    pending.sort_by(|(left_key, left), (right_key, right)| {
        (left.ended_at_ms, *left_key).cmp(&(right.ended_at_ms, *right_key))
    });
    Ok(pending
        .into_iter()
        .take(limit.get())
        .map(|(_, plan)| plan.clone())
        .collect())
}

pub(super) async fn get(
    registry: &TestLocalProcessRegistry,
    parent: &ParentScope,
) -> Result<Option<ParentEndPlan>, PluginError> {
    let Ok(key) = ledger_key(parent) else {
        return Ok(None);
    };
    let _transaction = registry.transaction.lock().await;
    Ok(registry.parent_end_plans.lock().await.get(&key).cloned())
}

/// Turn scopes with live `Cancel` children and no ledger row yet.
///
/// The in-memory mirror of the SQL candidate query the parent-end recovery
/// sweep pages: a crash between a turn's commit and its ledger row leaves
/// exactly this shape.
pub(super) async fn list_unrecorded_turn_parents(
    registry: &TestLocalProcessRegistry,
    after: Option<&str>,
    limit: NonZeroUsize,
) -> Result<Vec<ParentScope>, PluginError> {
    let _transaction = registry.transaction.lock().await;
    let plans = registry.parent_end_plans.lock().await;
    let managed = registry.managed.lock().await;
    let mut candidates = managed
        .values()
        .filter(|managed| {
            managed.record.lifecycle.on_parent_end == crate::OnParentEnd::Cancel
                && managed.record.status.is_live()
                && managed.record.cancel_request.is_none()
                && matches!(managed.record.lifecycle.parent, ParentScope::Turn { .. })
        })
        .filter_map(|managed| {
            let parent = &managed.record.lifecycle.parent;
            let key = ledger_key(parent).ok()?;
            if after.is_some_and(|after| key.1.as_str() <= after) {
                return None;
            }
            (!plans.contains_key(&key)).then(|| (key.1, parent.clone()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(left, _), (right, _)| left.cmp(right));
    candidates.dedup_by(|(left, _), (right, _)| left == right);
    Ok(candidates
        .into_iter()
        .take(limit.get())
        .map(|(_, parent)| parent)
        .collect())
}

pub(super) async fn children(
    registry: &TestLocalProcessRegistry,
    parent: &ParentScope,
    after: Option<&ProcessId>,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let _transaction = registry.transaction.lock().await;
    let managed = registry.managed.lock().await;
    let mut matched = managed
        .iter()
        .filter(|(process_id, managed)| {
            after.is_none_or(|after| *process_id > after)
                && managed.record.lifecycle.parent == *parent
                && managed.record.lifecycle.on_parent_end == crate::OnParentEnd::Cancel
                && managed.record.status.is_live()
                && managed.record.cancel_request.is_none()
        })
        .map(|(process_id, managed)| (process_id.clone(), managed.record.clone()))
        .collect::<Vec<_>>();
    matched.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(matched
        .into_iter()
        .take(limit.get())
        .map(|(_, record)| record)
        .collect())
}

pub(super) async fn settle(
    registry: &TestLocalProcessRegistry,
    parent: &ParentScope,
) -> Result<(), PluginError> {
    let key = ledger_key(parent)?;
    let _transaction = registry.transaction.lock().await;
    let settled_at_ms = registry.clock.timestamp_ms();
    if let Some(plan) = registry.parent_end_plans.lock().await.get_mut(&key) {
        plan.settled_at_ms.get_or_insert(settled_at_ms);
    }
    Ok(())
}

impl TestLocalProcessRegistry {
    /// Ledger read that assumes the caller already holds the transaction and
    /// `managed` locks, so registration can fence a late child without
    /// releasing the critical section it registers in.
    pub(super) async fn parent_end_plan_for(&self, parent: &ParentScope) -> Option<ParentEndPlan> {
        let key = ledger_key(parent).ok()?;
        self.parent_end_plans.lock().await.get(&key).cloned()
    }
}
