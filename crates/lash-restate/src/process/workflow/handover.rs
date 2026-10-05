//! Process lifecycle publication precedes release of the segment journal pin.

use super::*;

impl<R: RestateProcessRunner> LashProcessWorkflowImpl<R> {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_segment(
        &self,
        context: &WorkflowContext<'_>,
        input: RestateProcessWorkflowInput,
        started: &SegmentStarted,
        end: SegmentRunEnd,
        journal_pin: scope_journal::ProcessJournalPin,
        writer: String,
    ) -> HandlerResult<RestateProcessWorkflowOutput> {
        let process_id = input.process_id.clone();
        let handover = match end {
            SegmentRunEnd::Terminal(proposal) => {
                let output = self
                    .complete_terminal_step(
                        context,
                        &process_id,
                        proposal,
                        Some((started.segment_ordinal(), started.started_at_ms())),
                    )
                    .await?;
                journal_pin.release(context, self.route.namespace()).await?;
                resolve_process_cancel_signal(
                    context,
                    RestateProcessCancelSignal::SegmentFinished,
                )?;
                return self
                    .deliver_segment_terminal(context, &process_id, input.segment_ordinal, output)
                    .await;
            }
            SegmentRunEnd::Boundary(handover) => handover,
        };
        resolve_process_cancel_signal(context, RestateProcessCancelSignal::SegmentFinished)?;
        let next_segment_ordinal = input.segment_ordinal.saturating_add(1);
        let successor_key = process_segment_workflow_key(&process_id, next_segment_ordinal);
        // The successor's reference names who owns the process now.
        // It is observational: the lost-run pass keys on the latest
        // handover, and admission, not the reference, decides whether
        // a segment runs. It is still written before the handover and
        // the send, so a visible handover always has its reference.
        // Both writes are one journaled step: a store fault ends the
        // attempt unrecorded and the redrive writes again,
        // compare-and-set on the ordinal. The successor's invocation
        // id is not known until the send, so the reference carries
        // none; the workflow key identifies the owner.
        let registry = &self.registry;
        let continuations = &self.continuations;
        let pid = &process_id;
        // The route is data (FIG-3795 S3/S5): it is recorded with the
        // handover, so the external reference, a forwarded cancel and a
        // redrive all address the recorded route rather than recomputing one.
        let successor_route = self.successor_route();
        let route = successor_route.name().into_owned();
        let written_generation = self.build_generation.clone();
        let reference_id = format!("{route}/{successor_key}");
        let Json(handed_over) = context
            .run_json_or_retry_send(PublishHandoverStep, async move {
                if let Err(error) = registry
                    .set_external_ref(
                        pid,
                        lash_core::ProcessExternalRef {
                            backend: "restate".to_string(),
                            id: reference_id,
                            metadata: None,
                            segment_ordinal: Some(next_segment_ordinal),
                        },
                    )
                    .await
                {
                    return step_fault(error);
                }
                match continuations
                    .put_segment_handover(
                        pid,
                        lash_core::PersistedSegmentHandover {
                            writer,
                            segment_ordinal: next_segment_ordinal,
                            written_generation,
                            route,
                            handover,
                        },
                    )
                    .await
                {
                    Ok(receipt) => {
                        if let (Some(tracing), Some(scope)) = (
                            self.tracing.clone().or_else(|| self.runner.tracing()),
                            receipt.record.scope.as_ref(),
                        ) {
                            completion::emit_segment_completion(
                                &tracing,
                                scope,
                                started.segment_ordinal(),
                                started.started_at_ms(),
                                receipt.record.committed_at_ms,
                                lash_trace::TraceDomainStatus::Yielded,
                                receipt.permit().as_ref(),
                            );
                        }
                        Ok(Ok(()))
                    }
                    Err(error) => step_fault(error),
                }
            })
            .await
            .map_err(HandlerError::from)?;
        if let Err(error) = handed_over {
            // Nothing was sent: the process is still this segment's to end.
            let output = self
                .fail_segment(
                    context,
                    &process_id,
                    input.segment_ordinal,
                    SegmentFailure::HandoverWrite(error),
                    SegmentSignal::Resolved,
                )
                .await?;
            journal_pin.release(context, self.route.namespace()).await?;
            return Ok(output);
        }
        // FIG-788: successor emission is unconditional. A cancellation
        // can land between attempts, so the recorded read below may
        // shape only commands after this deployed prefix.
        let successor = routed_workflow::<_, _, RestateProcessWorkflowOutput>(
            context,
            &successor_route,
            successor_key.clone(),
            "run",
            RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                process_id: process_id.clone(),
                registration: input.registration,
                execution_context: input.execution_context,
                segment_ordinal: next_segment_ordinal,
                sender_generation: self.build_generation.clone(),
            }),
        )
        .send()
        .await?;
        journal_pin
            .transfer_to(context, self.route.namespace(), successor.invocation_id())
            .await?;
        // Cancellation can race the gap after the current segment
        // retires its promise but before the successor handover is
        // visible to the cancel endpoint. Forward that durable fact
        // after scheduling so the successor owns terminalization; the
        // read is a step, so a redrive forwards exactly what the first
        // execution forwarded.
        let registry = &self.registry;
        let pid = &process_id;
        let Json(forward) = context
            .run_json_or_retry_send(ForwardCancelStep, async move {
                let record = match registry.get_process(pid).await {
                    Ok(Some(record)) => record,
                    // No process is left to cancel.
                    Ok(None) => return Ok(Ok(None)),
                    Err(error) => return step_fault(error),
                };
                if record.cancel_request.is_none() {
                    return Ok(Ok(None));
                }
                Ok(RestateProcessCancelRequest::from_record(&record)
                    .map(Some)
                    .map_err(|error| error.to_string()))
            })
            .await
            .map_err(HandlerError::from)?;
        // The successor carries the process from the send on, so a failure
        // from here cannot strand it: it ends only this invocation.
        if let Some(cancel) = forward.map_err(TerminalError::new)? {
            routed_workflow::<_, _, ()>(
                context,
                &successor_route,
                successor_key,
                "deliver_cancel",
                cancel,
            )
            .call()
            .await?;
        }
        // This segment has handed the process on and recorded the cancel it
        // forwards, so its handover is no longer anyone's to read: retire it,
        // and any older one. A redrive of this segment, before or after this
        // step, replays its runner from the handover its resume step
        // journaled (FIG-3809).
        if input.segment_ordinal > 0 {
            let continuations = &self.continuations;
            let pid = &process_id;
            let segment_ordinal = input.segment_ordinal;
            let Json(retired) = context
                .run_json_or_retry_send(RetireSegmentStep, async move {
                    match continuations
                        .retire_segment_handovers_through(pid, segment_ordinal)
                        .await
                    {
                        Ok(()) => Ok(Ok(())),
                        Err(error) => step_fault(error),
                    }
                })
                .await
                .map_err(HandlerError::from)?;
            if let Err(error) = retired {
                // Retiring is cleanup: a handover left behind is deleted with
                // the process when it is pruned, and the successor carries
                // the process either way.
                tracing::warn!(
                    process_id = process_id.as_str(),
                    segment_ordinal = input.segment_ordinal,
                    error = error.as_str(),
                    "a retired process segment could not delete its handovers"
                );
            }
        }
        Ok(RestateProcessWorkflowOutput::SegmentChained {
            next_segment_ordinal,
        })
    }
}

struct PublishHandoverStep;
impl crate::JournalStep for PublishHandoverStep {
    type Output = Result<(), String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.segment.handover";
    fn instance(&self) -> String {
        String::new()
    }
}

struct ForwardCancelStep;
impl crate::JournalStep for ForwardCancelStep {
    type Output = Result<Option<RestateProcessCancelRequest>, String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.segment.cancel-forward";
    fn instance(&self) -> String {
        String::new()
    }
}

struct RetireSegmentStep;
impl crate::JournalStep for RetireSegmentStep {
    type Output = Result<(), String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.segment.retire";
    fn instance(&self) -> String {
        String::new()
    }
}
