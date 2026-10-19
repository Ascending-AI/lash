//! Who mints span ids and makes the one sampling decision of a trace.
//!
//! A container span (a turn, a tool call, a code cell, a process, a tool
//! intent) takes [`derive_span`] of its parent and its durable identity, so
//! any owner computes it from retained data. A leaf (a model call) takes a
//! random id from [`TraceIdentity::leaf_span_id`] when it opens. A trace
//! root is made only by [`TraceIdentity::open_producer`], at a host call.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ProducerSpec, TraceCarrier, TraceLinkMode, TraceParent, TraceScopeId};
use super::{W3cSpanId, W3cTraceFlags, W3cTraceId, W3cTraceState};

/// The identity authority of a runtime: the caller's ambient context, the
/// producers that root and decide its traces, and the random ids of its
/// leaves. Engine code derives every container id itself.
pub trait TraceIdentity: Send + Sync + 'static {
    /// The caller's ambient context, captured once per host call, before
    /// its first await.
    fn ambient(&self) -> Option<TraceCarrier>;

    /// The span of the producer `spec` opens, with the trace's one sampling
    /// decision in its flags: the caller's child in [`TraceLinkMode::Parent`]
    /// mode, otherwise a new root. The only place a trace root is made.
    fn open_producer(&self, spec: &ProducerSpec) -> TraceCarrier;

    /// A random span id for a leaf, from the deployment's id source.
    fn leaf_span_id(&self) -> W3cSpanId;
}

/// The identity of a runtime with no telemetry adapter: random W3C ids, no
/// ambient context, and no sampler. A producer copies its caller's flags,
/// and a root is sampled: sinks decide for themselves.
#[derive(Clone, Copy, Debug, Default)]
pub struct LashIdentity;

impl TraceIdentity for LashIdentity {
    fn ambient(&self) -> Option<TraceCarrier> {
        None
    }

    fn open_producer(&self, spec: &ProducerSpec) -> TraceCarrier {
        match (&spec.caller, spec.link_mode) {
            (Some(caller), TraceLinkMode::Parent) => TraceCarrier::new(
                caller.trace_id(),
                random_span_id(),
                caller.flags(),
                caller.tracestate().clone(),
            ),
            _ => TraceCarrier::new(
                random_trace_id(),
                random_span_id(),
                W3cTraceFlags::from_byte(W3cTraceFlags::SAMPLED),
                W3cTraceState::default(),
            ),
        }
    }

    fn leaf_span_id(&self) -> W3cSpanId {
        random_span_id()
    }
}

#[expect(
    clippy::expect_used,
    reason = "a v4 UUID's version nibble is 4, so its bytes are never all zero"
)]
fn random_trace_id() -> W3cTraceId {
    W3cTraceId::from_bytes(*uuid::Uuid::new_v4().as_bytes()).expect("a v4 UUID is nonzero")
}

fn random_span_id() -> W3cSpanId {
    loop {
        let bytes = uuid::Uuid::new_v4().as_u64_pair().0.to_be_bytes();
        if let Ok(id) = W3cSpanId::from_bytes(bytes) {
            return id;
        }
    }
}

/// The durable identity a container span is keyed by.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum SpanKey<'a> {
    /// A turn, tool call, tool intent or process: its scope id.
    Scope { scope: &'a TraceScopeId },
    /// A code cell: its foreground graph key.
    Cell { graph_key: &'a str },
    /// One occurrence of a workflow node: its whole effect identity (task,
    /// site, occurrence and enclosing loop iterations), since a site alone
    /// does not name an execution.
    Node {
        at: &'a lash_protocol_types::EffectId,
    },
}

const SPAN_DOMAIN: &[u8] = b"lash.span\0";

/// The span lash gives the container `key` under `parent`: a pure function of
/// retained data, so every owner computes the same one. Its trace id, flags
/// and tracestate are the parent's.
#[expect(
    clippy::expect_used,
    reason = "a span key serializes into memory and has no failing case"
)]
pub fn derive_span(parent: &TraceCarrier, key: &SpanKey<'_>) -> TraceCarrier {
    let mut hasher = Sha256::new();
    hasher.update(SPAN_DOMAIN);
    hasher.update(parent.trace_id().to_bytes());
    hasher.update(parent.span_id().to_bytes());
    hasher.update(serde_json::to_vec(key).expect("a span key serializes"));
    let digest = hasher.finalize();
    let (chunks, _) = digest.as_chunks::<8>();
    let span_id = chunks
        .iter()
        .find_map(|bytes| W3cSpanId::from_bytes(*bytes).ok())
        .expect("a SHA-256 digest is not all zero");
    TraceCarrier::new(
        parent.trace_id(),
        span_id,
        parent.flags(),
        parent.tracestate().clone(),
    )
}

/// How a producer's host call ended.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProducerOutcome {
    /// This call's acceptance retained the cause.
    Selected,
    /// A retry read back an earlier acceptance's cause.
    Reused,
    /// The call failed before acceptance.
    Refused { error_type: Cow<'static, str> },
    /// The host future was dropped or panicked before it settled.
    Abandoned,
}

impl ProducerOutcome {
    /// The `lash.producer.outcome` value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Reused => "reused",
            Self::Refused { .. } => "refused",
            Self::Abandoned => "abandoned",
        }
    }
}

/// How a producer reports its one record: the runtime's emitter, given the
/// call's outcome, the ids known at settle and the phases it timed.
type SettleProducer =
    Box<dyn FnOnce(ProducerOutcome, crate::TraceIds, Vec<crate::ProcessStartPhaseTime>) + Send>;

/// The producer span of one host call that admits durable work: the trace
/// root, or the caller's child, that the admitted work takes as its parent.
///
/// It emits one whole record when it settles, to every sink. Dropped
/// unsettled (the host future was dropped or panicked), it settles
/// [`ProducerOutcome::Abandoned`].
#[must_use = "a producer settles when it is dropped"]
pub struct ProducerSpan {
    span: Option<TraceCarrier>,
    ids: crate::TraceIds,
    phases: Vec<crate::ProcessStartPhaseTime>,
    settle: Option<SettleProducer>,
}

impl ProducerSpan {
    /// The producer the runtime opened at `span`, knowing `ids`, which
    /// `settle` reports. Only the runtime opens one, through its
    /// [`TraceIdentity`].
    #[doc(hidden)]
    pub fn opened(span: TraceCarrier, ids: crate::TraceIds, settle: SettleProducer) -> Self {
        Self {
            span: Some(span),
            ids,
            phases: Vec::new(),
            settle: Some(settle),
        }
    }

    /// The producer of a runtime nothing observes: it parents nothing and
    /// records nothing.
    pub fn unobserved() -> Self {
        Self {
            span: None,
            ids: crate::TraceIds::default(),
            phases: Vec::new(),
            settle: None,
        }
    }

    /// The parent the work this call admits retains: `None` when the
    /// runtime is unobserved.
    pub fn parent(&self) -> Option<TraceParent> {
        self.span.clone().map(TraceParent)
    }

    /// Names the input this call's acceptance assigned, on the settle
    /// record.
    pub fn accepted(&mut self, send: lash_protocol_types::InputId) {
        self.ids.send = Some(send);
    }

    /// The ids its settle record names, for a call that learns one after
    /// it opened: the process a start registered, the intent it admitted.
    pub fn ids_mut(&mut self) -> &mut crate::TraceIds {
        &mut self.ids
    }

    /// Names the time the call spent per phase, on the settle record.
    pub fn timed(&mut self, phases: Vec<crate::ProcessStartPhaseTime>) {
        self.phases = phases;
    }

    /// Ends the host call with `outcome`, emitting the producer's record now.
    pub fn settle(mut self, outcome: ProducerOutcome) {
        self.finish(outcome);
    }

    fn finish(&mut self, outcome: ProducerOutcome) {
        if let Some(settle) = self.settle.take() {
            settle(
                outcome,
                std::mem::take(&mut self.ids),
                std::mem::take(&mut self.phases),
            );
        }
    }
}

impl Drop for ProducerSpan {
    fn drop(&mut self) {
        self.finish(ProducerOutcome::Abandoned);
    }
}

impl std::fmt::Debug for ProducerSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProducerSpan")
            .field("span", &self.span)
            .field("ids", &self.ids)
            .finish_non_exhaustive()
    }
}
