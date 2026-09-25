//! The worker's park of a refused process (FIG-3586, FIG-3571).

use super::*;

impl DurableProcessWorker {
    /// Park `process_id` when `error` is a refusal that parks: the body
    /// refused to replay its journal with nothing dispatched (FIG-3586), or
    /// its incarnation was started under a retired executable generation
    /// (FIG-3571). The process parks — non-terminal, with no terminal
    /// evidence — until an operator acts. The refusing park is what exempts
    /// its later sweeps from the attempt budget.
    pub(super) async fn park_refused_process(
        &self,
        process_id: &crate::ProcessId,
        error: &PluginError,
        park_authority: &crate::ProcessExecutionWriteAuthority,
    ) -> Result<(), PluginError> {
        let Some(reason) = error.park_reason() else {
            return Ok(());
        };
        let code = reason.code();
        let parked = self
            .config
            .process_registry()
            .park_process_with_authority(process_id, reason, park_authority)
            .await?;
        lash_core_ids::operational_metrics::record_work_parked("process", code.as_str());
        tracing::warn!(
            event = "process.parked",
            process_id = process_id.as_str(),
            reason_code = code.as_str(),
            effect_kind = parked
                .park
                .as_deref()
                .and_then(|park| park.reason.effect_kind())
                .unwrap_or_default(),
            attempts = parked.park.as_deref().map_or(0, |park| park.attempts),
            park_id = parked
                .park
                .as_deref()
                .map_or(0, |park| park.park_id.feed_sequence()),
            "process parked on a replay refusal"
        );
        Ok(())
    }
}
