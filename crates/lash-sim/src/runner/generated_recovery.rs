use lash_core::engine::ReconcileCursor;
use lash_core::shift::relay::RelayPolicy;
use std::num::NonZeroUsize;

/// Generated worlds schedule recovery explicitly. A timed-out immediate scope
/// close is durable retry work, so the final history must include its recovery.
pub(super) async fn recover_scope_closes(
    engine: &lash_restate_test::RestateTestBackend,
) -> Result<(), String> {
    crate::invariants::settle_shifts(engine).await?;
    let policy = RelayPolicy::default();
    let recovery_budget = std::time::Duration::from_millis(
        policy.claim_ttl_ms + policy.attempt_budget_ms * u64::from(policy.attempt_ceiling.get()),
    );
    tokio::time::timeout(recovery_budget, async {
        let mut cursor = ReconcileCursor::default();
        loop {
            let snapshot = crate::invariants::StoreSnapshot::read("recovery", engine.stores())?;
            let pending = snapshot
                .obligations
                .iter()
                .filter(|row| {
                    row.table == "session_runs"
                        && matches!(row.state.as_deref(), Some("due" | "claimed"))
                })
                .collect::<Vec<_>>();
            if pending.is_empty() {
                return Ok(());
            }
            let shifts = engine
                .restate()
                .session_work_engine()
                .shifts_slot()
                .installed()
                .ok_or_else(|| {
                    "generated scope-close recovery has no installed `SessionShifts`".to_owned()
                })?;
            // A retry waits for its due time; a dead claimant waits for its lapse.
            // Advance the server clock rather than bypassing either ledger fence.
            if let Some(due) = pending.iter().filter_map(|row| row.due_at_ms).max() {
                engine.server().advance_to(due);
            }
            cursor = shifts
                .reconcile(&cursor, NonZeroUsize::MIN.saturating_add(63))
                .await
                .map_err(|error| format!("generated scope-close recovery: {error}"))?;
            crate::invariants::settle_shifts(engine).await?;
        }
    })
    .await
    .map_err(|_| format!("generated scope-close recovery exceeded {recovery_budget:?}"))?
}
