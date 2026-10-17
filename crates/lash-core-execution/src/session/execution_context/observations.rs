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
/// `scope` is the scope the execution runs under, a turn's or a process's,
/// `container` the span inside that scope the execution's work is issued
/// from, when it is not the scope's own (a code cell's), and `scope_context`
/// carries the matching record identity (session / turn / iteration).
/// Coordination runs live from committed state, never against a replay
/// journal (ADR 0132 §2).
#[derive(Clone)]
pub struct RuntimeExecutionTracing {
    pub(super) runtime: crate::trace::TraceRuntime,
    pub(super) scope: Option<lash_trace::DurableTraceScope>,
    pub(super) container: Option<lash_trace::TraceParent>,
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
            container: None,
            scope_context,
        }
    }

    /// This execution issuing its work from the container `key` inside its
    /// scope's span: a code cell, whose tool calls are its children (§3.3).
    #[must_use]
    pub fn in_container(mut self, key: &lash_trace::SpanKey<'_>) -> Self {
        self.container = self
            .scope
            .as_ref()
            .and_then(lash_trace::DurableTraceScope::as_parent)
            .map(|parent| parent.container(key));
        self
    }

    /// The parent of the work this execution issues: its container, else
    /// its scope's span. `None` when the scope is unobserved.
    pub fn parent(&self) -> Option<lash_trace::TraceParent> {
        self.container.clone().or_else(|| {
            self.scope
                .as_ref()
                .and_then(lash_trace::DurableTraceScope::as_parent)
        })
    }

    /// The runtime's shared trace handle.
    pub fn runtime(&self) -> &crate::trace::TraceRuntime {
        &self.runtime
    }

    /// The scope the execution runs under.
    pub fn scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        self.scope.as_ref()
    }

    /// Coordination continues from committed state and is never replayed.
    /// Its observations therefore need no journal frontier (ADR 0132 §2).
    pub(crate) fn coordination(&self) -> crate::trace::TraceStanding {
        self.runtime.unreplayed(self.scope.clone())
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
    /// Retain the language call's passive graph identity for any process it
    /// declares. Replay reconstructs this correlation while re-executing code;
    /// it never participates in admission, keys, or the journal command stream.
    pub fn record_language_call_attribution(
        &self,
        call_id: crate::ToolCallId,
        language: impl Into<String>,
        identity: lash_trace::TraceLanguageExecutionIdentity,
        at: lash_protocol_types::EffectIdentity,
    ) {
        self.language_calls.lock_recover().insert(
            call_id,
            Arc::new(crate::runtime::process::LanguageCallAttribution {
                language: language.into(),
                identity,
                at,
            }),
        );
    }

    pub(crate) fn process_scope_for_language_call(
        &self,
        parent_invocation: crate::RuntimeInvocation,
        call_id: &crate::ToolCallId,
    ) -> crate::ProcessOpScope<'_> {
        let mut scope = self.process_scope(Some(parent_invocation));
        scope.language_call = self.language_calls.lock_recover().get(call_id).cloned();
        scope
    }

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
            None => tracing.coordination(),
        })
    }

    /// Where the protocol or code-executor body this context runs stands in
    /// the trace: the cell's span while a cell runs, else the turn's. Spans
    /// opened behind the journal frontier of a shift wait for it, and are
    /// dropped when the journal serves the step (§7.4 rule 5).
    pub fn trace(&self) -> crate::trace::ExtensionTrace {
        self.extension_trace.clone()
    }

    /// Runs `run` as the body of the registered `callback` (a code executor,
    /// or a protocol plugin) over this context: bound from the registration,
    /// positioned at the cell's span (else the turn's), polled under it, and
    /// revoked when `run` exits however it exits.
    #[doc(hidden)]
    pub async fn run_executing<F, Fut>(
        self,
        callback: Option<&crate::store::plugin_writers::PluginCallbackIdentity>,
        run: F,
    ) -> Fut::Output
    where
        F: FnOnce(Self) -> Fut,
        Fut: std::future::Future,
    {
        let seed = self.executing_seed();
        let (trace, body) = match callback {
            Some(callback) => seed.for_callback(callback),
            None => (
                crate::trace::ExtensionTrace::untraced(),
                crate::trace::BodyTrace::untraced(),
            ),
        };
        let mut context = self;
        context.extension_trace = trace.clone();
        body.guard(trace.instrument(run(context))).await
    }

    /// The position an executing body of this context binds from: its
    /// standing, at its container.
    fn executing_seed(&self) -> crate::trace::ExtensionTrace {
        let (Some(tracing), Some(standing)) = (&self.tracing, self.trace_standing()) else {
            return crate::trace::ExtensionTrace::untraced();
        };
        let Some(container) = &tracing.container else {
            return crate::trace::ExtensionTrace::seed(standing, tracing.scope_context.clone());
        };
        let standing =
            standing.within(container.carrier().clone(), lash_trace::TraceSpanKind::Cell);
        let mut spec = crate::trace::BodySpec::at(standing, tracing.scope_context.clone());
        spec.anchor = Some(container.clone());
        spec.cell = self
            .held_response()
            .cloned()
            .zip(self.code_block_graph_key());
        crate::trace::ExtensionTrace::seed_from(spec)
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

    /// The live standing of this execution's tool-call coordination.
    pub(in crate::session) fn coordination_standing(
        &self,
        tracing: &RuntimeExecutionTracing,
    ) -> crate::trace::TraceStanding {
        #[cfg(any(test, feature = "testing"))]
        if let Some(standing) = &self.fixture_standing {
            return standing.clone();
        }
        tracing.coordination()
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

impl RuntimeExecutionContext<'_> {
    pub fn with_tool_live(mut self, lane: Arc<crate::ToolLive>) -> Self {
        self.tool_live = lane;
        self
    }
}
