use super::RuntimeExecutionContext;
use lash_sansio::sync::MutexExt;

/// What a call's trace start left for its completion: the scope it opened,
/// when, and the context it was observed under.
#[derive(Clone)]
pub(crate) struct TracedToolCall {
    scope: lash_trace::DurableTraceScope,
    context: lash_trace::TraceContext,
    /// What the attempt that ran last annotated the span with, in memory
    /// only: the closer stamps it (§7.4 rule 2).
    annotations: std::sync::Arc<std::sync::Mutex<lash_trace::ToolSpanAnnotations>>,
}

impl TracedToolCall {
    fn new(scope: lash_trace::DurableTraceScope, context: lash_trace::TraceContext) -> Self {
        Self {
            scope,
            context,
            annotations: std::sync::Arc::default(),
        }
    }
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
        let standing = self
            .coordination_standing(tracing)
            .under(scope.clone())
            .selected(lash_trace::TraceSpanKind::Tool, &context.ids());
        let content = standing.content();
        standing.open_scope(|| {
            (
                context.clone(),
                lash_trace::TraceEvent::ToolCallStarted {
                    call_id: start.call_id.clone(),
                    provider_call_id: start.provider_call_id.map(str::to_owned),
                    name: start.tool.to_owned(),
                    args: content.capture(lash_trace::ContentCategory::ToolArguments, || {
                        start.args.clone()
                    }),
                    issuing_node_id: issuing_node,
                },
            )
        });
        self.tool_requests
            .lock_recover()
            .insert(start.call_id.clone(), TracedToolCall::new(scope, context));
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
            .insert(call_id.clone(), TracedToolCall::new(scope, context));
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
        let Some(TracedToolCall {
            scope,
            context,
            annotations,
        }) = self.tool_requests.lock_recover().remove(&record.call_id)
        else {
            return;
        };
        let ended_at_ms = ended_at_ms.unwrap_or_else(|| self.dispatch.clock.timestamp_ms());
        let duration_ms = ended_at_ms.saturating_sub(scope.started_at_ms);
        let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
        let standing = self
            .coordination_standing(tracing)
            .under(scope)
            .selected(lash_trace::TraceSpanKind::Tool, &context.ids());
        let content = standing.content();
        standing.close_scope(ended_at_ms, || {
            (
                context,
                lash_trace::TraceEvent::ToolCallCompleted {
                    call_id: record.call_id.clone(),
                    provider_call_id: record.provider_call_id.clone(),
                    name: record.tool.clone().into(),
                    args: content
                        .capture(lash_trace::ContentCategory::ToolArguments, || {
                            record.args.clone()
                        })
                        .into(),
                    output: crate::trace::trace_tool_call_output(&record.output, content),
                    duration_ms,
                    issuing_node_id: issuing_node,
                    attempts: (!attempts.is_empty()).then(|| attempts.to_vec()),
                    span_attributes: std::mem::take(&mut *annotations.lock_recover()),
                },
            )
        });
    }

    /// The position the tool hooks of `call_id` run at: the call's tool
    /// span, from the scope its start traced, else the scope its admission
    /// will select. They run with the shift that issues the call's steps,
    /// so what they observe behind the journal frontier waits for it.
    pub(crate) fn tool_hook_trace(
        &self,
        call_id: &crate::ToolCallId,
    ) -> crate::trace::ExtensionTrace {
        let Some(tracing) = &self.tracing else {
            return crate::trace::ExtensionTrace::untraced();
        };
        let traced = self.tool_requests.lock_recover().get(call_id).cloned();
        let (scope, context) = match traced {
            Some(traced) => (traced.scope, traced.context),
            None => {
                let Ok(Some(scope)) =
                    self.tool_trace_scope(call_id, self.dispatch.clock.timestamp_ms())
                else {
                    return crate::trace::ExtensionTrace::untraced();
                };
                (
                    scope,
                    tracing.scope_context.clone().for_call(call_id.clone()),
                )
            }
        };
        let standing = self
            .coordination_standing(tracing)
            .under(scope)
            .selected(lash_trace::TraceSpanKind::Tool, &context.ids());
        crate::trace::ExtensionTrace::seed(standing, context)
    }

    /// What one attempt body of `call` binds its capability from: the
    /// call's tool span, entered in the attempt's live step, or as its own
    /// live attempt when the round runs the body natively. `None` for a
    /// call whose start this execution did not trace.
    pub(crate) fn tool_body_spec(
        &self,
        call: crate::CallAddress,
        live: Option<&std::sync::Arc<crate::trace::LiveStep>>,
    ) -> Option<crate::trace::BodySpec> {
        let tracing = self.tracing.as_ref()?;
        let TracedToolCall {
            scope,
            context,
            annotations,
        } = self
            .tool_requests
            .lock_recover()
            .get(&call.call_id)?
            .clone();
        // A body a recorded step runs is that step's live attempt; one the
        // round runs natively is no journal's, so each run is its own.
        let standing = self.coordination_standing(tracing).under(scope);
        let standing = match live {
            Some(live) => standing.in_body(live),
            None => standing,
        }
        .selected(lash_trace::TraceSpanKind::Tool, &context.ids());
        let mut spec = crate::trace::BodySpec::at(standing, context);
        spec.call = Some(call);
        spec.tool_span = Some(annotations);
        spec.cell = self
            .held_response()
            .cloned()
            .zip(self.code_block_graph_key());
        Some(spec)
    }
}
