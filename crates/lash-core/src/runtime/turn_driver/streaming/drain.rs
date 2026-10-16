//! The tail of a model call whose cell the protocol accepted mid-stream
//! (FIG-6433).
//!
//! Accepting a cell does not end the provider's generation: the provider
//! bills all of it, and a host decorator settles from its provider terminal.
//! So the call hands its provider task
//! here instead of cancelling it, and the turn runs the cell at once. The
//! tail is read to the provider's terminal event off the turn's path, its
//! content discarded. The call's record, usage and span settle from what the
//! provider returned, or say the usage is unknown (ADR 0031) when the call's
//! deadline passes, the provider fails, or the turn's immediate cancel stops
//! the tail first. The turn settles every drain before it closes.

use super::*;

type ProviderTask =
    crate::task::JoinHandle<Result<crate::ProviderCompletion, crate::ProviderCompletionError>>;

/// What a call hands its drain: the provider task still running, the stream
/// it reports through, and what the call observed before its cell was
/// accepted.
pub(super) struct CallTail {
    pub(super) task: ProviderTask,
    pub(super) stream: crate::session_model::LlmStreamEventRx,
    pub(super) streamed_usage: LlmUsage,
    pub(super) sideband: crate::facade_support::ProviderCompletionSideband,
    pub(super) call_id: crate::LlmCallId,
    pub(super) protocol_suppressed_stop_sequences: bool,
    /// The call's pinned deadline on `clock`; `None` when the call has none.
    pub(super) deadline: Option<std::time::Instant>,
    pub(super) clock: Arc<dyn crate::Clock>,
    pub(super) trace: Option<TailTrace>,
}

/// The call's open span, closed when the drain ends.
pub(super) struct TailTrace {
    pub(super) call: crate::trace::OpenSpan,
    pub(super) context: lash_trace::TraceContext,
    /// The response the turn derived the cell from.
    pub(super) response: LlmResponse,
    pub(super) request_model: String,
    pub(super) started_at: std::time::Instant,
    pub(super) stream_summary: serde_json::Value,
}

/// A drained call: its sealed record, and its provider usage, `None` when
/// that never arrived.
pub(in crate::runtime::turn_driver) struct DrainedCall {
    pub(in crate::runtime::turn_driver) record: crate::LlmCallRecord,
    pub(in crate::runtime::turn_driver) usage: Option<LlmUsage>,
}

/// The model calls of one turn whose tails are still being read, shared by
/// the turn's driver and the effect bodies that run its calls.
#[derive(Clone, Default)]
pub(in crate::runtime) struct CallDrains(Arc<std::sync::Mutex<DrainSet>>);

#[derive(Default)]
struct DrainSet {
    pending: Vec<CallDrain>,
    /// How many drains the turn started: the ordinal of the newest.
    started: u64,
}

struct CallDrain {
    ordinal: u64,
    call_id: crate::LlmCallId,
    stop: CancellationToken,
    sideband: crate::facade_support::ProviderCompletionSideband,
    task: crate::task::JoinHandle<DrainedCall>,
}

impl Drop for CallDrain {
    /// A turn that goes away without settling its drain stops it: the
    /// drain seals the call unknown and closes its span on its own.
    fn drop(&mut self) {
        self.sideband.cancel(if self.stop.is_cancelled() {
            crate::provider::AttemptEndReason::ImmediateCancel
        } else {
            crate::provider::AttemptEndReason::TailDrainDrop
        });
        self.stop.cancel();
    }
}

impl CallDrains {
    fn set(&self) -> std::sync::MutexGuard<'_, DrainSet> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read `tail` to its end off the turn's path, until `stop` fires.
    pub(super) fn start(&self, tail: CallTail, stop: CancellationToken) {
        let call_id = tail.call_id.clone();
        let sideband = tail.sideband.clone();
        let task = crate::task::spawn(tail.drain(stop.clone()));
        let mut set = self.set();
        set.started = set.started.saturating_add(1);
        let ordinal = set.started;
        set.pending.push(CallDrain {
            ordinal,
            call_id,
            stop,
            sideband,
            task,
        });
    }

    /// The drains that ended, in the order their calls started: every drain
    /// when `wait`, which waits for each to end. Each comes with whether it
    /// is the turn's newest.
    pub(in crate::runtime::turn_driver) async fn settle(
        &self,
        wait: bool,
    ) -> Vec<(DrainedCall, bool)> {
        let (drains, newest) = {
            let mut set = self.set();
            let pending = std::mem::take(&mut set.pending);
            let (ended, running): (Vec<_>, Vec<_>) = pending
                .into_iter()
                .partition(|drain| wait || drain.task.is_finished());
            set.pending = running;
            (ended, set.started)
        };
        let mut settled = Vec::with_capacity(drains.len());
        for mut drain in drains {
            let drained = match (&mut drain.task).await {
                Ok(drained) => drained,
                Err(error) => {
                    let failure = crate::llm::transport::LlmTransportError::new(format!(
                        "the model call's tail drain failed: {error}"
                    ))
                    .with_kind(crate::ProviderFailureKind::Unknown)
                    .with_lash_code(TurnFailureCode::TaskJoinFailed)
                    .with_retry_verdict(crate::llm::transport::TransportRetryVerdict::NotRetryable);
                    DrainedCall {
                        record: crate::provider::synthetic_terminal_call_record(
                            drain.call_id.clone(),
                            crate::AttemptOutcome::Interrupted,
                            &failure,
                            true,
                            crate::ProtocolPosition::OutputStarted,
                            Vec::new(),
                        ),
                        usage: None,
                    }
                }
            };
            settled.push((drained, drain.ordinal == newest));
        }
        settled
    }
}

impl CallTail {
    async fn drain(mut self, stop: CancellationToken) -> DrainedCall {
        let joined = {
            let deadline = self.deadline;
            let clock = Arc::clone(&self.clock);
            let expiry = async move {
                match deadline {
                    Some(deadline) => clock.sleep_until(deadline).await,
                    None => futures_util::future::pending().await,
                }
            };
            futures_util::pin_mut!(expiry);
            let stopped = stop.cancelled();
            futures_util::pin_mut!(stopped);
            futures_util::select_biased! {
                () = stopped.fuse() => Err(crate::AttemptOutcome::Aborted),
                () = expiry.fuse() => Err(crate::AttemptOutcome::Interrupted),
                joined = (&mut self.task).fuse() => Ok(joined),
            }
        };
        let joined = match joined {
            Ok(joined) => joined,
            // The turn stopped the tail, or the call's deadline passed: the
            // provider task cooperatively stops, delivering its terminal at
            // the provider seam before returning its call record.
            Err(outcome) => {
                self.sideband.cancel(match outcome {
                    crate::AttemptOutcome::Aborted => {
                        crate::provider::AttemptEndReason::ImmediateCancel
                    }
                    _ => crate::provider::AttemptEndReason::ModelDeadline,
                });
                match (&mut self.task).await {
                    Ok(result) => Ok(result),
                    Err(_) => {
                        let failure = tail_stop_failure(outcome);
                        Ok(Err(crate::ProviderCompletionError {
                            call_record: Box::new(self.sideband.terminal_call_record(
                                self.call_id.clone(),
                                outcome,
                                &failure,
                                true,
                                crate::ProtocolPosition::OutputStarted,
                            )),
                            error: failure,
                        }))
                    }
                }
            }
        };
        // The tail's content is discarded; its usage is the fallback for a
        // response that reports none.
        while let Ok(event) = self.stream.try_recv() {
            match event {
                LlmStreamEvent::Usage(usage) => self.streamed_usage = usage,
                LlmStreamEvent::AttemptReset => self.streamed_usage = LlmUsage::default(),
                _ => {}
            }
        }
        let (mut record, usage, completion) = match joined {
            Ok(Ok(crate::ProviderCompletion {
                response,
                call_record,
            })) => {
                let usage = if response_usage_is_empty(&response.usage) {
                    self.streamed_usage.clone()
                } else {
                    response.usage.clone()
                };
                let reported =
                    response.provider_usage.is_some() || !response_usage_is_empty(&usage);
                (call_record, reported.then_some(usage), Some(response))
            }
            Ok(Err(failure)) => (*failure.call_record, None, None),
            Err(error) => {
                let failure = crate::llm::transport::LlmTransportError::new(format!(
                    "internal task failed: {error}"
                ))
                .with_kind(crate::ProviderFailureKind::Unknown)
                .with_lash_code(TurnFailureCode::TaskJoinFailed)
                .with_retry_verdict(crate::llm::transport::TransportRetryVerdict::NotRetryable);
                let record = self.sideband.terminal_call_record(
                    self.call_id.clone(),
                    crate::AttemptOutcome::Interrupted,
                    &failure,
                    true,
                    crate::ProtocolPosition::OutputStarted,
                );
                (record, None, None)
            }
        };
        // The provider cannot see the output the turn consumed before the
        // cell was accepted: an attempt that ended the call unsuccessfully
        // observed at least that output.
        if let Some(attempt) = record.attempts.last_mut()
            && attempt.outcome != crate::AttemptOutcome::Completed
            && matches!(
                attempt.protocol_position,
                crate::ProtocolPosition::NoResponse | crate::ProtocolPosition::ResponseObserved
            )
        {
            attempt.protocol_position = crate::ProtocolPosition::OutputStarted;
        }
        if self.protocol_suppressed_stop_sequences {
            record_protocol_owned_stop_suppression_in(&mut record);
        }
        if let Some(trace) = self.trace {
            let mut response = trace.response;
            response.usage = usage.clone().unwrap_or_else(LlmUsage::unknown_call);
            if let Some(completion) = completion {
                response.provider_usage = completion.provider_usage;
                response.response_metadata = completion.response_metadata;
                response.execution_evidence = completion.execution_evidence;
            }
            crate::runtime::effect::emit_llm_trace_completed(
                trace.call,
                trace.context,
                &response,
                &trace.request_model,
                self.clock
                    .now()
                    .saturating_duration_since(trace.started_at)
                    .as_millis() as u64,
                Some(trace.stream_summary),
                Some(&record),
            );
        }
        DrainedCall { record, usage }
    }
}

/// Why a tail stopped before its provider returned: the turn's immediate
/// cancel (`Aborted`), or the call's deadline (`Interrupted`).
fn tail_stop_failure(outcome: crate::AttemptOutcome) -> crate::llm::transport::LlmTransportError {
    let failure = match outcome {
        crate::AttemptOutcome::Aborted => {
            crate::llm::transport::LlmTransportError::new("cancelled")
                .with_kind(crate::ProviderFailureKind::Unknown)
                .with_lash_code(TurnFailureCode::Cancelled)
                .with_terminal_reason(crate::LlmTerminalReason::Cancelled)
        }
        _ => crate::llm::transport::LlmTransportError::new(
            "the model call reached its deadline before its provider returned",
        )
        .with_kind(crate::ProviderFailureKind::Timeout)
        .with_lash_code(TurnFailureCode::ModelTotalExceeded),
    };
    failure.with_retry_verdict(crate::llm::transport::TransportRetryVerdict::NotRetryable)
}
