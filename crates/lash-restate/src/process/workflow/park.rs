use super::*;

impl<R> LashProcessWorkflowImpl<R>
where
    R: RestateProcessRunner,
{
    /// Park `process_id` on the replay refusal `refusal` (FIG-3659 NOW-B).
    ///
    /// Best effort, like a turn's park: the attempt fails retryably either
    /// way, and a retry that refuses again writes the park again, so a failed
    /// write is logged rather than allowed to turn a park into a failure.
    pub(super) async fn park_diverged_process(
        &self,
        process_id: &ProcessId,
        refusal: &PluginError,
        started: &SegmentStarted,
    ) {
        let Some(reason) = refusal.park_reason() else {
            return;
        };
        let code = reason.code();
        // The park carries the checkpoint's generation stamp (FIG-3795 S8):
        // the recorded admission's build generation, never the refusing
        // build's own.
        let write = lash_core::store::ProcessParkWrite {
            reason,
            engine: None,
            build_generation: started.build_generation().cloned(),
        };
        let parked = match self.registry.get_process(process_id).await {
            Ok(Some(record)) => {
                self.registry
                    .park_process_with_authority(
                        process_id,
                        write,
                        &park_authority(&record, started),
                    )
                    .await
            }
            Ok(None) => Err(lash_core::runtime::registry_transitions::unknown_process(
                process_id,
            )),
            Err(error) => Err(error),
        };
        match parked {
            Ok(parked) => {
                let tracing = self
                    .tracing
                    .clone()
                    .or_else(|| self.runner.tracing())
                    .unwrap_or_default();
                lanes::observe_refusal_park(
                    tracing.metrics(),
                    parked.permit().as_ref(),
                    process_id,
                    code,
                    &parked.record,
                );
            }
            Err(error) => tracing::error!(
                event = "process.park_record_failed",
                process_id = process_id.as_str(),
                reason_code = code.as_str(),
                error = %error,
                "a diverged process could not record its park"
            ),
        }
    }
}
