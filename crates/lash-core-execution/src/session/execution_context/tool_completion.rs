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
        if let Some(scope) = self.tool_trace_scope(start.call_id, requested_at_ms)? {
            self.trace_tool_call_admitted(start, scope);
        }
        Ok(())
    }

    /// The trace scope of `call_id`, requested at `requested_at_ms`, under
    /// its issuing container: the cell it was called from, else the turn's
    /// (or process's) span. Its span is derived from that parent's, so every
    /// owner that reads the admission back names the same one. `None`
    /// without tracing.
    pub(crate) fn tool_trace_scope(
        &self,
        call_id: &crate::ToolCallId,
        requested_at_ms: u64,
    ) -> Result<Option<lash_trace::DurableTraceScope>, crate::RuntimeEffectControllerError> {
        let Some(tracing) = &self.tracing else {
            return Ok(None);
        };
        let opener = crate::EffectOpener::for_scope(&self.admitted_scope()).map_err(|error| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeToolRunShape,
                error.to_string(),
            )
        })?;
        Ok(Some(crate::trace::tool_trace_scope(
            &opener,
            tracing.parent().as_ref(),
            call_id,
            requested_at_ms,
        )))
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
        let context = tracing
            .scope_context
            .clone()
            .for_call(start.call_id.clone());
        let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
        let content = tracing.runtime.content();
        self.coordination_standing(tracing)
            .under(scope.clone())
            .open_scope(|| {
                (
                    context.clone(),
                    lash_trace::TraceEvent::ToolCallStarted {
                        call_id: start.call_id.clone(),
                        provider_call_id: start.provider_call_id.map(str::to_owned),
                        name: start.tool.to_owned(),
                        args: content.capture(|| start.args.clone()),
                        issuing_node_id: issuing_node,
                    },
                )
            });
        self.tool_requests
            .lock_recover()
            .insert(start.call_id.clone(), TracedToolCall { scope, context });
    }

    /// Trace the rest of `call_id`, whose start the owner that admitted it
    /// under `scope` observed, under that scope: its completion is this
    /// execution's to observe.
    pub(crate) fn adopt_tool_call_trace(
        &self,
        call_id: &crate::ToolCallId,
        scope: lash_trace::DurableTraceScope,
    ) {
        let Some(tracing) = &self.tracing else {
            return;
        };
        let context = tracing.scope_context.clone().for_call(call_id.clone());
        self.tool_requests
            .lock_recover()
            .insert(call_id.clone(), TracedToolCall { scope, context });
    }

    /// Close the span of a call whose start this execution traced, at
    /// `ended_at_ms`: the end its outcome's commit retained, or now for a
    /// call that commits nothing. Only the owner whose acknowledged commit
    /// wrote the outcome calls it (P1).
    pub(crate) fn trace_tool_call_completed(
        &self,
        record: &crate::ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
        ended_at_ms: Option<u64>,
    ) {
        let Some(tracing) = &self.tracing else {
            return;
        };
        let Some(TracedToolCall { scope, context }) =
            self.tool_requests.lock_recover().remove(&record.call_id)
        else {
            return;
        };
        let ended_at_ms = ended_at_ms.unwrap_or_else(|| self.dispatch.clock.timestamp_ms());
        let duration_ms = ended_at_ms.saturating_sub(scope.started_at_ms);
        let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
        let content = tracing.runtime.content();
        self.coordination_standing(tracing)
            .under(scope)
            .close_scope(ended_at_ms, || {
                (
                    context,
                    lash_trace::TraceEvent::ToolCallCompleted {
                        call_id: record.call_id.clone(),
                        provider_call_id: record.provider_call_id.clone(),
                        name: record.tool.clone(),
                        args: content.capture(|| record.args.clone()),
                        output: crate::trace::trace_tool_call_output(&record.output, content),
                        duration_ms,
                        issuing_node_id: issuing_node,
                        attempts: (!attempts.is_empty()).then(|| attempts.to_vec()),
                    },
                )
            });
    }
}
