//! The chat-loop cases' own plumbing (FIG-6841): the stamped benchmark tool,
//! the steered send into a running turn, and the RLM cell core.

use std::sync::Arc;

use anyhow::{Context, Result};
use lash_core::{ToolAttemptOutcome, ToolContract, ToolManifest, ToolProvider};
use lash_core_ids::perf_witness::latency::Stamp;

use super::loop_spans::{LoopClocks, ToolStamp};
use super::provider::{LaneHold, ProviderTiming, STEERED_TEXT};
use super::runner::{
    CaseSpec, CaseTopology, STORE_POLL_TIMEOUT, Sample, SampleCtx, create_lane_session,
    measure_send, observer_poll_store,
};
use crate::runtime_perf::providers::BenchmarkEchoTool;
use lash_protocol_types::SessionId;

/// The benchmark echo tool, stamping each execution's own start and return
/// under its session.
pub(crate) struct StampedTools {
    inner: BenchmarkEchoTool,
    timing: Arc<ProviderTiming>,
}

impl StampedTools {
    pub(crate) fn new(inner: BenchmarkEchoTool, timing: Arc<ProviderTiming>) -> Self {
        Self { inner, timing }
    }
}

#[async_trait::async_trait]
impl ToolProvider for StampedTools {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
    {
        self.inner.tool_manifests(config)
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.inner.resolve_contract(name, config)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolAttemptOutcome {
        let session = call
            .context
            .session_id()
            .map(|session| session.as_str().to_owned())
            .unwrap_or_default();
        let started = Stamp::now();
        let outcome = self.inner.execute(call).await;
        self.timing.stamps.tool(
            &session,
            ToolStamp {
                started,
                returned: Stamp::now(),
            },
        );
        outcome
    }
}

/// The RLM protocol the `cell` case's core serves: TypeScript cells on the
/// cell channel under the standard instruction bound.
pub(crate) fn cell_protocol() -> Result<lash_protocol_rlm::RlmProtocolPluginFactory> {
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::standard())
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        lash_protocol_rlm::CellDialect::typescript(),
    );
    // Kernel workers start with the topology, before any measured send.
    factory.worker_service().pool()?;
    Ok(factory)
}

/// One `steer` sample: the measured turn's provider call holds; while it
/// runs, the host steers another input into that turn and releases the call
/// the moment the steered input is accepted. The provider stamps the call
/// that carries it.
pub(crate) async fn steer_sample(
    ctx: &SampleCtx<'_>,
    lane: usize,
    index: usize,
    hold: &LaneHold,
) -> Result<Sample> {
    let turn = lash::TurnId::parse(format!("latency-{}-{lane}-{index}", ctx.spec.name))
        .map_err(|error| anyhow::anyhow!("steer turn id: {error}"))?;
    hold.arm();
    let steer = async {
        tokio::time::timeout(STORE_POLL_TIMEOUT, hold.started.notified())
            .await
            .context("the steered turn's provider call never started")?;
        let steer_request = Stamp::now();
        let steered = ctx
            .session
            .send(lash::TurnInput::text(format!(
                "{STEERED_TEXT} {lane}/{index}"
            )))
            .ingress(lash_core::TurnInputIngress::active_turn(
                turn.clone(),
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ))
            .into_future()
            .await
            .map_err(anyhow::Error::from);
        let steer_accepted = Stamp::now();
        hold.release_one();
        let steered = steered?;
        Ok::<_, anyhow::Error>((steered, steer_request, steer_accepted))
    };
    let measured = measure_send(
        ctx,
        lane,
        index,
        Some(turn.clone()),
        LoopClocks::default(),
        || {},
    );
    let (sample, steered) = tokio::join!(measured, steer);
    let mut sample = sample?;
    let (steered, steer_request, steer_accepted) = steered?;
    // The steered input settles with the run it joined.
    let _ = steered.outcome().await;
    sample.loop_clocks.steer_request = Some(steer_request);
    sample.loop_clocks.steer_accepted = Some(steer_accepted);
    Ok(sample)
}

/// One `cold` lane: every sample opens a fresh session, so every send is
/// that session's first turn; the open is stamped before the send.
pub(crate) async fn cold_lane(
    spec: &CaseSpec,
    topology: &CaseTopology,
    lane: usize,
    count: usize,
    timing: &Arc<ProviderTiming>,
) -> Result<Vec<Sample>> {
    let mut samples = Vec::with_capacity(count);
    for index in 0..count {
        let session_id = SessionId::fixture(format!("latency-{}-{lane}-{index}", spec.name));
        let open_started = Stamp::now();
        let session = create_lane_session(topology, spec.topology, &session_id).await?;
        let poll_store = observer_poll_store(&topology.observer, &session_id).await?;
        let ctx = SampleCtx {
            spec,
            session: &session,
            poll_store: &poll_store,
            session_id: &session_id,
            timing,
        };
        let clocks = LoopClocks {
            session_open_started: Some(open_started),
            ..LoopClocks::default()
        };
        let mut sample = measure_send(&ctx, lane, index, None, clocks, || {}).await?;
        sample.cold = true;
        samples.push(sample);
        let _ = session.close().await;
    }
    Ok(samples)
}
