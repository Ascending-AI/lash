//! An extension span handle (§7.3).

use std::borrow::Cow;
use std::sync::Arc;

use lash_trace::{
    AttrKey, ContentCategory, ExtensionSpanEnd, SpanSpec, TraceCarrier, TraceOmissionCounts,
    TraceSpanKind, TraceValue,
};

use super::ExtensionTrace;
use super::body::{BodyTraceState, NodeId};

/// An extension span the plugin opened. It ends when [`end`](Self::end) is
/// called or when it is dropped; every mutation is charged to the body's
/// budget first, and a refusal is counted, never raised.
///
/// A span that is not recording is a no-op handle. It keeps the capability
/// it was opened from, so nothing opened through it is built where its
/// kind is off, and a child, point or model call opened through a span the
/// budget refused stands at that capability's position: the nearest span
/// lash minted.
#[must_use = "a span ends when it is dropped"]
pub struct ExtensionSpan {
    inner: SpanInner,
}

enum SpanInner {
    /// Not minted: the capability it was opened from.
    Noop(Box<ExtensionTrace>),
    Open(Box<OpenHandle>),
}

struct OpenHandle {
    body: Arc<BodyTraceState>,
    node: NodeId,
    span: TraceCarrier,
    /// The capability positioned under this span.
    trace: ExtensionTrace,
    omissions: TraceOmissionCounts,
    ended: bool,
}

impl ExtensionSpan {
    pub(super) fn open(from: &ExtensionTrace, spec: SpanSpec) -> Self {
        let noop = || Self {
            inner: SpanInner::Noop(Box::new(from.clone())),
        };
        let Some(cap) = &from.inner else {
            return noop();
        };
        if !cap.body.records(TraceSpanKind::Extension) {
            return noop();
        }
        let Some(parent_span) = from.position() else {
            return noop();
        };
        match cap
            .body
            .open(cap.at.as_ref().map(|at| at.node), &parent_span, spec)
        {
            Ok((node, span, started_at_ms)) => Self {
                inner: SpanInner::Open(Box::new(OpenHandle {
                    body: Arc::clone(&cap.body),
                    node,
                    trace: from.at_span(node, span.clone(), started_at_ms),
                    span,
                    omissions: TraceOmissionCounts::default(),
                    ended: false,
                })),
            },
            Err(_) => noop(),
        }
    }

    /// Whether the span was minted and the body may still emit.
    pub fn is_recording(&self) -> bool {
        match &self.inner {
            SpanInner::Open(open) => open.trace.is_recording(),
            SpanInner::Noop(_) => false,
        }
    }

    /// The W3C context of this span, for outbound propagation.
    pub fn position(&self) -> Option<TraceCarrier> {
        match &self.inner {
            SpanInner::Open(open) => Some(open.span.clone()),
            SpanInner::Noop(_) => None,
        }
    }

    /// Sets one attribute. Plain attributes are not content: keep content
    /// out of them and use [`content`](Self::content).
    pub fn attr(&mut self, key: impl Into<AttrKey>, value: impl Into<TraceValue>) -> &mut Self {
        if let SpanInner::Open(open) = &mut self.inner
            && let Err(omission) = open.body.attr(open.node, key.into(), value.into())
        {
            open.omissions.add(omission);
        }
        self
    }

    /// Sets one content value, built only when the `extension` content
    /// category is captured; the host's content hook and the content limits
    /// apply to it like to lash's own content.
    pub fn content(
        &mut self,
        key: impl Into<AttrKey>,
        build: impl FnOnce() -> TraceValue,
    ) -> &mut Self {
        if let SpanInner::Open(open) = &mut self.inner
            && open
                .trace
                .content_policy()
                .captures(ContentCategory::Extension)
            && let Err(omission) = open.body.content(open.node, key.into(), build().to_json())
        {
            open.omissions.add(omission);
        }
        self
    }

    /// Records an event at the current time, its attributes written by
    /// `attrs`.
    pub fn event(
        &mut self,
        name: impl Into<Cow<'static, str>>,
        attrs: impl FnOnce(&mut lash_trace::SpanAttributes),
    ) -> &mut Self {
        if let SpanInner::Open(open) = &mut self.inner
            && let Err(omission) = open.body.event(open.node, name.into().into_owned(), attrs)
        {
            open.omissions.add(omission);
        }
        self
    }

    /// Marks the span failed: `Error` status with `error.type`.
    pub fn fail(&mut self, error_type: impl Into<Cow<'static, str>>) -> &mut Self {
        if let SpanInner::Open(open) = &mut self.inner
            && let Err(omission) = open.body.fail(open.node, error_type.into().into_owned())
        {
            open.omissions.add(omission);
        }
        self
    }

    /// Opens an `Internal` child span.
    pub fn child(&self, name: impl Into<Cow<'static, str>>) -> ExtensionSpan {
        self.trace().span(name)
    }

    /// Opens a child span as `spec` describes it.
    pub fn child_with(&self, spec: SpanSpec) -> ExtensionSpan {
        self.trace().span_with(spec)
    }

    /// A capability positioned under this span: to hand to nested code, or
    /// to `instrument` work that should nest under it.
    pub fn trace(&self) -> ExtensionTrace {
        match &self.inner {
            SpanInner::Open(open) => open.trace.clone(),
            SpanInner::Noop(from) => (**from).clone(),
        }
    }

    /// The refusals this span's methods counted.
    pub fn omissions(&self) -> TraceOmissionCounts {
        match &self.inner {
            SpanInner::Open(open) => open.omissions,
            SpanInner::Noop(_) => TraceOmissionCounts::default(),
        }
    }

    /// Ends the span now. An end after the body exited, or after hold
    /// pressure finalized the span, emits nothing and is counted late.
    pub fn end(mut self) {
        self.close(ExtensionSpanEnd::End);
    }

    fn close(&mut self, ended_by: ExtensionSpanEnd) {
        if let SpanInner::Open(open) = &mut self.inner
            && !open.ended
        {
            open.ended = true;
            open.body.end(open.node, ended_by);
        }
    }
}

impl Drop for ExtensionSpan {
    /// Ends the span now with `lash.span.ended_by = "drop"`, or `"panic"`
    /// when the handle is dropped by a panic unwinding the body.
    fn drop(&mut self) {
        self.close(if std::thread::panicking() {
            ExtensionSpanEnd::Panic
        } else {
            ExtensionSpanEnd::Drop
        });
    }
}

impl std::fmt::Debug for ExtensionSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionSpan")
            .field("recording", &self.is_recording())
            .field("position", &self.position())
            .finish()
    }
}
