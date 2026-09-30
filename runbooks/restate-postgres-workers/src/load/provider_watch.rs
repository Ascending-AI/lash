//! The provider watchdog starts at the bound root's first journaled admission.
//! Reading that timestamp also preserves its 120-second budget after replay.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::{DeploymentStore, TurnId};
use lash_restate::{RestateAdminClient, RestateHttpErrorClass};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

const PROVIDER_WATCHDOG_MS: i64 = 120_000;

struct Sample {
    now_ms: i64,
    root_started_at_ms: Option<i64>,
    asked_at_ms: Option<i64>,
}

#[async_trait::async_trait]
trait ProviderProbe: Send {
    async fn sample(&mut self) -> Result<Sample>;
}

struct LoadProviderProbe<'a> {
    store: Arc<dyn DeploymentStore>,
    witness: &'a PgPool,
    receipt: &'a TurnInputAcceptanceReceipt,
    operation: &'a str,
    admin: RestateAdminClient,
    root: Option<TurnId>,
    started_at_ms: Option<i64>,
    enqueued_at_ms: Option<u64>,
}

impl LoadProviderProbe<'_> {
    async fn root_start(&mut self) -> Result<Option<i64>> {
        if self.started_at_ms.is_some() {
            return Ok(self.started_at_ms);
        }
        if self.root.is_none() {
            self.root = self
                .store
                .root_of_input(&self.receipt.session_id, &self.receipt.input_id)
                .await?;
        }
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let key = lash_restate::turn_workflow_key(&self.receipt.session_id, root);
        let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
        #[derive(serde::Deserialize)]
        struct Invocation {
            id: String,
        }
        let invocations = self
            .admin
            .query_json::<Invocation>(&format!(
                "SELECT id FROM sys_invocation WHERE \
                 (target_service_name = 'LashTurn' OR target_service_name LIKE 'LashTurn_g%') \
                 AND target_service_key = {} AND target_handler_name = 'run'",
                literal(&key),
            ))
            .await;
        let invocations = match invocations {
            Ok(rows) => rows,
            Err(error) if error.classification() == RestateHttpErrorClass::Transient => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        // The admission command was emitted while this root executed. Its
        // first append survives retries, unlike an observer's current time.
        #[derive(serde::Deserialize)]
        struct RootStart {
            appended_at: DateTime<Utc>,
        }
        for invocation in invocations {
            let starts = self
                .admin
                .query_json::<RootStart>(&format!(
                    "SELECT appended_at FROM sys_journal WHERE id = {} \
                     AND name = {} ORDER BY index LIMIT 1",
                    literal(&invocation.id),
                    literal(&format!("lash:drive-admit:{root}")),
                ))
                .await;
            match starts {
                Ok(starts) => {
                    if let Some(start) = starts.first() {
                        let started_at_ms = start.appended_at.timestamp_millis();
                        self.started_at_ms = Some(started_at_ms);
                        tracing::info!(
                            operation = self.operation,
                            root = root.as_str(),
                            started_at_ms,
                            queue_latency_ms = u64::try_from(started_at_ms)
                                .ok()
                                .zip(self.enqueued_at_ms)
                                .map(|(started, enqueued)| started.saturating_sub(enqueued)),
                            "load provider watchdog observes the root's admission"
                        );
                        return Ok(self.started_at_ms);
                    }
                }
                Err(error) if error.classification() == RestateHttpErrorClass::Transient => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }
}

#[async_trait::async_trait]
impl ProviderProbe for LoadProviderProbe<'_> {
    async fn sample(&mut self) -> Result<Sample> {
        let asked_at_us: Option<i64> = sqlx::query_scalar(
            "SELECT min(recorded_at_us) FROM witness_provider_receipts WHERE workflow_id = $1",
        )
        .bind(self.operation)
        .fetch_one(self.witness)
        .await
        .with_context(|| format!("poll the provider receipt of `{}`", self.operation))?;
        Ok(Sample {
            root_started_at_ms: self.root_start().await?,
            now_ms: Utc::now().timestamp_millis(),
            asked_at_ms: asked_at_us.map(|at| at / 1_000),
        })
    }
}

pub(super) async fn wait_for_provider_receipt(
    core: &lash::LashCore,
    witness: &PgPool,
    receipt: &TurnInputAcceptanceReceipt,
    operation: &str,
) -> Result<()> {
    let store = core.backend().session_store_factory();
    let enqueued_at_ms = store
        .pending_turn_input(&receipt.session_id, &receipt.input_id)
        .await?
        .map(|input| input.input.enqueued_at_ms);
    let mut probe = LoadProviderProbe {
        store,
        witness,
        receipt,
        operation,
        admin: RestateAdminClient::new(crate::restate_admin_url()),
        root: None,
        started_at_ms: None,
        enqueued_at_ms,
    };
    wait(&mut probe, operation).await
}

async fn wait(probe: &mut impl ProviderProbe, operation: &str) -> Result<()> {
    loop {
        let sample = probe.sample().await?;
        let deadline = sample
            .root_started_at_ms
            .map(|started| started.saturating_add(PROVIDER_WATCHDOG_MS));
        if let Some(asked) = sample.asked_at_ms
            && deadline.is_none_or(|deadline| asked <= deadline)
        {
            return Ok(());
        }
        if deadline.is_some_and(|deadline| sample.now_ms >= deadline) {
            anyhow::bail!("the provider was never asked for `{operation}`");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct ScriptedProbe(VecDeque<Sample>);

    #[async_trait::async_trait]
    impl ProviderProbe for ScriptedProbe {
        async fn sample(&mut self) -> Result<Sample> {
            self.0.pop_front().context("watchdog exhausted its script")
        }
    }

    fn sample(now_ms: i64, root_started_at_ms: Option<i64>, asked_at_ms: Option<i64>) -> Sample {
        Sample {
            now_ms,
            root_started_at_ms,
            asked_at_ms,
        }
    }

    #[tokio::test]
    async fn queued_112_seconds_does_not_spend_the_provider_watchdog() {
        let mut probe = ScriptedProbe(VecDeque::from([
            sample(0, None, None),
            sample(112_000, Some(112_000), None),
            sample(121_000, Some(112_000), None),
            sample(132_000, Some(112_000), Some(131_999)),
        ]));
        wait(&mut probe, "queued-input")
            .await
            .expect("running root has 120 seconds");
        assert!(
            probe.0.is_empty(),
            "the waiter must observe the actual provider receipt"
        );
    }

    #[tokio::test]
    async fn a_started_root_that_never_asks_expires_at_120_seconds() {
        let mut probe = ScriptedProbe(VecDeque::from([
            sample(0, None, None),
            sample(112_000, Some(112_000), None),
            sample(231_999, Some(112_000), None),
            sample(232_000, Some(112_000), None),
        ]));
        let error = wait(&mut probe, "unresponsive-root")
            .await
            .expect_err("root must time out");
        assert!(error.to_string().contains("provider was never asked"));
        assert!(
            probe.0.is_empty(),
            "queue time cannot cause an early timeout"
        );
    }
}
