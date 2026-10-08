//! SDK-independent trace causality vocabulary.
//!
//! An admitted operation retains one typed [`TraceCause`] (who caused it) and,
//! where it parents later work, one [`DurableTraceScope`] holding the
//! [`TraceAnchor`] its first admission selected. Both are plain data: no
//! OpenTelemetry `Context`, SDK span, provider or header map is stored. An
//! adapter turns a [`TraceCarrier`] into an SDK span context and back.
//!
//! Everything here sits beside business payloads and takes no part in any
//! identity preimage: submission digests, trigger matching, process
//! retained-start checks, effect-envelope identity, usage keys, graph-node ids
//! and tool-intent payload hashes never cover a carrier, cause or scope.
//!
//! External observations are emitted under an [`EmissionPermit`], which only
//! freshly executed work and newly committed transitions hold; a retained read
//! or a replayed journal entry has none.

use lash_sansio::{ProcessId, RuntimeOwner, SessionId, TurnId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Most contexts one [`TraceLinks`] retains; later ones are counted, not kept.
pub const TRACE_LINK_LIMIT: usize = 64;
/// Most list members a W3C `tracestate` may hold.
pub const TRACESTATE_MEMBER_LIMIT: usize = 32;
/// Most characters the canonical `tracestate` may hold.
pub const TRACESTATE_CHAR_LIMIT: usize = 512;

const TRACEPARENT_LEN: usize = 55;
const TRACESTATE_KEY_LIMIT: usize = 256;
const TRACESTATE_TENANT_LIMIT: usize = 241;
const TRACESTATE_SYSTEM_LIMIT: usize = 14;
const TRACESTATE_VALUE_LIMIT: usize = 256;

/// Why a W3C trace context was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidTraceCarrier {
    #[error("traceparent is not `version-traceid-parentid-flags`")]
    TraceparentShape,
    #[error("traceparent holds a character that is not lowercase hex")]
    TraceparentEncoding,
    #[error("traceparent version `ff` is not a version")]
    TraceparentVersion,
    #[error("trace id is all zeroes")]
    ZeroTraceId,
    #[error("span id is all zeroes")]
    ZeroSpanId,
    #[error("tracestate holds {members} members, over the limit of {TRACESTATE_MEMBER_LIMIT}")]
    TracestateMembers { members: usize },
    #[error("tracestate holds {chars} characters, over the limit of {TRACESTATE_CHAR_LIMIT}")]
    TracestateLength { chars: usize },
    #[error("tracestate member {index} is not a valid `key=value`")]
    TracestateMember { index: usize },
    #[error("tracestate member {index} repeats an earlier key")]
    TracestateDuplicateKey { index: usize },
}

/// A nonzero W3C trace id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct W3cTraceId([u8; 16]);

impl W3cTraceId {
    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, InvalidTraceCarrier> {
        if bytes == [0; 16] {
            return Err(InvalidTraceCarrier::ZeroTraceId);
        }
        Ok(Self(bytes))
    }

    pub fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl std::fmt::Display for W3cTraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_hex(f, &self.0)
    }
}

impl std::fmt::Debug for W3cTraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "W3cTraceId({self})")
    }
}

/// A nonzero W3C span id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct W3cSpanId([u8; 8]);

impl W3cSpanId {
    pub fn from_bytes(bytes: [u8; 8]) -> Result<Self, InvalidTraceCarrier> {
        if bytes == [0; 8] {
            return Err(InvalidTraceCarrier::ZeroSpanId);
        }
        Ok(Self(bytes))
    }

    pub fn to_bytes(self) -> [u8; 8] {
        self.0
    }
}

impl std::fmt::Display for W3cSpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_hex(f, &self.0)
    }
}

impl std::fmt::Debug for W3cSpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "W3cSpanId({self})")
    }
}

/// The W3C trace-flags byte, kept whole so an unsampled context stays
/// distinguishable from an absent one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct W3cTraceFlags(u8);

impl W3cTraceFlags {
    pub const SAMPLED: u8 = 0x01;

    pub fn from_byte(byte: u8) -> Self {
        Self(byte)
    }

    pub fn to_byte(self) -> u8 {
        self.0
    }

    pub fn is_sampled(self) -> bool {
        self.0 & Self::SAMPLED != 0
    }
}

/// A validated W3C `tracestate`, in the order its members arrived.
///
/// The canonical form is the members joined by `,` with optional whitespace
/// and empty members removed. An empty state is the absent header.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct W3cTraceState(String);

impl W3cTraceState {
    /// Parses one `tracestate` value. A transport that received several
    /// `tracestate` headers joins them with `,` in arrival order first.
    pub fn parse(header: &str) -> Result<Self, InvalidTraceCarrier> {
        let mut keys: Vec<&str> = Vec::new();
        let mut canonical = String::new();
        for member in header.split(',') {
            let member = member.trim_matches([' ', '\t']);
            if member.is_empty() {
                continue;
            }
            let index = keys.len();
            if index == TRACESTATE_MEMBER_LIMIT {
                return Err(InvalidTraceCarrier::TracestateMembers {
                    members: header
                        .split(',')
                        .filter(|member| !member.trim_matches([' ', '\t']).is_empty())
                        .count(),
                });
            }
            let Some((key, value)) = member.split_once('=') else {
                return Err(InvalidTraceCarrier::TracestateMember { index });
            };
            if !valid_tracestate_key(key) || !valid_tracestate_value(value) {
                return Err(InvalidTraceCarrier::TracestateMember { index });
            }
            if keys.contains(&key) {
                return Err(InvalidTraceCarrier::TracestateDuplicateKey { index });
            }
            keys.push(key);
            if !canonical.is_empty() {
                canonical.push(',');
            }
            canonical.push_str(member);
        }
        if canonical.len() > TRACESTATE_CHAR_LIMIT {
            return Err(InvalidTraceCarrier::TracestateLength {
                chars: canonical.len(),
            });
        }
        Ok(Self(canonical))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The `(key, value)` members in order.
    pub fn members(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .split(',')
            .filter_map(|member| member.split_once('='))
    }
}

/// One W3C trace context: the identity of a span some operation can name as
/// its parent or link.
///
/// A carrier never claims its span is local. A span is local only while its
/// SDK handle is alive in this process; every carrier read from transport or
/// from storage is a remote span context.
///
/// The serialized form is the W3C text itself (`traceparent`, and `tracestate`
/// when it is not empty). Decoding validates it: invalid stored data is a
/// decode error, never a reason to mint replacement ancestry. Baggage is not
/// carried.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "TraceCarrierWire", into = "TraceCarrierWire")]
pub struct TraceCarrier {
    trace_id: W3cTraceId,
    span_id: W3cSpanId,
    flags: W3cTraceFlags,
    tracestate: W3cTraceState,
}

impl TraceCarrier {
    pub fn new(
        trace_id: W3cTraceId,
        span_id: W3cSpanId,
        flags: W3cTraceFlags,
        tracestate: W3cTraceState,
    ) -> Self {
        Self {
            trace_id,
            span_id,
            flags,
            tracestate,
        }
    }

    /// Strict parse of an explicitly supplied context: any invalid field is
    /// refused with its reason.
    pub fn parse_w3c(
        traceparent: &str,
        tracestate: Option<&str>,
    ) -> Result<Self, InvalidTraceCarrier> {
        let (trace_id, span_id, flags) = parse_traceparent(traceparent)?;
        let tracestate = match tracestate {
            Some(tracestate) => W3cTraceState::parse(tracestate)?,
            None => W3cTraceState::default(),
        };
        Ok(Self::new(trace_id, span_id, flags, tracestate))
    }

    /// Tolerant extraction from transport headers: an absent or invalid
    /// `traceparent` is no context, and an invalid `tracestate` is dropped
    /// while a valid `traceparent` survives. A malformed telemetry header
    /// never refuses the request that carried it.
    pub fn extract_w3c(traceparent: Option<&str>, tracestate: Option<&str>) -> Option<Self> {
        let (trace_id, span_id, flags) = parse_traceparent(traceparent?).ok()?;
        let tracestate = tracestate
            .and_then(|tracestate| W3cTraceState::parse(tracestate).ok())
            .unwrap_or_default();
        Some(Self::new(trace_id, span_id, flags, tracestate))
    }

    pub fn trace_id(&self) -> W3cTraceId {
        self.trace_id
    }

    pub fn span_id(&self) -> W3cSpanId {
        self.span_id
    }

    pub fn flags(&self) -> W3cTraceFlags {
        self.flags
    }

    pub fn tracestate(&self) -> &W3cTraceState {
        &self.tracestate
    }

    /// The version-00 `traceparent` header value.
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id,
            self.span_id,
            self.flags.to_byte()
        )
    }

    /// Whether both carriers name the same span, whatever their flags or
    /// state.
    pub fn same_span(&self, other: &Self) -> bool {
        self.trace_id == other.trace_id && self.span_id == other.span_id
    }
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TraceCarrier")]
struct TraceCarrierWire {
    /// W3C `traceparent`, version 00.
    traceparent: String,
    /// W3C `tracestate`, absent when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tracestate: Option<String>,
}

impl TryFrom<TraceCarrierWire> for TraceCarrier {
    type Error = InvalidTraceCarrier;

    fn try_from(wire: TraceCarrierWire) -> Result<Self, Self::Error> {
        Self::parse_w3c(&wire.traceparent, wire.tracestate.as_deref())
    }
}

impl From<TraceCarrier> for TraceCarrierWire {
    fn from(carrier: TraceCarrier) -> Self {
        Self {
            traceparent: carrier.traceparent(),
            tracestate: (!carrier.tracestate.is_empty()).then_some(carrier.tracestate.0),
        }
    }
}

impl schemars::JsonSchema for TraceCarrier {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        TraceCarrierWire::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        TraceCarrierWire::json_schema(generator)
    }
}

/// The contexts an operation is causally linked to, in stable admission order.
///
/// At most [`TRACE_LINK_LIMIT`] distinct spans are retained; the rest are
/// counted in `omitted`. The limit bounds exported telemetry, never which
/// business inputs are admitted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "TraceLinksWire", into = "TraceLinksWire")]
pub struct TraceLinks {
    contexts: Vec<TraceCarrier>,
    omitted: u32,
}

impl TraceLinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one producer context. A span already present is not added twice;
    /// one past the limit is counted as omitted.
    pub fn push(&mut self, context: TraceCarrier) {
        if self.contexts.iter().any(|kept| kept.same_span(&context)) {
            return;
        }
        if self.contexts.len() == TRACE_LINK_LIMIT {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        self.contexts.push(context);
    }

    /// Adds another set's contexts after this one's, carrying its omissions.
    pub fn merge(&mut self, other: Self) {
        self.omitted = self.omitted.saturating_add(other.omitted);
        for context in other.contexts {
            self.push(context);
        }
    }

    pub fn contexts(&self) -> &[TraceCarrier] {
        &self.contexts
    }

    pub fn omitted(&self) -> u32 {
        self.omitted
    }

    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty() && self.omitted == 0
    }
}

impl FromIterator<TraceCarrier> for TraceLinks {
    fn from_iter<I: IntoIterator<Item = TraceCarrier>>(contexts: I) -> Self {
        let mut links = Self::new();
        for context in contexts {
            links.push(context);
        }
        links
    }
}

/// Stored links that break the bound or repeat a span.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidTraceLinks {
    #[error("{contexts} linked contexts, over the limit of {TRACE_LINK_LIMIT}")]
    TooMany { contexts: usize },
    #[error("linked context {index} repeats an earlier span")]
    Duplicate { index: usize },
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TraceLinks")]
struct TraceLinksWire {
    contexts: Vec<TraceCarrier>,
    /// Contexts dropped past the retained limit.
    #[serde(default, skip_serializing_if = "is_zero")]
    omitted: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl TryFrom<TraceLinksWire> for TraceLinks {
    type Error = InvalidTraceLinks;

    fn try_from(wire: TraceLinksWire) -> Result<Self, Self::Error> {
        if wire.contexts.len() > TRACE_LINK_LIMIT {
            return Err(InvalidTraceLinks::TooMany {
                contexts: wire.contexts.len(),
            });
        }
        for (index, context) in wire.contexts.iter().enumerate() {
            if wire.contexts[..index]
                .iter()
                .any(|earlier| earlier.same_span(context))
            {
                return Err(InvalidTraceLinks::Duplicate { index });
            }
        }
        Ok(Self {
            contexts: wire.contexts,
            omitted: wire.omitted,
        })
    }
}

impl From<TraceLinks> for TraceLinksWire {
    fn from(links: TraceLinks) -> Self {
        Self {
            contexts: links.contexts,
            omitted: links.omitted,
        }
    }
}

impl schemars::JsonSchema for TraceLinks {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        TraceLinksWire::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        TraceLinksWire::json_schema(generator)
    }
}

/// How an admitted operation relates to what caused it.
///
/// The first accepted cause is retained: a retry of the same business
/// submission under another context reads this one back, and an operation
/// first admitted as `Root` stays `Root`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "relation", content = "from", rename_all = "snake_case")]
pub enum TraceCause {
    /// No cause: the operation starts a trace of its own. An adapter starts
    /// it under an explicit empty context, never the ambient one.
    #[default]
    Root,
    /// The operation is an owned invocation of this span.
    Parent(TraceCarrier),
    /// The operation is independently admitted work these spans communicated
    /// with; it starts its own trace and links them.
    Linked(TraceLinks),
}

impl TraceCause {
    /// A linked cause, or `Root` when there is nothing to link.
    pub fn linked(links: TraceLinks) -> Self {
        if links.is_empty() {
            Self::Root
        } else {
            Self::Linked(links)
        }
    }

    /// A cause linking one producer, or `Root` when it had no context.
    pub fn linked_to(producer: Option<TraceCarrier>) -> Self {
        Self::linked(producer.into_iter().collect())
    }

    /// Every context this cause names, parent or links.
    pub fn contexts(&self) -> &[TraceCarrier] {
        match self {
            Self::Root => &[],
            Self::Parent(parent) => std::slice::from_ref(parent),
            Self::Linked(links) => links.contexts(),
        }
    }

    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }

    /// The cause of a scope that admits the work `members` caused, in
    /// admission order.
    ///
    /// One member's cause is the scope's. Several are fan-in: an owned
    /// invocation heading them keeps its parent, and otherwise the scope
    /// links every producer.
    pub fn of_admitted<'a>(members: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut members = members.into_iter();
        let Some(head) = members.next() else {
            return Self::Root;
        };
        if let Self::Parent(_) = head {
            return head.clone();
        }
        let mut links: TraceLinks = head.linked_contexts();
        for member in members {
            links.merge(member.linked_contexts());
        }
        Self::linked(links)
    }

    fn linked_contexts(&self) -> TraceLinks {
        match self {
            Self::Root => TraceLinks::new(),
            Self::Parent(parent) => std::iter::once(parent.clone()).collect(),
            Self::Linked(links) => links.clone(),
        }
    }
}

/// The durable owner of a trace scope: the admitted thing whose work the
/// scope parents.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TraceScopeOwner {
    /// An admitted run, the owner of its execution.
    Run { session_id: SessionId, run: TurnId },
    /// One physical agent turn, including a follow-on within a run.
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    /// One logical operation Run.
    Operation {
        session_id: SessionId,
        operation_id: String,
    },
    /// One tool call under its original logical owner.
    Tool {
        owner: TraceToolOwner,
        call_id: String,
    },
    /// One host-submitted tool intent, by its replay key, under the
    /// runtime whose authority declared it: a session or a process.
    ToolIntent {
        owner: RuntimeOwner,
        replay_key: String,
    },
    /// A registered process, across all of its segments.
    Process { process_id: ProcessId },
}

/// The logical owner of a tool call, independent of its execution route.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TraceToolOwner {
    /// The admitted logical run, whose turn id stays fixed across follow-ons.
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Process {
        process_id: ProcessId,
    },
    Operation {
        session_id: SessionId,
        operation_id: String,
    },
}

/// The kind of a scope owner: the static spelling exported telemetry uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TraceScopeKind {
    Run,
    Turn,
    Tool,
    ToolIntent,
    Process,
}

impl TraceScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Turn => "turn",
            Self::Tool => "tool",
            Self::ToolIntent => "tool_intent",
            Self::Process => "process",
        }
    }
}

/// A trace scope's identity: its owner and the owner's boundary ordinal.
///
/// Boundary 0 is the owner's admission. A later boundary of the same owner
/// (a process segment, a checkpoint that consumed new input) takes the next
/// ordinal the owner's own records already count. The id is separate from
/// every business identity preimage.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceScopeId {
    pub owner: TraceScopeOwner,
    #[serde(default, skip_serializing_if = "is_zero_boundary")]
    pub boundary: u64,
}

fn is_zero_boundary(boundary: &u64) -> bool {
    *boundary == 0
}

impl TraceScopeId {
    /// The owner's admission scope.
    pub fn admission(owner: TraceScopeOwner) -> Self {
        Self { owner, boundary: 0 }
    }

    /// The same owner at a later boundary.
    pub fn at_boundary(&self, boundary: u64) -> Self {
        Self {
            owner: self.owner.clone(),
            boundary,
        }
    }

    pub fn kind(&self) -> TraceScopeKind {
        match self.owner {
            TraceScopeOwner::Run { .. } => TraceScopeKind::Run,
            TraceScopeOwner::Operation { .. } => TraceScopeKind::Run,
            TraceScopeOwner::Turn { .. } => TraceScopeKind::Turn,
            TraceScopeOwner::Tool { .. } => TraceScopeKind::Tool,
            TraceScopeOwner::ToolIntent { .. } => TraceScopeKind::ToolIntent,
            TraceScopeOwner::Process { .. } => TraceScopeKind::Process,
        }
    }
}

/// The span context a scope's first admission selected for its children.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", content = "context", rename_all = "snake_case")]
pub enum TraceAnchor {
    /// No adapter was installed when the scope was admitted. Nothing invents
    /// an anchor for it later; its cause is still retained for record
    /// consumers.
    #[default]
    Untraced,
    /// The admission span's context. It may be unsampled: an unsampled
    /// context is retained and propagated, which is not the same as no
    /// telemetry.
    Context(TraceCarrier),
}

impl TraceAnchor {
    pub fn context(&self) -> Option<&TraceCarrier> {
        match self {
            Self::Untraced => None,
            Self::Context(context) => Some(context),
        }
    }
}

/// The retained telemetry of one admitted scope: what caused it, the anchor
/// its children hang under, and when it started.
///
/// It is written once, by the admission that inserts its owner, in the same
/// write. Every later admission of that owner reads it back unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DurableTraceScope {
    pub scope: TraceScopeId,
    #[serde(default, skip_serializing_if = "is_root_cause")]
    pub cause: TraceCause,
    #[serde(default, skip_serializing_if = "is_untraced")]
    pub anchor: TraceAnchor,
    /// Wall-clock epoch milliseconds of the first admission.
    pub started_at_ms: u64,
}

fn is_root_cause(cause: &TraceCause) -> bool {
    matches!(cause, TraceCause::Root)
}

fn is_untraced(anchor: &TraceAnchor) -> bool {
    matches!(anchor, TraceAnchor::Untraced)
}

impl DurableTraceScope {
    /// The context a child of this scope is admitted under: its anchor as
    /// the owning parent. An untraced scope gives its children no parent.
    pub fn parent_cause(&self) -> TraceCause {
        match &self.anchor {
            TraceAnchor::Untraced => TraceCause::Root,
            TraceAnchor::Context(context) => TraceCause::Parent(context.clone()),
        }
    }

    /// The cause of independent work this scope produced: a link to its
    /// anchor.
    pub fn linked_cause(&self) -> TraceCause {
        TraceCause::linked_to(self.anchor.context().cloned())
    }

    /// The cause and anchor this scope retains, as an offer: what a typed
    /// handler input carries for a scope its store already admitted.
    pub fn offer(&self) -> TraceScopeOffer {
        TraceScopeOffer::new(self.cause.clone(), self.anchor.clone())
    }
}

/// What an admission offers its store for the scope the store may insert:
/// the cause the caller was given and the anchor its candidate proposed.
///
/// The store that inserts the owning row builds the [`DurableTraceScope`]
/// from it, with the owner id and start time that same write records. A
/// store that finds the row already admitted ignores the offer and returns
/// the retained scope. Like every trace field, an offer is no part of any
/// business identity.
///
/// An empty offer (a root cause, no anchor) is one word wide, so the
/// requests and commands that carry one pay nothing for the common
/// untraced case.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TraceScopeOfferWire", into = "TraceScopeOfferWire")]
pub struct TraceScopeOffer(Option<Box<TraceScopeOfferWire>>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TraceScopeOffer")]
struct TraceScopeOfferWire {
    #[serde(default, skip_serializing_if = "is_root_cause")]
    cause: TraceCause,
    #[serde(default, skip_serializing_if = "is_untraced")]
    anchor: TraceAnchor,
}

static RUN_CAUSE: TraceCause = TraceCause::Root;
static UNTRACED_ANCHOR: TraceAnchor = TraceAnchor::Untraced;

impl TraceScopeOffer {
    pub fn new(cause: TraceCause, anchor: TraceAnchor) -> Self {
        if is_root_cause(&cause) && is_untraced(&anchor) {
            return Self(None);
        }
        Self(Some(Box::new(TraceScopeOfferWire { cause, anchor })))
    }

    /// An offer with no candidate anchor: the cause alone is retained.
    pub fn caused_by(cause: TraceCause) -> Self {
        Self::new(cause, TraceAnchor::Untraced)
    }

    /// The cause the caller was given.
    pub fn cause(&self) -> &TraceCause {
        self.0.as_ref().map_or(&RUN_CAUSE, |offer| &offer.cause)
    }

    /// The anchor the caller's admission candidate proposed.
    pub fn anchor(&self) -> &TraceAnchor {
        self.0
            .as_ref()
            .map_or(&UNTRACED_ANCHOR, |offer| &offer.anchor)
    }

    /// Whether the offer says nothing: a root cause and no anchor.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// The scope the inserting admission retains for `scope`, started at
    /// `started_at_ms`.
    pub fn into_scope(self, scope: TraceScopeId, started_at_ms: u64) -> DurableTraceScope {
        let TraceScopeOfferWire { cause, anchor } = self.0.map(|offer| *offer).unwrap_or_default();
        DurableTraceScope {
            scope,
            cause,
            anchor,
            started_at_ms,
        }
    }
}

impl From<TraceScopeOfferWire> for TraceScopeOffer {
    fn from(wire: TraceScopeOfferWire) -> Self {
        Self::new(wire.cause, wire.anchor)
    }
}

impl From<TraceScopeOffer> for TraceScopeOfferWire {
    fn from(offer: TraceScopeOffer) -> Self {
        offer.0.map(|offer| *offer).unwrap_or_default()
    }
}

impl schemars::JsonSchema for TraceScopeOffer {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        TraceScopeOfferWire::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        TraceScopeOfferWire::json_schema(generator)
    }
}

/// What a store's admission returns for the scope it was offered: the one it
/// wrote, or the one an earlier admission already retained.
///
/// It is never serialized. A journaled result holds the [`DurableTraceScope`]
/// alone, so reading it back yields no permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceScopeAdmission {
    Inserted(DurableTraceScope),
    Existing(DurableTraceScope),
}

impl TraceScopeAdmission {
    /// The admission of `scope`: inserted when the caller's store receipt
    /// says this call wrote the owning row.
    pub fn of(scope: DurableTraceScope, inserted: bool) -> Self {
        if inserted {
            Self::Inserted(scope)
        } else {
            Self::Existing(scope)
        }
    }

    /// The retained scope: dispatch continues under it, never under a losing
    /// candidate.
    pub fn scope(&self) -> &DurableTraceScope {
        match self {
            Self::Inserted(scope) | Self::Existing(scope) => scope,
        }
    }

    pub fn into_scope(self) -> DurableTraceScope {
        match self {
            Self::Inserted(scope) | Self::Existing(scope) => scope,
        }
    }

    /// The permit of the admission that won, and none for a retained read.
    pub fn permit(&self) -> Option<EmissionPermit> {
        match self {
            Self::Inserted(_) => Some(EmissionPermit::new_transition()),
            Self::Existing(_) => None,
        }
    }

    pub fn outcome(&self) -> TraceCandidateOutcome {
        match self {
            Self::Inserted(_) => TraceCandidateOutcome::Selected,
            Self::Existing(_) => TraceCandidateOutcome::Reused,
        }
    }
}

/// What became of one admission candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TraceCandidateOutcome {
    /// Its anchor was retained: it is the scope's admission span.
    Selected,
    /// An earlier admission's scope was retained instead.
    Reused,
    /// The admission was refused or failed; nothing was retained.
    Refused,
}

impl TraceCandidateOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Reused => "reused",
            Self::Refused => "refused",
        }
    }
}

/// One admission attempt's short local span, proposed before the durable
/// write and settled after it.
///
/// It lives only inside the admission call: never across a suspension, never
/// under a store lock, and its export is never required for the admission to
/// commit. The admission that retains its scope is the obligation to export
/// it: a reader of a retained admission whose export may be owed reconciles
/// it with [`TraceScopeFactory::export_admitted`].
pub trait TraceAdmissionCandidate: Send {
    /// The anchor this candidate would give the scope.
    fn anchor(&self) -> TraceAnchor;

    /// Ends the candidate's span. A selected candidate takes its scope's
    /// admitted name; the others keep the attempt name and record the
    /// outcome.
    fn settle(self: Box<Self>, outcome: TraceCandidateOutcome);

    /// Hands the candidate to its adapter when its admission may have
    /// committed but its owner cannot tell (the commit's acknowledgement was
    /// lost). The adapter selects it when a reader reconciles its anchor's
    /// admission ([`TraceScopeFactory::export_admitted`]), and refuses it
    /// once another candidate of its scope is selected. Without an adapter
    /// that holds candidates, it is refused now.
    fn defer(self: Box<Self>) {
        self.settle(TraceCandidateOutcome::Refused);
    }
}

/// A short host operation. Its carrier names this call, never a durable owner.
pub trait TraceHostOperation: Send {
    fn carrier(&self) -> TraceCarrier;
    /// Finish after the ingress commit. A retained acceptance remains an attempt.
    fn settle(self: Box<Self>, outcome: TraceCandidateOutcome);
}

/// The single identity-producing telemetry adapter of a runtime.
///
/// It mints admission anchors and reads the host's ambient context; it never
/// owns persistence. A runtime without an adapter uses [`UntracedScopes`].
pub trait TraceScopeFactory: Send + Sync {
    /// Snapshots the caller's current context, if the host has one. Only a
    /// facade entry point calls this, once per submission and before its
    /// first await; engine code passes explicit values.
    fn capture_current(&self) -> Option<TraceCarrier>;

    /// Begin a host send under its captured parent, before the first await.
    /// No installed adapter means no operation or carrier.
    fn begin_host_send(
        &self,
        _parent: Option<&TraceCarrier>,
    ) -> Option<Box<dyn TraceHostOperation>> {
        None
    }

    /// Starts an admission candidate for `scope` under `cause`.
    fn propose(&self, scope: &TraceScopeId, cause: &TraceCause)
    -> Box<dyn TraceAdmissionCandidate>;

    /// Exports the admission of `scope`, retained by a durable admission
    /// whose owner may not have exported it (it lost its life or the
    /// commit's acknowledgement first): the admission under the identity of
    /// its anchor. A reader that owes the admission's export reconciles it
    /// here; the adapter dedupes the identity, so an admission its candidate
    /// or an earlier reconcile already exported is exported no more.
    fn export_admitted(&self, _scope: &DurableTraceScope) {}
}

/// The factory of a runtime with no telemetry adapter: every scope is
/// admitted [`TraceAnchor::Untraced`] and nothing is captured.
#[derive(Clone, Copy, Debug, Default)]
pub struct UntracedScopes;

struct UntracedCandidate;

impl TraceAdmissionCandidate for UntracedCandidate {
    fn anchor(&self) -> TraceAnchor {
        TraceAnchor::Untraced
    }

    fn settle(self: Box<Self>, _outcome: TraceCandidateOutcome) {}
}

impl TraceScopeFactory for UntracedScopes {
    fn capture_current(&self) -> Option<TraceCarrier> {
        None
    }

    fn propose(
        &self,
        _scope: &TraceScopeId,
        _cause: &TraceCause,
    ) -> Box<dyn TraceAdmissionCandidate> {
        Box::new(UntracedCandidate)
    }
}

/// One real execution of a journaled body: a body that runs again after a
/// failure before its journal commit is another attempt.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct TraceAttemptId(String);

impl TraceAttemptId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The right to emit an external observation.
///
/// Only two things hold one: the body of a journaled step while it really
/// runs, and the caller a store just told its lifecycle fact is new. It is
/// neither serializable nor clonable, so it cannot be journaled and come back
/// fresh on replay.
#[derive(Debug)]
pub struct EmissionPermit {
    source: EmissionSource,
}

/// Where an [`EmissionPermit`] came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmissionSource {
    /// A journaled body executing now, in this attempt.
    LiveExecution { attempt: TraceAttemptId },
    /// A durable lifecycle fact this caller just inserted or changed.
    NewTransition,
}

impl EmissionPermit {
    /// Minted only inside the body passed to a journaled step.
    #[doc(hidden)]
    pub fn live_execution(attempt: TraceAttemptId) -> Self {
        Self {
            source: EmissionSource::LiveExecution { attempt },
        }
    }

    /// Minted only from a store receipt that reports a newly inserted or
    /// changed lifecycle fact, after its commit.
    #[doc(hidden)]
    pub fn new_transition() -> Self {
        Self {
            source: EmissionSource::NewTransition,
        }
    }

    pub fn source(&self) -> &EmissionSource {
        &self.source
    }
}

/// The lifecycle transition a logical record reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceTransitionKind {
    Admitted,
    Started,
    Terminal,
    WaitStarted,
    WaitResolved,
    InputConsumed,
}

/// The identity of one trace record, projected into [`TraceRecord::id`].
///
/// Two reconstructions of one durable fact name the same record; two real
/// attempts name different ones. Graph-node ids, tool-call ids and effect
/// keys stay separately owned.
///
/// [`TraceRecord::id`]: crate::TraceRecord::id
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum TraceRecordIdentity {
    /// One committed transition of a durable engine wait.
    Wait {
        wait_id: String,
        transition: TraceTransitionKind,
    },
    /// One real host-owned or diagnostic attempt, with no durable owner.
    UnscopedLive {
        attempt: TraceAttemptId,
        ordinal: u64,
    },
    /// A logical record: one durable transition of a scope.
    Transition {
        scope: TraceScopeId,
        transition: TraceTransitionKind,
        ordinal: u64,
    },
    /// A live record: one event of one real execution attempt.
    Live {
        scope: TraceScopeId,
        attempt: TraceAttemptId,
        ordinal: u64,
    },
}

const TRACE_RECORD_ID_DOMAIN: &[u8] = b"lash.trace-record-id\0";

impl TraceRecordIdentity {
    /// The record id: a domain-separated hash of the identity, rendered as 32
    /// lowercase hex characters.
    pub fn record_id(&self) -> Result<String, serde_json::Error> {
        let preimage = serde_json::to_vec(self)?;
        let mut hasher = Sha256::new();
        hasher.update(TRACE_RECORD_ID_DOMAIN);
        hasher.update(&preimage);
        let digest = hasher.finalize();
        Ok(hex(&digest[..16]))
    }
}

impl crate::TraceRecord {
    /// Emit a host-owned live record. Each call mints an independent attempt
    /// identity and stamps the host's current UTC time; durable facts use
    /// [`Self::identified`] with their retained identity and timestamp.
    pub fn host_owned(
        context: crate::TraceContext,
        event: crate::TraceEvent,
    ) -> Result<Self, serde_json::Error> {
        Self::identified(
            &TraceRecordIdentity::UnscopedLive {
                attempt: TraceAttemptId::new(uuid::Uuid::new_v4().to_string()),
                ordinal: 0,
            },
            context,
            event,
            chrono::Utc::now(),
        )
    }

    /// A record whose id is its [`TraceRecordIdentity`] and whose timestamp is
    /// the retained time of the fact it reports, so reconstructing the fact
    /// rebuilds the same record.
    pub fn identified(
        identity: &TraceRecordIdentity,
        context: crate::TraceContext,
        event: crate::TraceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            schema_version: crate::TRACE_SCHEMA_VERSION,
            id: identity.record_id()?,
            timestamp,
            context,
            event,
        })
    }
}

/// The durable-substrate attempt a handler is executing under, as that
/// substrate's transport delivered it.
///
/// It is an optional capability of the substrate seam: a substrate with no
/// attempt notion reports none. Freshly emitted domain spans link its
/// context; it never becomes an operation's parent and is never stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttemptObservation {
    /// The attempt span the substrate injected, absent when its tracing is
    /// off or the header was malformed.
    pub context: Option<TraceCarrier>,
    /// The substrate's invocation id, when the delivery names one.
    pub invocation_id: Option<String>,
}

fn parse_traceparent(
    header: &str,
) -> Result<(W3cTraceId, W3cSpanId, W3cTraceFlags), InvalidTraceCarrier> {
    let header = header.trim_matches([' ', '\t']);
    let bytes = header.as_bytes();
    if bytes.len() < TRACEPARENT_LEN || bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return Err(InvalidTraceCarrier::TraceparentShape);
    }
    let mut version = [0_u8; 1];
    decode_hex(&bytes[0..2], &mut version)?;
    match version[0] {
        0xff => return Err(InvalidTraceCarrier::TraceparentVersion),
        // Version 00 is exactly four fields.
        0 if bytes.len() != TRACEPARENT_LEN => return Err(InvalidTraceCarrier::TraceparentShape),
        // A later version may append fields after another delimiter.
        _ if bytes.len() > TRACEPARENT_LEN && bytes[TRACEPARENT_LEN] != b'-' => {
            return Err(InvalidTraceCarrier::TraceparentShape);
        }
        _ => {}
    }
    let mut trace_id = [0_u8; 16];
    decode_hex(&bytes[3..35], &mut trace_id)?;
    let mut span_id = [0_u8; 8];
    decode_hex(&bytes[36..52], &mut span_id)?;
    let mut flags = [0_u8; 1];
    decode_hex(&bytes[53..55], &mut flags)?;
    Ok((
        W3cTraceId::from_bytes(trace_id)?,
        W3cSpanId::from_bytes(span_id)?,
        W3cTraceFlags::from_byte(flags[0]),
    ))
}

fn decode_hex(text: &[u8], out: &mut [u8]) -> Result<(), InvalidTraceCarrier> {
    let (pairs, _) = text.as_chunks::<2>();
    for (byte, [high, low]) in out.iter_mut().zip(pairs) {
        *byte = (hex_digit(*high)? << 4) | hex_digit(*low)?;
    }
    Ok(())
}

fn hex_digit(digit: u8) -> Result<u8, InvalidTraceCarrier> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(InvalidTraceCarrier::TraceparentEncoding),
    }
}

fn write_hex(f: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn tracestate_key_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'*' | b'/')
}

fn tracestate_key_part(part: &str, limit: usize, digit_start: bool) -> bool {
    let bytes = part.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    let first_ok = first.is_ascii_lowercase() || (digit_start && first.is_ascii_digit());
    first_ok && bytes.len() <= limit && bytes.iter().copied().all(tracestate_key_char)
}

fn valid_tracestate_key(key: &str) -> bool {
    match key.split_once('@') {
        Some((tenant, system)) => {
            tracestate_key_part(tenant, TRACESTATE_TENANT_LIMIT, true)
                && tracestate_key_part(system, TRACESTATE_SYSTEM_LIMIT, false)
        }
        None => tracestate_key_part(key, TRACESTATE_KEY_LIMIT, true),
    }
}

fn valid_tracestate_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= TRACESTATE_VALUE_LIMIT
        && bytes.last() != Some(&b' ')
        && bytes
            .iter()
            .all(|byte| matches!(byte, 0x20..=0x7e) && !matches!(byte, b',' | b'='))
}

/// Projection of a permitted domain observation. Durable ancestry is explicit.
///
/// The emitter calls it once per emitted record, after the permit check, with
/// the record already built. `scope` is the retained scope the record was made
/// under, `attempt` the substrate attempt that made it, when the substrate
/// reports one.
pub trait TraceDomainProjector: Send + Sync {
    fn project(
        &self,
        scope: &DurableTraceScope,
        attempt: Option<&AttemptObservation>,
        source: &EmissionSource,
        record: &crate::TraceRecord,
    );
}

#[cfg(test)]
mod tests;

/// Injected operational instruments, with no-op handles when telemetry is absent.
#[path = "otel/metrics.rs"]
pub mod metrics;
