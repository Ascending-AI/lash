//! Execution-side delivery of a process segment's cancellation (FIG-3673).
//!
//! One responsibility: while a process segment's runner is live, fire the stop
//! the segment lends its step bodies (tool attempts, model calls, the tool
//! children a live opener lends it) once the segment's durable cancel promise
//! resolves. The drive never reads this stop. A step body that observes it
//! records a cancelled outcome, and that recorded outcome is what reaches the
//! drive; the drive's own observations of the cancellation are the recorded
//! races and peeks of the promise itself.
//!
//! This module sits outside the drive modules on purpose: its watch is a live
//! ingress long-poll on a spawned task, which no drive code may await.

use std::future::Future;
use std::time::Duration;

use lash_sansio::ProcessId;
use restate_sdk::errors::HandlerError;
use tokio_util::sync::CancellationToken;

use crate::ingress::RestateIngressClient;
use crate::process::{RestateProcessAwaitRequest, RestateProcessCancelSignal};

/// How many consecutive watch faults the delivery retries before it fails the
/// attempt: the ladder every execution-side gate watch shares (FIG-3672 P9),
/// eight attempts from 25ms doubling to 1s.
const WATCH_RETRY_ATTEMPTS: u32 = 8;
const WATCH_RETRY_FIRST_DELAY: Duration = Duration::from_millis(25);
const WATCH_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

/// The live watch that stops one segment attempt's step bodies.
pub(crate) struct ProcessStopDelivery {
    watch: Option<tokio::task::JoinHandle<HandlerError>>,
}

impl ProcessStopDelivery {
    /// No delivery: a segment driven without an ingress (a test) stops its
    /// step bodies only through its recorded outcomes.
    pub(crate) fn none() -> Self {
        Self { watch: None }
    }

    /// Watch `workflow_key`'s cancel promise over `ingress` and fire `stop`
    /// when it holds an accepted cancel request.
    ///
    /// The watch fails closed. A transport fault is retried on the shared
    /// ladder; a watch that exhausts it, or addresses a workflow nothing
    /// binds, ends the attempt with an unrecorded infrastructure error
    /// (FIG-1579), never a cancellation and never a recorded outcome: the
    /// engine retries the attempt, whose redrive replays its journal and
    /// watches again. A segment therefore never runs a step body its
    /// committed cancel cannot reach.
    pub(crate) fn watch(
        ingress: RestateIngressClient,
        process_id: ProcessId,
        workflow_key: String,
        stop: CancellationToken,
    ) -> Self {
        let request = RestateProcessAwaitRequest { process_id };
        let watch = lash_core::task::spawn(async move {
            let mut faults = 0;
            let mut delay = WATCH_RETRY_FIRST_DELAY;
            loop {
                match ingress
                    .call_workflow_json::<_, RestateProcessCancelSignal>(
                        crate::LashService::ProcessWorkflow.name(),
                        &workflow_key,
                        "await_cancel",
                        &request,
                    )
                    .await
                {
                    Ok(RestateProcessCancelSignal::CancelRequested) => {
                        stop.cancel();
                        return std::future::pending().await;
                    }
                    Ok(RestateProcessCancelSignal::SegmentFinished) => {
                        return std::future::pending().await;
                    }
                    Err(error) if error.is_timeout() => {
                        // The attach ceiling bounds one transport connection,
                        // not the watch: re-attach.
                        faults = 0;
                        delay = WATCH_RETRY_FIRST_DELAY;
                    }
                    Err(error) if error.is_service_unregistered() => {
                        // Retrying cannot make the binding appear (FIG-1579).
                        return crate::ingress::unregistered_service_terminal(
                            crate::LashService::ProcessWorkflow.name(),
                            "await_cancel",
                            &error,
                        )
                        .into();
                    }
                    Err(error) => {
                        faults += 1;
                        if faults >= WATCH_RETRY_ATTEMPTS {
                            return HandlerError::from(lash_core::PluginError::Runtime(
                                lash_core::RuntimeError::new(
                                    lash_core::RuntimeErrorCode::TransientCancelWatch,
                                    format!(
                                        "process `{workflow_key}` cancel watch failed \
                                         {faults} times: {error}"
                                    ),
                                ),
                            ));
                        }
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(WATCH_RETRY_MAX_DELAY);
                    }
                }
            }
        });
        Self { watch: Some(watch) }
    }

    /// Run `runner` for as long as the watch holds. A watch fault ends the
    /// attempt with that fault; the runner's own end ends the watch.
    pub(crate) async fn drive<T>(
        mut self,
        runner: impl Future<Output = T>,
    ) -> Result<T, HandlerError> {
        let Some(watch) = self.watch.as_mut() else {
            return Ok(runner.await);
        };
        tokio::pin!(runner);
        tokio::select! {
            biased;
            output = &mut runner => Ok(output),
            fault = watch => Err(fault.unwrap_or_else(|join| {
                HandlerError::from(lash_core::PluginError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::TransientCancelWatch,
                    format!("process cancel watch task ended: {join}"),
                )))
            })),
        }
    }
}

impl Drop for ProcessStopDelivery {
    fn drop(&mut self) {
        if let Some(watch) = self.watch.take() {
            watch.abort();
        }
    }
}
