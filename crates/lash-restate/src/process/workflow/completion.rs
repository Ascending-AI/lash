use super::*;

/// Store `proposed` as the process's terminal, answering the outcome the
/// registry kept: this one, or the one an earlier writer committed.
pub(crate) async fn complete_process_outcome(
    registry: &Arc<dyn ProcessRegistry>,
    attachments: &dyn lash_core::AttachmentReferrers,
    process_id: &ProcessId,
    proposed: ProcessAwaitOutput,
    prelude: Vec<lash_core::ProcessEventAppendRequest>,
    tracing: Option<&lash_core::facade_support::TraceRuntime>,
    segment: Option<(u64, u64)>,
) -> Result<ProcessAwaitOutput, PluginError> {
    // The record holds what its terminal delivers before the registry
    // records it (ADR 0124 §4); a swept source publishes the typed
    // source-gone failure instead.
    let proposed = lash_core::runtime::attachment_delivery::publish_process_terminal(
        attachments,
        process_id,
        proposed,
    )
    .await?;
    let completion = registry
        .complete_process_with_prelude(
            process_id,
            proposed,
            prelude,
            workflow_key_authority(process_id),
        )
        .await?;
    let record = match completion {
        lash_core::ProcessCompletionOutcome::Committed(record) => {
            if let (Some(tracing), Some(scope)) = (tracing, record.trace.as_ref()) {
                let status = match record.outcome().and_then(|output| output.terminal_status()) {
                    Some(TerminalProcessStatus::Completed) => {
                        lash_trace::TraceDomainStatus::Completed
                    }
                    Some(TerminalProcessStatus::Cancelled) => {
                        lash_trace::TraceDomainStatus::Cancelled
                    }
                    _ => lash_trace::TraceDomainStatus::Failed,
                };
                if let Some((ordinal, started_at_ms)) = segment {
                    emit_segment_completion(
                        tracing,
                        scope,
                        ordinal,
                        started_at_ms,
                        record.updated_at_ms,
                        status,
                        Some(&lash_trace::EmissionPermit::new_transition()),
                    );
                }
                tracing.unreplayed(Some(scope.clone())).transition(
                    Some(&lash_trace::EmissionPermit::new_transition()),
                    record.updated_at_ms,
                    lash_trace::TraceTransitionKind::Terminal,
                    0,
                    || {
                        (
                            lash_trace::TraceContext::default(),
                            lash_trace::TraceEvent::DomainCompleted {
                                completion: lash_trace::TraceDomainCompletion::new(
                                    lash_trace::TraceDomainOperation::Process,
                                    scope.started_at_ms,
                                    status,
                                ),
                            },
                        )
                    },
                );
            }
            record
        }
        lash_core::ProcessCompletionOutcome::AlreadyApplied { stored }
        | lash_core::ProcessCompletionOutcome::Superseded { stored } => stored,
    };
    record.outcome().ok_or_else(|| {
        PluginError::Session(format!(
            "process `{process_id}` completion returned a non-terminal record"
        ))
    })
}

pub(super) fn emit_segment_completion(
    tracing: &lash_core::facade_support::TraceRuntime,
    scope: &lash_trace::DurableTraceScope,
    ordinal: u64,
    started_at_ms: u64,
    ended_at_ms: u64,
    status: lash_trace::TraceDomainStatus,
    permit: Option<&lash_trace::EmissionPermit>,
) {
    let mut segment = scope.clone();
    segment.scope = scope.scope.at_boundary(ordinal.saturating_add(1));
    tracing.unreplayed(Some(segment)).transition(
        permit,
        ended_at_ms,
        lash_trace::TraceTransitionKind::Terminal,
        0,
        || {
            (
                lash_trace::TraceContext::default(),
                lash_trace::TraceEvent::DomainCompleted {
                    completion: lash_trace::TraceDomainCompletion::new(
                        lash_trace::TraceDomainOperation::ProcessSegment,
                        started_at_ms,
                        status,
                    ),
                },
            )
        },
    );
}
