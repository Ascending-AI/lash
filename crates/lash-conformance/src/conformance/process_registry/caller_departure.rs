use super::*;

/// Retention reclaims caller-departed rows on every backend (FIG-1383).
///
/// Nothing may ever honestly terminalize such a row, so excluding it from
/// retention would leak rows without bound. Reclaiming is not an outcome
/// claim: the tombstone records the label the row actually carried.
pub(super) async fn caller_departed_rows_are_reclaimed_by_retention(
    registry: Arc<dyn ProcessRegistry>,
) {
    let reclaimed_id = "caller-departure-reclaimed";
    let retained_id = "caller-departure-retained";
    registry
        .register_process(registration(reclaimed_id))
        .await
        .expect("register reclaimable row");
    registry
        .register_process(registration(retained_id))
        .await
        .expect("register still-running row");
    let departed = registry
        .record_caller_departure(&ProcessId::from(reclaimed_id))
        .await
        .expect("record caller departure");
    let (_, projection_cursor) = changes_after_full_relist_if_required(&registry, 4096).await;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    registry
        .prune_terminal_processes(
            departed.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::UpTo(projection_cursor),
        )
        .await
        .expect("prune retired rows");
    assert!(
        matches!(
            registry.get_process(&ProcessId::from(reclaimed_id)).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "retention must reclaim a row nothing may ever terminalize"
    );
    assert!(
        registry
            .get_process(&ProcessId::from(retained_id))
            .await
            .expect("read still-running row")
            .is_some(),
        "an externally-owned row whose caller is still present must survive retention"
    );
}
