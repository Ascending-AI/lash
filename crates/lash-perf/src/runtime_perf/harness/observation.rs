use std::sync::Arc;

use super::BenchmarkRuntime;
use crate::runtime_perf::providers::BenchmarkToolCatalogObservation;

pub(crate) struct TriggerDeliveryTerminalObservation {
    pub(crate) process_count: u64,
    pub(crate) durable_claim_count: u64,
    pub(crate) terminal_count: u64,
}

impl BenchmarkRuntime {
    pub(crate) async fn refresh_tool_catalog(&self, idempotency_key: &str) -> anyhow::Result<()> {
        self.session
            .as_ref()
            .expect("benchmark session")
            .commands()
            .refresh_tool_catalog("runtime perf catalog attribution", idempotency_key)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    }

    pub(crate) fn suppress_tool_catalog_composition_counting(&self) {
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .suppress_composition_counting();
    }

    pub(crate) fn resume_tool_catalog_composition_counting(&self) {
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .resume_composition_counting();
    }

    pub(crate) fn arm_tool_catalog_observation(
        &self,
        variant: &'static str,
        phase_probe: Arc<dyn lash::runtime::RuntimeTurnPhaseProbe>,
        observation_stage: Arc<dyn Fn() -> u8 + Send + Sync>,
    ) {
        let session_id = self
            .session
            .as_ref()
            .expect("benchmark session")
            .session_id();
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .arm(variant, session_id, phase_probe, observation_stage);
    }

    pub(crate) fn finish_tool_catalog_observation(&self) -> BenchmarkToolCatalogObservation {
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .finish()
    }

    pub(crate) fn tool_catalog_metrics(&self) -> anyhow::Result<(usize, usize)> {
        let manifests = self.core().tool_catalog().manifests();
        let rendered_bytes = serde_json::to_vec(&manifests)?.len();
        Ok((manifests.len(), rendered_bytes))
    }

    pub(crate) async fn observe_trigger_delivery_terminals(
        &self,
    ) -> anyhow::Result<TriggerDeliveryTerminalObservation> {
        let session = self.session.as_ref().expect("benchmark session");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let delivery = loop {
            if let Some(delivery) = session
                .processes()
                .list_all()
                .await?
                .into_iter()
                .filter(|process| {
                    matches!(
                        process.caused_by.as_ref(),
                        Some(lash_core::CausalRef::TriggerOccurrence { .. })
                    )
                })
                .min_by_key(|process| {
                    (
                        process.created_at_ms,
                        !process.terminal,
                        process.process_id.clone(),
                    )
                })
            {
                break delivery;
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("trigger occurrence produced no observable delivery process");
            }
            tokio::task::yield_now().await;
        };
        if !delivery.terminal {
            session
                .processes()
                .await_output(delivery.process_id.as_str())
                .await?;
        }
        let terminal_processes = [session
            .processes()
            .get(delivery.process_id.as_str())
            .await?
            .ok_or_else(|| anyhow::anyhow!("trigger delivery process disappeared"))?];
        let observation = TriggerDeliveryTerminalObservation {
            process_count: terminal_processes.len() as u64,
            durable_claim_count: terminal_processes
                .iter()
                .filter(|process| process.first_started.is_some())
                .count() as u64,
            terminal_count: terminal_processes
                .iter()
                .filter(|process| process.terminal)
                .count() as u64,
        };
        if observation.durable_claim_count != observation.process_count
            || observation.terminal_count != observation.process_count
        {
            anyhow::bail!(
                "trigger delivery observation did not reach durable claim and terminal state: processes={}, claims={}, terminals={}",
                observation.process_count,
                observation.durable_claim_count,
                observation.terminal_count
            );
        }
        Ok(observation)
    }
}
