//! Observation identities and trace standing of an execution.
use super::*;

/// Passive identity of the phase that requested a call. The logical owner
/// supplies the observer, trace runtime and emission authority.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ToolObservationAttribution {
    pub context: Option<lash_trace::TraceContext>,
    pub graph_key: Option<String>,
    pub issuing_node_id: Option<String>,
}

/// Trace handle threaded into tool execution so per-tool trace events are
/// emitted from the single shared seam, whichever protocol executes the turn.
///
/// `scope` is the scope the execution runs under, a turn's or a process's, and
/// `scope_context` carries the matching record identity (session / turn /
/// iteration) so [`crate::trace::assign_span_identity`] stamps
/// `tool:<call_id>` under the right turn. The right to emit is not held here:
/// each emission takes it from the controller its execution issues steps
/// through.
#[derive(Clone)]
pub struct RuntimeExecutionTracing {
    pub(super) runtime: crate::trace::TraceRuntime,
    pub(super) scope: Option<lash_trace::DurableTraceScope>,
    pub(super) scope_context: lash_trace::TraceContext,
}

impl RuntimeExecutionTracing {
    pub fn new(
        runtime: crate::trace::TraceRuntime,
        scope: Option<lash_trace::DurableTraceScope>,
        scope_context: lash_trace::TraceContext,
    ) -> Self {
        Self {
            runtime,
            scope,
            scope_context,
        }
    }

    /// The runtime's shared trace handle.
    pub fn runtime(&self) -> &crate::trace::TraceRuntime {
        &self.runtime
    }

    /// The scope the execution runs under.
    pub fn scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        self.scope.as_ref()
    }

    /// The standing of the coordination that issues a call's steps through
    /// `controller`: the tool lifecycle is observed once those steps' bodies
    /// have really run.
    pub(crate) fn coordination(
        &self,
        controller: &crate::ScopedEffectController<'_>,
    ) -> crate::trace::TraceStanding {
        self.runtime.shift(self.scope.clone(), controller)
    }
}

impl RuntimeExecutionContext<'_> {
    /// A fresh observation cursor for one emission lane of this execution —
    /// keyed under the dispatch's per-call key when one is installed
    /// ([`ToolDispatchContext::observation_keyed`]), else the enclosing
    /// cell's invocation, else the dispatch's own base (ADR 0105 §1).
    ///
    /// [`ToolDispatchContext::observation_keyed`]: crate::tool_dispatch::ToolDispatchContext::observation_keyed
    pub(crate) fn observation_cursor(&self, lane: &str) -> crate::engine::ObservationCursor {
        if self.dispatch.observation_call_key.is_none()
            && let Some(key) = self
                .parent_invocation
                .as_ref()
                .and_then(crate::RuntimeInvocation::effect_replay_key)
        {
            return crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new(format!(
                "{key}:{lane}"
            )));
        }
        self.dispatch.observation_cursor(lane)
    }

    /// `material` qualified under the base this context's lanes resolve — its
    /// own invocation when it carries one, else the dispatch's. The protocol
    /// path's `{iteration}:{index}:{call_id}` material is what a caller
    /// passes; the returned string is the call's fully-qualified observation
    /// key (ADR 0105 §1).
    pub(crate) fn call_observation_key(&self, material: &str) -> String {
        let base = self
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::effect_replay_key)
            .map(str::to_owned)
            .unwrap_or_else(|| self.dispatch.observation_base_key());
        format!("{base}:call:{material}")
    }

    /// This context with one call's resolved observation key installed on its
    /// dispatch — [`Self::call_observation_key`] output or the call's own
    /// effect-invocation replay key. The clone shares every buffer and
    /// service; only emissions re-key.
    pub(crate) fn with_call_observation_key(&self, key: impl Into<String>) -> Self {
        let mut context = self.clone();
        let mut dispatch = (*context.dispatch).clone();
        dispatch.observation_call_key = Some(key.into());
        context.dispatch = std::sync::Arc::new(dispatch);
        context
    }
}

impl RuntimeExecutionContext<'_> {
    pub(crate) fn tool_observation_attribution(&self) -> ToolObservationAttribution {
        ToolObservationAttribution {
            context: self
                .tracing
                .as_ref()
                .map(|tracing| tracing.scope_context.clone()),
            graph_key: self.code_block_graph_key.clone(),
            issuing_node_id: self.issuing_language_node_id.as_deref().map(str::to_owned),
        }
    }

    pub(crate) fn with_tool_observation_attribution(
        &self,
        attribution: &ToolObservationAttribution,
    ) -> Self {
        let mut context = self.clone();
        if let (Some(tracing), Some(identity)) = (&mut context.tracing, &attribution.context) {
            tracing.scope_context = identity.clone();
        }
        context.code_block_graph_key = attribution.graph_key.clone();
        context.issuing_language_node_id = attribution.issuing_node_id.clone().map(Arc::from);
        context
    }

    pub(crate) fn replay_validation_trace(&self) -> Option<crate::RuntimeEffectReplayTrace> {
        let tracing = self.tracing.as_ref()?;
        crate::RuntimeEffectReplayTrace::for_divergence(
            &tracing.runtime,
            tracing.scope.clone(),
            tracing.scope_context.clone(),
        )
    }

    pub(crate) fn recorded_tool_observation(
        &self,
        mut event: lash_trace::TraceEvent,
    ) -> Result<Option<serde_json::Value>, String> {
        let Some(tracing) = self
            .tracing
            .as_ref()
            .filter(|tracing| tracing.runtime.is_observed())
        else {
            return Ok(None);
        };
        match &mut event {
            lash_trace::TraceEvent::ToolCallStarted {
                issuing_node_id, ..
            }
            | lash_trace::TraceEvent::ToolCallCompleted {
                issuing_node_id, ..
            } => {
                *issuing_node_id = self.issuing_language_node_id.as_deref().map(str::to_owned);
            }
            _ => {}
        }
        serde_json::to_value((tracing.scope_context.clone(), event))
            .map(Some)
            .map_err(|error| error.to_string())
    }

    /// The runtime's shared trace handle, when this execution was given one.
    pub fn trace_runtime(&self) -> Option<&crate::trace::TraceRuntime> {
        self.tracing.as_ref().map(RuntimeExecutionTracing::runtime)
    }

    /// The scope this execution runs under.
    pub fn trace_scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        self.tracing
            .as_ref()
            .and_then(RuntimeExecutionTracing::scope)
    }

    /// Where code running in this execution stands when it observes: in the
    /// live step of the recorded body the execution runs in, or else with the
    /// shift that issues this execution's steps. The handle is cloneable and
    /// may move into what the execution spawns; it carries the scope, the
    /// substrate attempt and the right to emit.
    pub fn trace_standing(&self) -> Option<crate::trace::TraceStanding> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(standing) = &self.fixture_standing {
            return Some(standing.clone());
        }
        let tracing = self.tracing.as_ref()?;
        Some(match &self.live_step {
            Some(live) => tracing.runtime.body(tracing.scope.clone(), live),
            None => tracing.coordination(&self.dispatch.effect_controller),
        })
    }

    /// Places this context at `standing` for a fixture: the right to emit is
    /// the standing's, whatever controller the context issues steps through.
    /// A context given no tracing handle takes the standing's runtime and
    /// scope.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_trace_standing(mut self, standing: crate::trace::TraceStanding) -> Self {
        if self.tracing.is_none() {
            self.tracing = Some(RuntimeExecutionTracing::new(
                standing.runtime().clone(),
                standing.scope().cloned(),
                lash_trace::TraceContext::default(),
            ));
        }
        self.fixture_standing = Some(standing);
        self
    }

    /// Where the coordination of this execution's tool calls stands: with the
    /// shift that issues their steps.
    pub(in crate::session) fn coordination_standing(
        &self,
        tracing: &RuntimeExecutionTracing,
    ) -> crate::trace::TraceStanding {
        #[cfg(any(test, feature = "testing"))]
        if let Some(standing) = &self.fixture_standing {
            return standing.clone();
        }
        tracing.coordination(&self.dispatch.effect_controller)
    }

    pub fn with_code_block_graph_key(mut self, graph_key: Option<String>) -> Self {
        self.code_block_graph_key = graph_key;
        self
    }

    pub fn with_issuing_language_node_id(mut self, node_id: impl Into<String>) -> Self {
        self.issuing_language_node_id = Some(Arc::from(node_id.into()));
        self
    }

    /// Graph key of the enclosing code block for tool calls run from this
    /// context, or `None` when no code block is executing.
    pub(in crate::session) fn code_block_graph_key(&self) -> Option<String> {
        self.code_block_graph_key.clone()
    }
}
