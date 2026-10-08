use super::RuntimeExecutionContext;
use lash_sansio::sync::MutexExt;

/// What a call's trace start left for its completion: the scope it opened,
/// when, and the context it was observed under.
#[derive(Clone)]
pub(crate) struct TracedToolCall {
    scope: lash_trace::DurableTraceScope,
    context: lash_trace::TraceContext,
}

/// What a call's start trace names.
pub(crate) struct ToolCallStart<'a> {
    pub(crate) call_id: &'a crate::ToolCallId,
    pub(crate) provider_call_id: Option<&'a str>,
    pub(crate) tool: &'a str,
    pub(crate) args: &'a serde_json::Value,
}

impl<'a> From<&'a crate::ToolCallRecord> for ToolCallStart<'a> {
    fn from(record: &'a crate::ToolCallRecord) -> Self {
        Self {
            call_id: &record.call_id,
            provider_call_id: record.provider_call_id.as_deref(),
            tool: &record.tool,
            args: &record.args,
        }
    }
}

impl RuntimeExecutionContext<'_> {
    /// Observe the start of a call under the call's own trace scope,
    /// admitted now. Each execution that reaches the call observes it:
    /// nothing replays one (ADR 0132 §5).
    pub(crate) fn trace_tool_call_started(
        &self,
        start: ToolCallStart<'_>,
        requested_at_ms: u64,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        if let Some(proposal) = self.propose_tool_trace(start.call_id, requested_at_ms)? {
            proposal
                .candidate
                .settle(lash_trace::TraceCandidateOutcome::Selected);
            self.trace_tool_call_admitted(start, proposal.scope);
        }
        Ok(())
    }

    /// Propose the admission of `call_id`'s trace scope, requested at
    /// `requested_at_ms`, leaving its candidate for the caller to settle.
    /// `None` without tracing.
    pub(crate) fn propose_tool_trace(
        &self,
        call_id: &crate::ToolCallId,
        requested_at_ms: u64,
    ) -> Result<
        Option<crate::runtime::actor::round::TraceProposal>,
        crate::RuntimeEffectControllerError,
    > {
        let Some(tracing) = &self.tracing else {
            return Ok(None);
        };
        let opener = crate::EffectOpener::for_scope(&self.admitted_scope()).map_err(|error| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeToolRunShape,
                error.to_string(),
            )
        })?;
        let mut scope = crate::trace::tool_trace_scope(
            &opener,
            tracing.scope.as_ref(),
            call_id,
            requested_at_ms,
        );
        let candidate = tracing.runtime.scopes().propose(&scope.scope, &scope.cause);
        scope.anchor = candidate.anchor();
        Ok(Some(crate::runtime::actor::round::TraceProposal {
            scope,
            candidate,
        }))
    }

    /// Observe the start of a call under `scope`, the trace scope its
    /// admission selected: retained by its round's admission, it is read
    /// back by every owner of the round and admitted by none (FIG-5382).
    pub(crate) fn trace_tool_call_admitted(
        &self,
        start: ToolCallStart<'_>,
        scope: lash_trace::DurableTraceScope,
    ) {
        let Some(tracing) = &self.tracing else {
            return;
        };
        let context = tracing.scope_context.clone();
        let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
        self.coordination_standing(tracing)
            .under(scope.clone())
            .observe(|| {
                (
                    context.clone(),
                    lash_trace::TraceEvent::ToolCallStarted {
                        call_id: start.call_id.clone(),
                        provider_call_id: start.provider_call_id.map(str::to_owned),
                        name: start.tool.to_owned(),
                        args: start.args.clone(),
                        issuing_node_id: issuing_node,
                    },
                )
            });
        self.tool_requests
            .lock_recover()
            .insert(start.call_id.clone(), TracedToolCall { scope, context });
    }

    /// Observe the completion of a call whose start this execution traced.
    pub(crate) fn trace_tool_call_completed(
        &self,
        record: &crate::ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
    ) {
        let Some(tracing) = &self.tracing else {
            return;
        };
        let Some(TracedToolCall { scope, context }) =
            self.tool_requests.lock_recover().remove(&record.call_id)
        else {
            return;
        };
        let duration_ms = self
            .dispatch
            .clock
            .timestamp_ms()
            .saturating_sub(scope.started_at_ms);
        let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
        self.coordination_standing(tracing)
            .under(scope)
            .observe(|| {
                (
                    context,
                    lash_trace::TraceEvent::ToolCallCompleted {
                        call_id: record.call_id.clone(),
                        provider_call_id: record.provider_call_id.clone(),
                        name: record.tool.clone(),
                        args: record.args.clone(),
                        output: crate::trace::trace_tool_call_output(&record.output),
                        duration_ms,
                        issuing_node_id: issuing_node,
                        attempts: (!attempts.is_empty()).then(|| attempts.to_vec()),
                    },
                )
            });
    }
}
