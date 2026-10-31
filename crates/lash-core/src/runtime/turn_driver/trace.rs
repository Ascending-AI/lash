use super::*;

impl RuntimeTurnDriver<'_> {
    pub(in crate::runtime) fn trace_context(
        &self,
        protocol_iteration: usize,
    ) -> lash_trace::TraceContext {
        turn_trace_context(
            &self.session_id,
            &self.turn_id,
            self.turn_index,
            protocol_iteration,
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
        execution_tracing(
            &self.host.core.tracing,
            &self.trace,
            self.trace_context(protocol_iteration),
        )
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
        payload: protocol_event.payload.clone().into_value(),
    }
}

/// A live measurement span; cancellation and failed commits close it on drop.
pub(in crate::runtime) struct TurnPhaseSpan {
    probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    phase: RuntimeTurnPhase,
}

impl TurnPhaseSpan {
    pub(in crate::runtime) fn begin(
        probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
        phase: RuntimeTurnPhase,
    ) -> Self {
        if let Some(probe) = &probe {
            probe.begin(phase);
        }
        Self { probe, phase }
    }
}

impl Drop for TurnPhaseSpan {
    fn drop(&mut self) {
        if let Some(probe) = &self.probe {
            probe.end(self.phase);
        }
    }
}

impl TurnEffectContext {
    pub(super) fn trace_context(&self, protocol_iteration: usize) -> lash_trace::TraceContext {
        turn_trace_context(
            &self.session_id,
            &self.turn_id,
            self.turn_index,
            protocol_iteration,
        )
    }

    pub(super) fn execution_tracing(
        &self,
        protocol_iteration: usize,
    ) -> Option<crate::RuntimeExecutionTracing> {
        execution_tracing(
            &self.host.core.tracing,
            &self.trace,
            self.trace_context(protocol_iteration),
        )
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
}

fn turn_trace_context(
    session_id: &SessionId,
    turn_id: &lash_vocabulary::identity::TurnId,
    turn_index: usize,
    protocol_iteration: usize,
) -> lash_trace::TraceContext {
    lash_trace::TraceContext::default()
        .for_session(session_id.clone())
        .for_turn_index(turn_index)
        .for_protocol_iteration(protocol_iteration)
        .for_turn(turn_id.clone())
}

fn execution_tracing(
    tracing: &crate::trace::TraceRuntime,
    standing: &crate::trace::TraceStanding,
    context: lash_trace::TraceContext,
) -> Option<crate::RuntimeExecutionTracing> {
    Some(crate::RuntimeExecutionTracing::new(
        tracing.clone(),
        standing.scope().cloned(),
        context,
    ))
}
