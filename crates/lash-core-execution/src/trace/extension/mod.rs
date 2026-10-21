//! The extension tracing capability (§7): how tool providers, hooks,
//! protocol and code-executor plugins, process-step bodies and plugin
//! operations stand in the trace.
//!
//! Lash binds every [`ExtensionTrace`] privately from the registered
//! callback it is about to run, positions it at the anchored container its
//! slot names ([`TraceScopeOf`](lash_vocabulary::providers::TraceScopeOf)),
//! and revokes it when that body exits, however it exits. A plugin cannot
//! choose a parent, open a root, set a span id or emit in another plugin's
//! name.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use lash_protocol_types::sync::MutexExt as _;
use lash_trace::{
    ContentPolicy, PublicationHold, SpanSpec, ToolSpanAnnotations, TraceCarrier, TraceContext,
    TraceOmissionDiagnostic, TraceParent, TraceSpanKind, WriterRemaining,
};
use lash_vocabulary::providers::CallbackSlot;
use lash_vocabulary::providers::PluginCallbackIdentity;

use super::TraceStanding;

mod body;
mod instrument;
mod span;
mod tool_span;

#[cfg(test)]
mod tests;

use body::BodyTraceState;
pub(crate) use body::{BodyTrace, HOLD_ENTRY_BYTES, HoldGate};
pub use instrument::TraceInstrumented;
pub use span::ExtensionSpan;
pub use tool_span::ToolSpanAttributes;

/// The registered callback a capability is bound from: its identity and
/// the slot its key names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundCallback {
    pub(crate) identity: PluginCallbackIdentity,
    pub(crate) slot: CallbackSlot,
}

impl BoundCallback {
    /// The binding of `identity`, when its key names a slot.
    pub(crate) fn of(identity: &PluginCallbackIdentity) -> Option<Self> {
        CallbackSlot::of(identity).map(|slot| Self {
            identity: identity.clone(),
            slot,
        })
    }
}

/// What lash binds one body execution's capability from.
pub(crate) struct BodySpec {
    /// The standing at the anchored container the slot names.
    pub(crate) standing: TraceStanding,
    /// What work lash admits for the body takes as its parent.
    pub(crate) anchor: Option<TraceParent>,
    /// The context of the body's records.
    pub(crate) context: TraceContext,
    /// The callback, when the wrapper knows it; else the router binds it.
    pub(crate) callback: Option<BoundCallback>,
    /// The admitted attempt a tool or process-step body runs for.
    pub(crate) call: Option<crate::CallAddress>,
    /// The cell the body runs in while the turn holds its model response,
    /// with the cell's graph key.
    pub(crate) cell: Option<(crate::HeldResponse, String)>,
    /// Where a tool body's annotations of its own tool span are kept for
    /// the span's closer.
    pub(crate) tool_span: Option<Arc<Mutex<ToolSpanAnnotations>>>,
}

impl BodySpec {
    /// A body standing at `standing`, with no callback, call or cell yet:
    /// anchored at its scope's span.
    pub(crate) fn at(standing: TraceStanding, context: TraceContext) -> Self {
        Self {
            anchor: standing
                .scope()
                .and_then(lash_trace::DurableTraceScope::as_parent),
            standing,
            context,
            callback: None,
            call: None,
            cell: None,
            tool_span: None,
        }
    }

    /// Binds the capability and its guard: the guard goes to the wrapper
    /// that runs the body, the capability to the body.
    pub(crate) fn bind(self) -> (ExtensionTrace, BodyTrace) {
        let state = BodyTraceState::new(self);
        (
            ExtensionTrace {
                inner: Some(Capability {
                    body: Arc::clone(&state),
                    at: None,
                }),
            },
            BodyTrace::new(Some(state)),
        )
    }
}

/// Where extension code stands in the trace, and its right to emit there.
///
/// Only lash constructs one, bound to the registered callback that runs and
/// revoked when that callback's body exits. Clones share one body budget.
/// Every method is total: a capability that does not record hands out
/// no-op spans and costs nothing.
#[derive(Clone, Default)]
pub struct ExtensionTrace {
    inner: Option<Capability>,
}

#[derive(Clone)]
struct Capability {
    body: Arc<BodyTraceState>,
    /// The open extension span this capability is positioned at; `None` at
    /// the body's anchored container.
    at: Option<SpanAt>,
}

#[derive(Clone)]
struct SpanAt {
    node: body::NodeId,
    span: TraceCarrier,
    standing: TraceStanding,
}

impl std::fmt::Debug for ExtensionTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionTrace")
            .field("recording", &self.is_recording())
            .field("position", &self.position())
            .finish()
    }
}

impl ExtensionTrace {
    /// A capability that never records, for code outside any traced body.
    pub fn untraced() -> Self {
        Self { inner: None }
    }

    /// Whether anything observes this position and the body may still
    /// emit. Test it before preparing expensive attributes.
    pub fn is_recording(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|cap| cap.body.records(TraceSpanKind::Extension))
    }

    /// The W3C context of this position, for outbound propagation (an MCP
    /// `_meta`, an HTTP header, a queue message): equal to the context the
    /// exporter gives that span. `None` when untraced.
    pub fn position(&self) -> Option<TraceCarrier> {
        let cap = self.inner.as_ref()?;
        match &cap.at {
            Some(at) => Some(at.span.clone()),
            None => cap.body.standing().container().cloned(),
        }
    }

    /// The parent work lash admits on the plugin's behalf takes: the
    /// capability's anchored container (the tool span of a tool or
    /// process-step body, the turn of a turn hook, an anchored cell or its
    /// turn for a code executor, the producer of an operation). Never an
    /// extension span, whichever span this capability was taken from.
    pub fn as_parent(&self) -> Option<TraceParent> {
        self.inner.as_ref()?.body.anchor().cloned()
    }

    /// Opens an `Internal` child span. Never fails: a no-op handle when not
    /// recording.
    pub fn span(&self, name: impl Into<Cow<'static, str>>) -> ExtensionSpan {
        self.span_with(SpanSpec::internal(name))
    }

    /// Opens a child span as `spec` describes it.
    pub fn span_with(&self, spec: SpanSpec) -> ExtensionSpan {
        ExtensionSpan::open(self, spec)
    }

    /// Records a zero-duration point span named `name` under this position,
    /// its attributes written by `attrs`.
    pub fn event(
        &self,
        name: impl Into<Cow<'static, str>>,
        attrs: impl FnOnce(&mut lash_trace::SpanAttributes),
    ) {
        let Some(cap) = &self.inner else {
            return;
        };
        if !cap.body.records(TraceSpanKind::Point) {
            return;
        }
        let Some(parent_span) = self.position() else {
            return;
        };
        cap.body.point(
            cap.at.as_ref().map(|at| at.node),
            &parent_span,
            name.into(),
            attrs,
        );
    }

    /// Runs `fut` with this position as the ambient context on every poll:
    /// third-party OpenTelemetry and `tracing` instrumentation inside it
    /// nests here, and so does a model call it makes. Use it for nested
    /// work and for every task the body spawns.
    pub fn instrument<F: std::future::Future>(&self, fut: F) -> TraceInstrumented<F> {
        TraceInstrumented::new(fut, self.frame())
    }

    /// Runs `f` synchronously with this position as the ambient context.
    pub fn in_scope<R>(&self, f: impl FnOnce() -> R) -> R {
        match self.frame() {
            Some(frame) => {
                let _entered = frame.enter();
                f()
            }
            None => f(),
        }
    }

    /// The effective content policy at this position: the core ceiling
    /// after the capture selector, for code that builds its own content.
    pub fn content_policy(&self) -> ContentPolicy {
        self.inner
            .as_ref()
            .map_or_else(ContentPolicy::default, |cap| cap.standing().content())
    }

    /// The tool-live address of the admitted attempt this body runs for
    /// (tool provider and process-step bodies). The span's
    /// `gen_ai.tool.call.id` stays the logical call id; this names the
    /// attempt.
    pub fn call_address(&self) -> Option<&crate::CallAddress> {
        self.inner.as_ref()?.body.call()
    }

    /// What is left of this body's budget: the extension and point spans it
    /// may still open, and the bytes its contributions may still charge.
    pub fn remaining(&self) -> WriterRemaining {
        self.inner.as_ref().map_or(
            WriterRemaining {
                attributes: 0,
                bytes: 0,
            },
            |cap| cap.body.remaining(),
        )
    }

    /// This body's omissions so far, with its address. At exit the same
    /// diagnostic is emitted as a `body_omissions` record when it is not
    /// empty.
    pub fn omissions(&self) -> TraceOmissionDiagnostic {
        self.inner.as_ref().map_or_else(
            || TraceOmissionDiagnostic {
                site: lash_trace::TraceOmissionSite::Body {
                    owner: String::new(),
                    key: String::new(),
                    at: None,
                    call: None,
                    attempt: None,
                },
                counts: lash_trace::TraceOmissionCounts::default(),
            },
            |cap| cap.body.diagnostic(),
        )
    }

    /// The position a host hands the callbacks of one slot run: `standing`
    /// at the slot's anchored container, observed under `context`. It
    /// records nothing itself; lash binds a capability from it for each
    /// registered callback it runs ([`run_bound`]).
    #[doc(hidden)]
    pub fn seed(standing: TraceStanding, context: TraceContext) -> Self {
        Self::seed_from(BodySpec::at(standing, context))
    }

    /// The position of the operations a host call admits: its producer span
    /// `parent`, from a host call that is never replayed.
    #[doc(hidden)]
    pub fn at_producer(
        runtime: &super::TraceRuntime,
        parent: TraceParent,
        context: TraceContext,
    ) -> Self {
        let standing = runtime
            .unreplayed(None)
            .within(parent.carrier().clone(), TraceSpanKind::Producer);
        let mut spec = BodySpec::at(standing, context);
        spec.anchor = Some(parent);
        Self::seed_from(spec)
    }

    /// The seed `spec` describes, its callback still unbound.
    pub(crate) fn seed_from(spec: BodySpec) -> Self {
        Self {
            inner: Some(Capability {
                body: BodyTraceState::new(spec),
                at: None,
            }),
        }
    }

    /// The capability of one run of the registered callback `identity` at
    /// this seed's position, and its guard. Untraced when the seed is, or
    /// when the callback's slot is untraced.
    pub(crate) fn for_callback(&self, identity: &PluginCallbackIdentity) -> (Self, BodyTrace) {
        let callback = BoundCallback::of(identity).filter(|bound| {
            bound.slot.trace_scope() != lash_vocabulary::providers::TraceScopeOf::Untraced
        });
        let (Some(cap), Some(callback)) = (&self.inner, callback) else {
            return (Self::untraced(), BodyTrace::untraced());
        };
        let seed = &cap.body;
        let mut spec = BodySpec::at(seed.standing().clone(), seed.context().clone());
        spec.anchor = seed.anchor().cloned();
        spec.cell = seed.cell().cloned();
        spec.callback = Some(callback);
        spec.bind()
    }

    /// The standing lash's own leaves opened from this position stand at: a
    /// model call made from the body is the child of this position.
    pub(crate) fn standing(&self) -> Option<TraceStanding> {
        self.inner.as_ref().map(|cap| cap.standing().clone())
    }

    /// Binds the registered callback the body runs, when its wrapper did
    /// not know it (a tool provider, which its router resolves).
    pub(crate) fn bind(&self, callback: BoundCallback) {
        if let Some(cap) = &self.inner {
            cap.body.bind(callback);
        }
    }

    /// The W3C context of the body's own tool span, wherever this
    /// capability is positioned.
    pub(crate) fn tool_span_context(&self) -> Option<TraceCarrier> {
        self.inner.as_ref()?.body.tool_span_context()
    }

    /// The annotations writer of the body's own tool span.
    pub(crate) fn tool_span(&self) -> ToolSpanAttributes {
        ToolSpanAttributes::new(self.inner.as_ref().map(|cap| Arc::clone(&cap.body)))
    }

    /// This capability positioned at the open span `node`.
    fn at_span(&self, node: body::NodeId, span: TraceCarrier, started_at_ms: u64) -> Self {
        let Some(cap) = &self.inner else {
            return Self::untraced();
        };
        let parent = self.position().unwrap_or_else(|| span.clone());
        let standing = cap.standing().in_extension(
            span.clone(),
            parent,
            started_at_ms,
            HoldGate::new(Arc::clone(&cap.body), node),
        );
        Self {
            inner: Some(Capability {
                body: Arc::clone(&cap.body),
                at: Some(SpanAt {
                    node,
                    span,
                    standing,
                }),
            }),
        }
    }

    fn frame(&self) -> Option<Arc<lash_trace::AmbientFrame>> {
        let position = self.position()?;
        let scope = self
            .inner
            .as_ref()
            .and_then(|cap| cap.body.standing().runtime().ambient().cloned())
            .map(|ambient| ambient.for_position(&position));
        Some(lash_trace::AmbientFrame::new(scope, Arc::new(self.clone())))
    }
}

impl Capability {
    fn standing(&self) -> &TraceStanding {
        match &self.at {
            Some(at) => &at.standing,
            None => self.body.standing(),
        }
    }
}

/// Runs one registered callback `identity` at `seed`'s position: `invoke`
/// gets the bound capability, the callback polls under it, and its exit,
/// however it exits, revokes it.
pub(crate) async fn run_bound<F: std::future::Future>(
    seed: &ExtensionTrace,
    identity: &PluginCallbackIdentity,
    invoke: impl FnOnce(ExtensionTrace) -> F,
) -> F::Output {
    let (trace, body) = seed.for_callback(identity);
    body.guard(trace.instrument(invoke(trace.clone()))).await
}

/// The standing a leaf lash opens now, from code polled under an extension
/// position of `runtime` (inside `instrument`, or a task spawned from it),
/// takes: that position, with its narrowing even where the capability
/// records nothing, so a skipped body's model calls are skipped too.
/// `None` outside one, or under an untraced capability.
pub(crate) fn ambient_standing(runtime: &super::TraceRuntime) -> Option<TraceStanding> {
    lash_trace::AmbientFrame::current_local::<ExtensionTrace>()?
        .standing()
        .filter(|standing| standing.runtime().same(runtime))
}

/// The records and bytes one execution's holds keep: every hold of one
/// body execution, or one unanchored cell's (§3.8 P5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HoldAccount {
    records: u32,
    bytes: u64,
}

/// One execution's hold account, bounded by `trace_limits.publication_hold`.
#[derive(Clone)]
pub(crate) struct SharedHold {
    cap: PublicationHold,
    account: Arc<Mutex<HoldAccount>>,
}

impl SharedHold {
    pub(crate) fn new(cap: PublicationHold) -> Self {
        Self {
            cap,
            account: Arc::default(),
        }
    }

    /// Charges one held record of `cost` bytes, unless it would pass either
    /// cap.
    pub(crate) fn admit(&self, cost: u64) -> bool {
        let mut account = self.account.lock_recover();
        if account.records.saturating_add(1) > self.cap.records()
            || account.bytes.saturating_add(cost) > self.cap.bytes()
        {
            return false;
        }
        account.records += 1;
        account.bytes += cost;
        true
    }

    /// Charges one held record whatever the caps say: a lash closing record
    /// no strategy may drop.
    pub(crate) fn force(&self, cost: u64) {
        let mut account = self.account.lock_recover();
        account.records = account.records.saturating_add(1);
        account.bytes = account.bytes.saturating_add(cost);
    }

    /// Returns what `records` held records charged.
    pub(crate) fn refund(&self, records: usize, bytes: u64) {
        let mut account = self.account.lock_recover();
        account.records = account
            .records
            .saturating_sub(u32::try_from(records).unwrap_or(u32::MAX));
        account.bytes = account.bytes.saturating_sub(bytes);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> HoldAccount {
        *self.account.lock_recover()
    }
}
