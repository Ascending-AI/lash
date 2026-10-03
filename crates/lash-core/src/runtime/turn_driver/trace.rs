use super::*;

impl RuntimeTurnDriver<'_> {
    pub(in crate::runtime) fn trace_context(
        &self,
        protocol_iteration: usize,
    ) -> lash_trace::TraceContext {
        lash_trace::TraceContext::default()
            .for_session(self.session_id.clone())
            .for_turn_index(self.turn_index)
            .for_protocol_iteration(protocol_iteration)
            .for_turn(self.turn_id.clone())
    }

    /// The trace id of one model call, named by the effect that made it, so a
    /// step body on any worker names the call the same way without a counter
    /// carried between steps.
    pub(super) fn llm_call_id(
        &self,
        protocol_iteration: usize,
        invocation: &crate::RuntimeInvocation,
    ) -> String {
        format!(
            "{}:{}:{}:{}",
            self.session_id,
            self.turn_index,
            protocol_iteration,
            invocation.effect_id().unwrap_or_default()
        )
    }

    /// Observes one event of this driver's own work at `protocol_iteration`.
    /// `event` is built only when the record will be emitted.
    pub(super) fn emit_trace(
        &self,
        protocol_iteration: usize,
        event: impl FnOnce() -> lash_trace::TraceEvent,
    ) {
        self.trace
            .observe(|| (self.trace_context(protocol_iteration), event()));
    }

    /// `None` when nothing observes the runtime, externally or as a product
    /// observer, keeping emission a no-op.
    pub(super) fn execution_tracing(
        &self,
        protocol_iteration: usize,
    ) -> Option<crate::RuntimeExecutionTracing> {
        let tracing = &self.host.core.tracing;
        Some({
            crate::RuntimeExecutionTracing::new(
                tracing.clone(),
                self.trace.scope().cloned(),
                self.trace_context(protocol_iteration),
            )
        })
    }

    pub(super) fn mark_phase_begin(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.begin(phase);
        }
    }

    pub(super) fn mark_phase_end(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.end(phase);
        }
    }
}

pub(in crate::runtime) fn protocol_step_trace_event(
    protocol_event: &crate::ProtocolEvent,
) -> lash_trace::TraceEvent {
    lash_trace::TraceEvent::ProtocolStep {
        plugin_id: protocol_event.plugin_id.clone(),
        payload: protocol_event.payload.clone(),
    }
}
