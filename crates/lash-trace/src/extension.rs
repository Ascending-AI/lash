//! What extension code contributes to the trace: the one record of each
//! span it opened, the annotations a tool body sets on its own tool span,
//! and the ambient context its polls run under.
//!
//! Lash builds every value here from a capability bound to the registered
//! callback that ran (lash-core-execution's `ExtensionTrace`); nothing in
//! this module lets a caller choose a parent, an id or a plugin identity.

use std::borrow::Cow;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{TraceCarrier, TraceValue};

/// The OpenTelemetry kind of an extension span.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSpanKind {
    #[default]
    Internal,
    Client,
    Producer,
    Consumer,
}

/// The GenAI operation a typed [`SpanSpec`] preset declares. Lash sets its
/// semconv attributes; a plugin cannot set them through `attr`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionOperation {
    /// `embeddings {gen_ai.request.model}`.
    Embeddings {
        provider: String,
        request_model: String,
    },
    /// `retrieval {gen_ai.data_source.id}`.
    Retrieval { data_source_id: String },
}

impl ExtensionOperation {
    /// The `gen_ai.operation.name` the preset sets.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Embeddings { .. } => "embeddings",
            Self::Retrieval { .. } => "retrieval",
        }
    }
}

/// What an extension span is opened as: its name, its kind and its GenAI
/// operation preset, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanSpec {
    name: Cow<'static, str>,
    kind: ExtensionSpanKind,
    operation: Option<ExtensionOperation>,
}

impl SpanSpec {
    fn of(name: impl Into<Cow<'static, str>>, kind: ExtensionSpanKind) -> Self {
        Self {
            name: name.into(),
            kind,
            operation: None,
        }
    }

    /// An `Internal` span.
    pub fn internal(name: impl Into<Cow<'static, str>>) -> Self {
        Self::of(name, ExtensionSpanKind::Internal)
    }

    /// A `Client` span.
    pub fn client(name: impl Into<Cow<'static, str>>) -> Self {
        Self::of(name, ExtensionSpanKind::Client)
    }

    /// A `Producer` span.
    pub fn producer(name: impl Into<Cow<'static, str>>) -> Self {
        Self::of(name, ExtensionSpanKind::Producer)
    }

    /// A `Consumer` span.
    pub fn consumer(name: impl Into<Cow<'static, str>>) -> Self {
        Self::of(name, ExtensionSpanKind::Consumer)
    }

    /// `embeddings {request_model}`, `Client`: sets `gen_ai.operation.name`,
    /// `gen_ai.provider.name` and `gen_ai.request.model`.
    pub fn embeddings(provider: &str, request_model: &str) -> Self {
        Self {
            name: Cow::Owned(format!("embeddings {request_model}")),
            kind: ExtensionSpanKind::Client,
            operation: Some(ExtensionOperation::Embeddings {
                provider: provider.to_owned(),
                request_model: request_model.to_owned(),
            }),
        }
    }

    /// `retrieval {data_source_id}`, `Client`: sets `gen_ai.operation.name`
    /// and `gen_ai.data_source.id`.
    pub fn retrieval(data_source_id: &str) -> Self {
        Self {
            name: Cow::Owned(format!("retrieval {data_source_id}")),
            kind: ExtensionSpanKind::Client,
            operation: Some(ExtensionOperation::Retrieval {
                data_source_id: data_source_id.to_owned(),
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> ExtensionSpanKind {
        self.kind
    }

    pub fn operation(&self) -> Option<&ExtensionOperation> {
        self.operation.as_ref()
    }

    /// The parts of the spec, for lash's span builder.
    #[doc(hidden)]
    pub fn into_parts(
        self,
    ) -> (
        Cow<'static, str>,
        ExtensionSpanKind,
        Option<ExtensionOperation>,
    ) {
        (self.name, self.kind, self.operation)
    }
}

/// Why an extension span ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSpanEnd {
    /// The plugin called `end`.
    End,
    /// The handle was dropped.
    Drop,
    /// The body returned with the span still open.
    BodyExit,
    /// The body future was dropped with the span still open.
    Cancelled,
    /// The body unwound with the span still open.
    Panic,
    /// The body's publication hold was full: lash finalized the span early so
    /// the records held under it could publish.
    HoldPressure,
}

impl ExtensionSpanEnd {
    /// The `lash.span.ended_by` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::End => "end",
            Self::Drop => "drop",
            Self::BodyExit => "body_exit",
            Self::Cancelled => "cancelled",
            Self::Panic => "panic",
            Self::HoldPressure => "hold_pressure",
        }
    }
}

/// One contributed attribute, as the bounded writer admitted it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtensionAttribute {
    pub key: String,
    pub value: TraceValue,
}

/// One event an extension recorded on its span.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSpanEvent {
    pub name: String,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<ExtensionAttribute>,
}

/// One content value an extension built under the `extension` category.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtensionContent {
    pub key: String,
    pub value: serde_json::Value,
}

/// The one record of an extension span (or point): emitted when it ends,
/// whole, with everything it buffered.
///
/// `plugin_id` and `callback` are the registered callback the span's
/// capability was bound from, never a value the plugin supplied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExtensionSpanRecord {
    pub plugin_id: String,
    pub callback: String,
    pub name: String,
    pub kind: ExtensionSpanKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<ExtensionOperation>,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    /// A zero-duration point recorded with no open span.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub point: bool,
    /// Set when the plugin called `fail`: the span's `error.type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// `None` for a point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_by: Option<ExtensionSpanEnd>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<ExtensionAttribute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ExtensionSpanEvent>,
    /// Serialized as `contents`: the record envelope's `content` is its
    /// content marks, and the record flattens this one beside them.
    #[serde(rename = "contents", default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ExtensionContent>,
}

/// The attributes a tool body set on its own `execute_tool` span, through
/// its bounded writer. Held in memory with the attempt that set them and
/// stamped by the span's closer; never persisted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct ToolSpanAnnotations(pub Vec<ExtensionAttribute>);

impl ToolSpanAnnotations {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The annotated value of `key`, if the body set one.
    pub fn get(&self, key: &str) -> Option<&TraceValue> {
        self.0
            .iter()
            .find(|attribute| attribute.key == key)
            .map(|attribute| &attribute.value)
    }
}

/// Whether `key` is one a tool body may set on its own tool span: the MCP,
/// JSON-RPC, RPC, network and server namespaces, and the refinable tool
/// defaults `gen_ai.tool.type` and `error.type`.
pub fn tool_span_key_allowed(key: &str) -> bool {
    const PREFIXES: [&str; 5] = ["mcp.", "jsonrpc.", "rpc.", "network.", "server."];
    PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        || matches!(key, "gen_ai.tool.type" | "error.type")
}

/// The context a position's polls run under: installed by the host's
/// exporter, so third-party OpenTelemetry and `tracing` instrumentation
/// inside an extension body nests under the extension's position.
pub trait AmbientContext: Send + Sync + 'static {
    /// The scope that enters `position` as the current context.
    fn for_position(&self, position: &TraceCarrier) -> Arc<dyn AmbientScope>;
}

/// One position's ambient scope, entered for the length of one poll.
pub trait AmbientScope: Send + Sync {
    /// Enters the scope; the guard leaves it when dropped. It is never held
    /// across an `.await`.
    fn enter(&self) -> AmbientGuard;
}

/// Leaves an entered [`AmbientScope`] when dropped.
#[must_use = "the scope is left when the guard is dropped"]
pub struct AmbientGuard(Option<Box<dyn std::any::Any>>);

impl AmbientGuard {
    /// A guard that holds `inner` (the host's own guard) until it drops.
    pub fn new(inner: impl std::any::Any) -> Self {
        Self(Some(Box::new(inner)))
    }

    /// A guard that entered nothing.
    pub fn none() -> Self {
        Self(None)
    }
}

impl std::fmt::Debug for AmbientGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmbientGuard")
            .field("entered", &self.0.is_some())
            .finish()
    }
}

/// The position a poll runs under: the host's ambient scope for it (an
/// OpenTelemetry context), and lash's own frame, which lash code inside the
/// poll reads (a model call made from an extension body takes its parent
/// from it). Entered by [`Framed`] for each poll, never across an `.await`.
pub struct AmbientFrame {
    scope: Option<Arc<dyn AmbientScope>>,
    local: Arc<dyn std::any::Any + Send + Sync>,
}

thread_local! {
    static CURRENT_FRAME: std::cell::RefCell<Option<Arc<AmbientFrame>>> =
        const { std::cell::RefCell::new(None) };
}

impl AmbientFrame {
    /// A frame whose lash part is `local` and whose host part is `scope`.
    pub fn new(
        scope: Option<Arc<dyn AmbientScope>>,
        local: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self { scope, local })
    }

    /// The frame the current poll runs under, if any.
    pub fn current() -> Option<Arc<Self>> {
        CURRENT_FRAME.with(|current| current.borrow().clone())
    }

    /// The lash part of the current frame, when it is a `T`.
    pub fn current_local<T: Send + Sync + 'static>() -> Option<Arc<T>> {
        Self::current().and_then(|frame| Arc::clone(&frame.local).downcast::<T>().ok())
    }

    /// Enters this frame on the current thread until the guard drops: lash's
    /// part, then the host's scope.
    pub fn enter(self: &Arc<Self>) -> FrameGuard {
        let previous = CURRENT_FRAME.with(|current| current.replace(Some(Arc::clone(self))));
        FrameGuard {
            host: self.scope.as_ref().map(|scope| scope.enter()),
            previous: Some(previous),
        }
    }
}

impl std::fmt::Debug for AmbientFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmbientFrame")
            .field("host_scope", &self.scope.is_some())
            .finish_non_exhaustive()
    }
}

/// Leaves an entered [`AmbientFrame`]: the host's scope first, then lash's
/// frame, restoring the one entered before it.
#[must_use = "the frame is left when the guard is dropped"]
pub struct FrameGuard {
    host: Option<AmbientGuard>,
    previous: Option<Option<Arc<AmbientFrame>>>,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        drop(self.host.take());
        if let Some(previous) = self.previous.take() {
            CURRENT_FRAME.with(|current| *current.borrow_mut() = previous);
        }
    }
}

/// Polls a future with a frame entered for each poll. The innermost wrapper
/// of a task: entered after any `tracing` span an outer adapter entered, so
/// the host's current context during the poll is the frame's position.
#[must_use = "futures do nothing unless polled"]
pub struct Framed<F> {
    inner: std::pin::Pin<Box<F>>,
    frame: Arc<AmbientFrame>,
}

impl<F: std::future::Future> Framed<F> {
    pub fn new(inner: F, frame: Arc<AmbientFrame>) -> Self {
        Self {
            inner: Box::pin(inner),
            frame,
        }
    }
}

impl<F: std::future::Future> std::future::Future for Framed<F> {
    type Output = F::Output;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let _entered = self.frame.enter();
        self.inner.as_mut().poll(cx)
    }
}

impl<F> std::fmt::Debug for Framed<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Framed")
            .field("frame", &self.frame)
            .finish()
    }
}

/// The attributes of one extension event or point span, written through
/// the bounded writer under the event's own cap: what it refuses is
/// counted, never raised.
pub struct SpanAttributes {
    writer: crate::TraceAttributeWriter,
}

impl SpanAttributes {
    #[doc(hidden)]
    pub fn new(limits: &crate::TraceLimits) -> Self {
        Self {
            writer: crate::TraceAttributeWriter::new(crate::TraceSpanKind::Point, limits),
        }
    }

    /// Sets one attribute. Plain attributes are not content.
    pub fn set(
        &mut self,
        key: impl Into<crate::AttrKey>,
        value: impl Into<crate::TraceValue>,
    ) -> &mut Self {
        let _ = self.writer.set(key, value);
        self
    }

    /// What is left of the event's budget.
    pub fn remaining(&self) -> crate::WriterRemaining {
        self.writer.remaining()
    }

    #[doc(hidden)]
    pub fn finish(self) -> (Vec<ExtensionAttribute>, crate::TraceOmissionCounts) {
        let (contributions, omissions) = self.writer.finish();
        (
            contributions
                .added
                .into_iter()
                .map(|(key, value)| ExtensionAttribute {
                    key: key.as_str().to_owned(),
                    value,
                })
                .collect(),
            omissions,
        )
    }
}

impl std::fmt::Debug for SpanAttributes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanAttributes")
            .field("remaining", &self.writer.remaining())
            .finish()
    }
}
