//! A logical model call holds its request facts and sealed provider attempts
//! until its one closing record publishes the call and its attempt ledger.

use super::*;
use std::collections::BTreeMap;

pub(super) struct ModelSpanState {
    facts: lash_trace::TraceModelCall,
    attempts: Arc<Mutex<BTreeMap<u32, lash_trace::TraceModelAttemptSpan>>>,
}

/// A leaf span opened by `TraceStanding::open`. Its closing record is its
/// only publication path, including any sealed model-attempt children.
#[must_use = "an open span publishes only when it is closed"]
pub struct OpenSpan {
    pub(super) standing: TraceStanding,
    pub(super) model: Option<ModelSpanState>,
    pub(crate) request: Option<lash_trace::TraceLlmRequest>,
}

impl OpenSpan {
    pub(crate) fn select_record(&mut self, ids: &TraceIds) {
        let kind = self
            .standing
            .leaf
            .as_ref()
            .map_or(TraceSpanKind::Model, |leaf| leaf.kind);
        self.standing = self.standing.selected(kind, ids);
    }
    /// The model call's own span; absent when its detail is disabled or no
    /// observed container exists. Never substitutes the enclosing span.
    pub fn carrier(&self) -> Option<&TraceCarrier> {
        self.standing
            .leaf
            .as_ref()
            .filter(|leaf| leaf.kind == TraceSpanKind::Model && self.standing.is_observed())
            .map(|leaf| &leaf.span)
    }

    pub fn standing(&self) -> &TraceStanding {
        &self.standing
    }

    pub(crate) fn with_model(mut self, facts: impl FnOnce() -> lash_trace::TraceModelCall) -> Self {
        if self.standing.is_observed() {
            self.model = Some(ModelSpanState {
                facts: facts(),
                attempts: Arc::default(),
            });
        }
        self
    }

    /// Bind observation at the provider seam. The sideband mints each real
    /// attempt's leaf id when it seals it; the execution ledger keeps no
    /// telemetry identity or clocks.
    pub fn provider_attempts(
        &self,
        sideband: lash_core_llm::core_internal::ProviderCompletionSideband,
        context: TraceContext,
    ) -> lash_core_llm::core_internal::ProviderCompletionSideband {
        let standing = &self.standing;
        let Some(model) = &self.model else {
            return sideband;
        };
        if !standing
            .runtime
            .parts
            .detail
            .builds(TraceSpanKind::ModelAttempt)
        {
            return sideband;
        }
        let attempts = model.attempts.clone();
        let standing = standing.clone();
        let identity = standing.runtime.parts.identity.clone();
        let clock = standing.runtime.clock().clone();
        sideband.with_attempt_observer(
            Arc::new(move |attempt, observation, response_metadata| {
                let standing = standing.selected(TraceSpanKind::ModelAttempt, &context.ids());
                if !standing.is_observed() {
                    return;
                }
                if let Some(span) = &attempt.span {
                    attempts
                        .lock_recover()
                        .insert(attempt.ordinal, span.clone());
                }
                standing.observe(|| {
                    (
                        context.clone(),
                        TraceEvent::LlmAttemptCompleted {
                            response_metadata: standing
                                .content()
                                .capture(lash_trace::ContentCategory::ResponseMetadata, || {
                                    response_metadata.clone()
                                }),
                            attempt: attempt.attempt,
                            observation,
                        },
                    )
                });
            }),
            clock,
            identity,
        )
    }

    /// Close with the request facts and provider clocks from this execution,
    /// joining the trace metadata to the final ledger by attempt ordinal.
    pub fn close(self, record: impl FnOnce() -> (TraceContext, TraceEvent)) {
        let role = if self.standing.leaf.is_some() {
            SpanRole::Closes
        } else {
            SpanRole::Event
        };
        self.standing.observe_as(role, None, || {
            let (context, mut event) = record();
            if let Some(state) = self.model {
                match &mut event {
                    TraceEvent::LlmCallCompleted {
                        model, attempts, ..
                    }
                    | TraceEvent::LlmCallFailed {
                        model, attempts, ..
                    } => {
                        *model = Some(state.facts);
                        let sealed = state.attempts.lock_recover();
                        for attempt in attempts.iter_mut().flatten() {
                            attempt.span = sealed.get(&attempt.ordinal).cloned();
                        }
                    }
                    _ => {}
                }
            }
            (context, event)
        });
    }
}
