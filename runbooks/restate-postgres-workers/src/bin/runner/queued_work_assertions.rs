use super::*;
use lash_core::runtime::{
    QueuedWorkEnqueueOutcome, QueuedWorkPayload, process_wake_batch_draft_with_delivery_policy,
};
use lash_core::store::{IngressTerminal, IngressTerminalCause, QueuedWorkStore as _};

pub(super) async fn assert_no_live_queued_work(storage: &PostgresStorage) -> Result<()> {
    let live = storage
        .session_store_factory()
        .list_queued_work(&SessionId::from(DEFAULT_SESSION_ID))
        .await
        .context("list live open and admitted queued work using the store's terminal predicate")?;
    anyhow::ensure!(
        live.is_empty(),
        "runtime defect: leftover live open/admitted queued work after wake consumption: {live:?}"
    );
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct RetainedWake {
    batch_id: String,
    source_key: String,
    delivery_policy: String,
    submission_digest: String,
    terminal_cause: Option<String>,
    terminal_at_ms: Option<i64>,
    admitted_root: Option<String>,
    admitted_by: Option<String>,
    payload_json: String,
}

impl RetainedWake {
    fn terminal(&self) -> Result<IngressTerminal> {
        let terminal = lash_core::store_backend_support::decode_ingress_terminal(
            "QueuedWorkBatch",
            self.terminal_cause.as_deref(),
            self.terminal_at_ms.map(u64::try_from).transpose()?,
        )?
        .with_context(|| format!("runtime defect: retained batch is live work: {self:?}"))?;
        anyhow::ensure!(
            terminal.cause == IngressTerminalCause::Delivered
                && self.admitted_root.is_none()
                && self.admitted_by.is_none(),
            "expected delivered wake tombstone with released admission: {self:?}"
        );
        Ok(terminal)
    }
}

async fn retained_queued_work_snapshot(pool: &sqlx::PgPool) -> Result<Vec<Value>> {
    sqlx::query_scalar(
        "SELECT to_jsonb(batch)
         FROM lash_queued_work_batches batch
         WHERE batch.session_id = $1 ORDER BY batch.enqueue_seq",
    )
    .bind(DEFAULT_SESSION_ID)
    .fetch_all(pool)
    .await
    .context("snapshot retained queued batches, including terminal causes")
}

pub(super) async fn assert_retained_wake_tombstones(storage: &PostgresStorage) -> Result<()> {
    let before = retained_queued_work_snapshot(storage.pool()).await?;
    let roots = driven_queued_roots(storage.pool(), DEFAULT_SESSION_ID).await?;
    anyhow::ensure!(
        before.len() == 2 && roots.len() == 2,
        "expected the two consumed kitchen-sink wakes and their driven roots, got rows={before:?}, roots={roots:?}"
    );
    let store = storage.session_store_factory();
    for snapshot in &before {
        let row: RetainedWake = serde_json::from_value(snapshot.clone())?;
        let terminal = row.terminal()?;
        let QueuedWorkPayload::ProcessWake { wake } = serde_json::from_str(&row.payload_json)?
        else {
            anyhow::bail!("expected retained process-wake payload: {row:?}");
        };
        let policy = lash_core::DeliveryPolicy::from_wire_str(&row.delivery_policy)
            .context("invalid retained wake delivery policy")?;
        let draft = process_wake_batch_draft_with_delivery_policy(*wake, policy);
        anyhow::ensure!(
            draft.session_id.as_str() == DEFAULT_SESSION_ID
                && draft.source_key.as_deref() == Some(row.source_key.as_str())
                && draft.submission_digest()? == row.submission_digest,
            "retained wake identity or submission digest changed: {row:?}"
        );
        let outcome = store
            .enqueue_queued_work_with_outcome(draft)
            .await
            .with_context(|| format!("redeliver retained wake `{}`", row.batch_id))?;
        let QueuedWorkEnqueueOutcome::Existing(batch) = outcome else {
            anyhow::bail!(
                "redelivery reopened terminal wake `{}`: {outcome:?}",
                row.batch_id
            );
        };
        anyhow::ensure!(
            batch.batch_id.as_str() == row.batch_id
                && batch.terminal == Some(terminal)
                && batch.submission_digest == row.submission_digest,
            "redelivery lost the original terminal wake evidence: {batch:?}"
        );
        println!(
            "retained wake tombstone: batch={} source={} cause={} terminal_at_ms={} redelivery=existing",
            row.batch_id,
            row.source_key,
            terminal.cause.as_str(),
            terminal.at_ms
        );
    }
    anyhow::ensure!(
        retained_queued_work_snapshot(storage.pool()).await? == before,
        "redelivery changed retained queued batches or items"
    );
    assert_no_live_queued_work(storage).await?;
    anyhow::ensure!(
        driven_queued_roots(storage.pool(), DEFAULT_SESSION_ID).await? == roots,
        "redelivery created another driven root"
    );
    println!("queued-work cleanup passed: live=0 retained=2 idempotent-redeliveries=2");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retained_wake(cause: Option<IngressTerminalCause>) -> RetainedWake {
        RetainedWake {
            batch_id: "batch".into(),
            source_key: "wake".into(),
            delivery_policy: "unused".into(),
            submission_digest: "digest".into(),
            terminal_cause: cause.map(|cause| cause.as_str().into()),
            terminal_at_ms: cause.map(|_| 123),
            admitted_root: None,
            admitted_by: None,
            payload_json: String::new(),
        }
    }

    #[test]
    fn retained_wake_accepts_delivered_tombstone_and_rejects_live_rows() {
        assert!(
            retained_wake(Some(IngressTerminalCause::Delivered))
                .terminal()
                .is_ok()
        );
        assert!(retained_wake(None).terminal().is_err());
        let mut admitted = retained_wake(None);
        admitted.admitted_root = Some("root".into());
        admitted.admitted_by = Some("step".into());
        assert!(admitted.terminal().is_err());
    }

    #[test]
    fn retained_wake_rejects_wrong_causes_and_incomplete_settlement() {
        for cause in IngressTerminalCause::ALL {
            if cause != IngressTerminalCause::Delivered {
                assert!(retained_wake(Some(cause)).terminal().is_err());
            }
        }
        let delivered = || retained_wake(Some(IngressTerminalCause::Delivered));
        let mut missing_time = delivered();
        missing_time.terminal_at_ms = None;
        assert!(missing_time.terminal().is_err());
        let mut missing_cause = delivered();
        missing_cause.terminal_cause = None;
        assert!(missing_cause.terminal().is_err());
        let mut unknown_cause = delivered();
        unknown_cause.terminal_cause = Some("unknown".into());
        assert!(unknown_cause.terminal().is_err());
        let mut negative_time = delivered();
        negative_time.terminal_at_ms = Some(-1);
        assert!(negative_time.terminal().is_err());
        let mut bound_root = delivered();
        bound_root.admitted_root = Some("root".into());
        assert!(bound_root.terminal().is_err());
        let mut bound_step = delivered();
        bound_step.admitted_by = Some("step".into());
        assert!(bound_step.terminal().is_err());
    }
}
