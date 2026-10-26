//! SDK-independent trace causality vocabulary.
//!
//! An admitted operation retains one typed [`TraceCause`] (who caused it) and,
//! where it parents later work, one [`DurableTraceScope`] holding the span
//! lash derived for it ([`derive_span`]). Both are plain data: no
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

use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::RuntimeOwner;
use lash_vocabulary::identity::TurnId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use lash_protocol_types::{
    trace_carrier::InvalidTraceCarrier, trace_carrier::TRACESTATE_CHAR_LIMIT,
    trace_carrier::TRACESTATE_MEMBER_LIMIT, trace_carrier::TraceCarrier,
    trace_carrier::TraceStateBudget, trace_carrier::W3cSpanId, trace_carrier::W3cTraceFlags,
    trace_carrier::W3cTraceId, trace_carrier::W3cTraceState,
};

/// A host's narrowing of the W3C `tracestate` ceilings, for carriers it
/// parses from its own transport ([`TraceCarrier::parse_w3c_with`]). The W3C
/// maxima are the defaults and the hard ceilings: a host narrows them, never
/// raises them.
///
/// Sealed: built through [`TraceStateLimits::new`], which refuses a value
/// over its ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TraceStateLimits {
    members: usize,
    chars: usize,
    key: usize,
    tenant: usize,
    system: usize,
    value: usize,
}

impl TraceStateLimits {
    /// The W3C ceilings: 32 members, 512 characters, 256-byte keys, 241-byte
    /// tenant and 14-byte system parts of a multi-tenant key, and 256-byte
    /// values.
    pub const fn standard() -> Self {
        use crate::TracePolicyField as F;
        Self {
            members: F::TracestateMembers.default_value() as usize,
            chars: F::TracestateChars.default_value() as usize,
            key: F::TracestateKeyBytes.default_value() as usize,
            tenant: F::TracestateTenantBytes.default_value() as usize,
            system: F::TracestateSystemBytes.default_value() as usize,
            value: F::TracestateValueBytes.default_value() as usize,
        }
    }

    /// Limits of at most `members` members and `chars` characters, `key`
    /// bytes of a simple key, `tenant` and `system` bytes of a multi-tenant
    /// key's parts and `value` bytes of a value. Each is refused over its
    /// W3C ceiling (`tracestate_limits.*`).
    pub fn new(
        members: u64,
        chars: u64,
        key: u64,
        tenant: u64,
        system: u64,
        value: u64,
    ) -> Result<Self, crate::TracePolicyError> {
        use crate::TracePolicyField as F;
        let fields = [
            (F::TracestateMembers, members),
            (F::TracestateChars, chars),
            (F::TracestateKeyBytes, key),
            (F::TracestateTenantBytes, tenant),
            (F::TracestateSystemBytes, system),
            (F::TracestateValueBytes, value),
        ];
        for (field, value) in fields {
            field.check(value)?;
        }
        let size = |value: u64| usize::try_from(value).unwrap_or(usize::MAX);
        Ok(Self {
            members: size(members),
            chars: size(chars),
            key: size(key),
            tenant: size(tenant),
            system: size(system),
            value: size(value),
        })
    }

    pub const fn members(&self) -> usize {
        self.members
    }
    pub const fn chars(&self) -> usize {
        self.chars
    }
    pub const fn key_bytes(&self) -> usize {
        self.key
    }
    pub const fn tenant_bytes(&self) -> usize {
        self.tenant
    }
    pub const fn system_bytes(&self) -> usize {
        self.system
    }
    pub const fn value_bytes(&self) -> usize {
        self.value
    }
}

impl Default for TraceStateLimits {
    fn default() -> Self {
        Self::standard()
    }
}

impl TraceStateBudget for TraceStateLimits {
    fn members(&self) -> usize {
        self.members
    }
    fn chars(&self) -> usize {
        self.chars
    }
    fn key_bytes(&self) -> usize {
        self.key
    }
    fn tenant_bytes(&self) -> usize {
        self.tenant
    }
    fn system_bytes(&self) -> usize {
        self.system
    }
    fn value_bytes(&self) -> usize {
        self.value
    }
}

/// The contexts an operation is causally linked to, in stable admission order.
///
/// At most its limit of distinct spans are retained, the configured
/// [`TraceLimits::links`](crate::TraceLimits::links); the rest are counted in
/// `omitted`. The limit bounds exported telemetry, never which business
/// inputs are admitted.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "TraceLinksWire", into = "TraceLinksWire")]
pub struct TraceLinks {
    limit: usize,
    contexts: Vec<TraceCarrier>,
    omitted: u32,
}

impl PartialEq for TraceLinks {
    fn eq(&self, other: &Self) -> bool {
        self.contexts == other.contexts && self.omitted == other.omitted
    }
}
impl Eq for TraceLinks {}

impl Default for TraceLinks {
    fn default() -> Self {
        Self {
            contexts: Vec::new(),
            omitted: 0,
            limit: TRACE_LINK_STANDARD,
        }
    }
}

/// The standard link limit, `trace_limits.links`' default.
const TRACE_LINK_STANDARD: usize = crate::TracePolicyField::Links.default_value() as usize;
/// The most links any stored set holds: `trace_limits.links`' maximum.
const TRACE_LINK_CEILING: usize = match crate::TracePolicyField::Links.bounds() {
    Some(bounds) => bounds.max as usize,
    None => 0,
};

impl TraceLinks {
    /// An empty set that keeps at most `limit` links, the configured
    /// [`TraceLimits::links`](crate::TraceLimits::links).
    pub fn with_limit(limit: u32) -> Self {
        Self {
            limit: usize::try_from(limit)
                .map_or(TRACE_LINK_CEILING, |limit| limit.min(TRACE_LINK_CEILING)),
            ..Self::default()
        }
    }

    /// The same links, keeping at most `limit` from now on: a set read back
    /// from storage takes the limit of the owner that extends it.
    #[must_use]
    pub fn limited(mut self, limit: u32) -> Self {
        self.limit = Self::with_limit(limit).limit;
        self
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one producer context. A span already present is not added twice;
    /// one past the limit is counted as omitted.
    pub fn push(&mut self, context: TraceCarrier) {
        if self.contexts.iter().any(|kept| kept.same_span(&context)) {
            return;
        }
        if self.contexts.len() >= self.limit {
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
    #[error("{contexts} linked contexts, over the ceiling of {TRACE_LINK_CEILING}")]
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
        if wire.contexts.len() > TRACE_LINK_CEILING {
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
            limit: TRACE_LINK_STANDARD,
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

/// What caused an admitted operation, as its first acceptance retained it.
///
/// The first accepted cause is retained: a retry of the same business
/// submission under another context reads this one back. It is stored data;
/// lash builds it only from a [`TraceParent`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceCause {
    /// No provenance. The scope is unobserved: it has no span and records
    /// none. Also the canonical value of effect-envelope canonicalization
    /// and the decode of a NULL column. Never a trace root.
    #[default]
    None,
    /// The operation is an owned invocation of this span.
    Parent(TraceCarrier),
}

impl From<Option<TraceParent>> for TraceCause {
    fn from(parent: Option<TraceParent>) -> Self {
        parent.map_or(Self::None, |parent| Self::Parent(parent.0))
    }
}

impl TraceCause {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// The parent work this cause admits takes: the span it names.
    pub fn as_parent(&self) -> Option<TraceParent> {
        match self {
            Self::None => None,
            Self::Parent(parent) => Some(TraceParent(parent.clone())),
        }
    }

    /// The parent and links of a scope that admits the work `members`
    /// caused, in admission order: the head member's parent, and every
    /// other member's parent as a link (fan-in), at most `limit` of them,
    /// the configured [`TraceLimits::links`](crate::TraceLimits::links). A
    /// head with no cause admits the scope unobserved.
    pub fn of_admitted<'a>(
        members: impl IntoIterator<Item = &'a Self>,
        limit: u32,
    ) -> (Option<TraceParent>, TraceLinks) {
        let mut members = members.into_iter();
        let parent = members.next().and_then(Self::as_parent);
        let mut links = TraceLinks::with_limit(limit);
        if let Some(head) = &parent {
            for member in members {
                if let Self::Parent(context) = member
                    && !context.same_span(&head.0)
                {
                    links.push(context.clone());
                }
            }
        }
        (parent, links)
    }
}

/// What admitted work retains as its parent. Only a producer span, an
/// observed retained scope or a retained cause makes one; nothing builds
/// one from a carrier a caller supplies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceParent(TraceCarrier);

impl TraceParent {
    pub fn carrier(&self) -> &TraceCarrier {
        &self.0
    }

    /// The container `key` under this parent, as the parent of the work it
    /// issues: a code cell's span under its turn's, which parents the tool
    /// calls the cell makes. Its span is derived, so every owner names the
    /// same one.
    #[must_use]
    pub fn container(&self, key: &SpanKey<'_>) -> Self {
        Self(derive_span(&self.0, key))
    }
}

/// How a producer span relates to its caller's context. One spelling on
/// every producer entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceLinkMode {
    /// The producer is the caller's child.
    #[default]
    Parent,
    /// The producer is a new trace root that links the caller.
    Link,
}

/// The host call a producer span stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProducerKind {
    /// A send of one input.
    Send,
    /// A batch send.
    SendBatch,
    /// A host process start.
    ProcessStart,
    /// A host tool-intent submission.
    ToolIntentSubmit,
    /// A plugin operation invoked through the host admin surface.
    PluginInvoke,
}

impl ProducerKind {
    /// The producer span's name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Send => "lash.send",
            Self::SendBatch => "lash.send.batch",
            Self::ProcessStart => "lash.process.start",
            Self::ToolIntentSubmit => "lash.tool_intent.submit",
            Self::PluginInvoke => "lash.plugin.invoke",
        }
    }
}

/// A phase of a host process start, in the order a start passes them. Each
/// phase a start reached carries its duration on the start's producer.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStartPhase {
    /// The host call before staging: the start key, the session-turn check
    /// and the effect's dispatch.
    Dispatch,
    /// The recorded admission: the host session's liveness and the
    /// session-turn admission.
    Admission,
    /// Staging the execution environment under the start's claim and
    /// reading it back.
    Environment,
    /// Reading and decoding the stored definition descriptor.
    DefinitionLoad,
    /// The engine deriving the definition's signature, including reading
    /// its document and any cache lookup or decode that read needs.
    DefinitionResolve,
    /// Acquiring the definition's artifact closure under the start's
    /// claim, including store transactions and their lock waits.
    DefinitionAcquire,
    /// Verifying the host's signature claim against the stored definition,
    /// including the engine's independent derivation.
    DefinitionVerify,
    /// Building the definition's start payload, or realizing a function
    /// the start carries inline. The named definition operations above
    /// have their own disjoint timings.
    Definition,
    /// The engine's admission of the payload and of its sealed settings.
    Admit,
    /// Holding the engine artifacts the payload names.
    Artifacts,
    /// The link check: the effects the payload performs against the
    /// environment's links and this node's components.
    LinkCheck,
    /// Staging the input's attachments.
    Input,
    /// Preparing the registration.
    Prepare,
    /// The registrar's transaction.
    Commit,
    /// Holding what the registered row adopts under its record.
    Adopt,
    /// Reading the registered process back for the receipt.
    Observe,
}

/// How long a process start spent in one phase, in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessStartPhaseTime {
    pub phase: ProcessStartPhase,
    pub elapsed_us: u64,
}

/// Which caller context a producer span takes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "context",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TraceCaller {
    /// The caller's ambient context, captured once, when the host call
    /// opens its producer.
    #[default]
    Ambient,
    /// A context the host chose (`None`: detached, a trace root); the
    /// ambient one is never consulted.
    Explicit(Option<TraceCarrier>),
}

/// The per-call trace choices of a producer entry: its caller context and
/// how its producer relates to it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceCall {
    pub caller: TraceCaller,
    pub link_mode: TraceLinkMode,
}

impl TraceCall {
    /// The producer of a `kind` host call making these choices, which knows
    /// `ids` at open, its ambient caller captured through `identity` now.
    pub fn producer(
        &self,
        kind: ProducerKind,
        identity: &dyn TraceIdentity,
        ids: crate::TraceIds,
    ) -> ProducerSpec {
        ProducerSpec {
            kind,
            caller: match &self.caller {
                TraceCaller::Ambient => identity.ambient(),
                TraceCaller::Explicit(caller) => caller.clone(),
            },
            link_mode: self.link_mode,
            ids,
        }
    }
}

/// What a producer is opened from: the host call it stands for, its
/// caller's context (the explicit one, else the ambient one, captured once),
/// how it relates to that caller, and the stable ids known at open: the
/// session, and the turn id when the host chose one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProducerSpec {
    pub kind: ProducerKind,
    pub caller: Option<TraceCarrier>,
    pub link_mode: TraceLinkMode,
    pub ids: crate::TraceIds,
}

/// The durable owner of a trace scope: the admitted thing whose work the
/// scope parents.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TraceScopeOwner {
    /// One physical agent turn, including a follow-on within a run.
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
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
    Turn,
    Tool,
    ToolIntent,
    Process,
}

impl TraceScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
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
            TraceScopeOwner::Turn { .. } => TraceScopeKind::Turn,
            TraceScopeOwner::Tool { .. } => TraceScopeKind::Tool,
            TraceScopeOwner::ToolIntent { .. } => TraceScopeKind::ToolIntent,
            TraceScopeOwner::Process { .. } => TraceScopeKind::Process,
        }
    }
}

/// The retained telemetry of one admitted scope: what caused it, the
/// fan-in it links, the span lash derived for it, and when it started.
///
/// It is written once, by the admission that inserts its owner, in the same
/// write. Every later admission of that owner reads it back unchanged. A
/// scope with no cause is unobserved: it has no span, and its records carry
/// no position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DurableTraceScope {
    pub scope: TraceScopeId,
    /// The parent span: the first writer's.
    #[serde(default, skip_serializing_if = "TraceCause::is_none")]
    pub cause: TraceCause,
    /// The admitted members beyond the parent: the other inputs' producers
    /// of a fan-in turn, in admission order.
    #[serde(default, skip_serializing_if = "TraceLinks::is_empty")]
    pub links: TraceLinks,
    /// This scope's own span, `derive_span(parent, Scope{scope})`: `None`
    /// exactly when `cause` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<TraceCarrier>,
    /// Wall-clock epoch milliseconds of the first admission.
    pub started_at_ms: u64,
}

impl DurableTraceScope {
    /// The scope `scope` admits under `parent`, linking `links`: its span is
    /// derived from the parent and the scope's identity, so any owner
    /// computes the same one from retained data. With no parent it is
    /// unobserved and links nothing: there is no way to make an observed
    /// scope without one.
    pub fn admit(
        scope: TraceScopeId,
        parent: Option<&TraceParent>,
        links: TraceLinks,
        started_at_ms: u64,
    ) -> Self {
        let span =
            parent.map(|parent| derive_span(parent.carrier(), &SpanKey::Scope { scope: &scope }));
        Self {
            scope,
            cause: TraceCause::from(parent.cloned()),
            links: if parent.is_some() {
                links
            } else {
                TraceLinks::new()
            },
            span,
            started_at_ms,
        }
    }

    /// The parent a child of this scope is admitted under: its span. An
    /// unobserved scope gives its children none, so they are unobserved.
    pub fn as_parent(&self) -> Option<TraceParent> {
        self.span.clone().map(TraceParent)
    }

    /// The cause, links and span this scope retains, as an offer: what a
    /// typed handler input carries for a scope its store already admitted.
    pub fn offer(&self) -> TraceScopeOffer {
        TraceScopeOffer::new(self.cause.clone(), self.links.clone(), self.span.clone())
    }
}

/// What an admission offers its store for the scope the store may insert:
/// the cause the caller was given, the links it admits and the span the
/// caller derived for the scope.
///
/// The store that inserts the owning row builds the [`DurableTraceScope`]
/// from it, with the owner id and start time that same write records. A
/// store that finds the row already admitted ignores the offer and returns
/// the retained scope. The store never derives a span. Like every trace
/// field, an offer is no part of any business identity.
///
/// An empty offer (no cause, no links, no span) is one word wide, so the
/// requests and commands that carry one pay nothing for the common
/// unobserved case.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TraceScopeOfferWire", into = "TraceScopeOfferWire")]
pub struct TraceScopeOffer(Option<Box<TraceScopeOfferWire>>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TraceScopeOffer")]
struct TraceScopeOfferWire {
    #[serde(default, skip_serializing_if = "TraceCause::is_none")]
    cause: TraceCause,
    #[serde(default, skip_serializing_if = "TraceLinks::is_empty")]
    links: TraceLinks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    span: Option<TraceCarrier>,
}

static NO_CAUSE: TraceCause = TraceCause::None;

impl TraceScopeOffer {
    pub fn new(cause: TraceCause, links: TraceLinks, span: Option<TraceCarrier>) -> Self {
        if cause.is_none() && links.is_empty() && span.is_none() {
            return Self(None);
        }
        Self(Some(Box::new(TraceScopeOfferWire { cause, links, span })))
    }

    /// The offer of a scope caused by `parent`, linking nothing, whose span
    /// its admission derives once its id is known.
    pub fn caused_by(parent: Option<TraceParent>) -> Self {
        Self::new(parent.into(), TraceLinks::new(), None)
    }

    /// The offer of `scope` admitted under `parent`, linking nothing: the
    /// cause, and the span derived from it.
    pub fn admitting(scope: &TraceScopeId, parent: Option<&TraceParent>) -> Self {
        DurableTraceScope::admit(scope.clone(), parent, TraceLinks::new(), 0).offer()
    }

    /// The cause the caller was given.
    pub fn cause(&self) -> &TraceCause {
        self.0.as_ref().map_or(&NO_CAUSE, |offer| &offer.cause)
    }

    /// The fan-in the caller admits beyond its parent.
    pub fn links(&self) -> TraceLinks {
        self.0
            .as_ref()
            .map_or_else(TraceLinks::new, |offer| offer.links.clone())
    }

    /// The span the caller derived for the scope.
    pub fn span(&self) -> Option<&TraceCarrier> {
        self.0.as_ref().and_then(|offer| offer.span.as_ref())
    }

    /// This offer for `scope`, its span derived from its cause: what an
    /// admission whose id is known only once it is prepared retains.
    #[must_use]
    pub fn deriving(self, scope: &TraceScopeId) -> Self {
        let TraceScopeOfferWire { cause, links, .. } = self.into();
        let span = cause
            .as_parent()
            .map(|parent| derive_span(parent.carrier(), &SpanKey::Scope { scope }));
        Self::new(cause, links, span)
    }

    /// Whether the offer says nothing: no cause, no links and no span.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// The scope the inserting admission retains for `scope`, started at
    /// `started_at_ms`: its span is derived from the offer's cause and
    /// `scope`, whatever span the offer carried, so a scope's span is a
    /// pure function of what it retains.
    pub fn into_scope(self, scope: TraceScopeId, started_at_ms: u64) -> DurableTraceScope {
        let TraceScopeOfferWire { cause, links, .. } =
            self.0.map(|offer| *offer).unwrap_or_default();
        DurableTraceScope::admit(scope, cause.as_parent().as_ref(), links, started_at_ms)
    }
}

impl From<TraceScopeOfferWire> for TraceScopeOffer {
    fn from(wire: TraceScopeOfferWire) -> Self {
        Self::new(wire.cause, wire.links, wire.span)
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
            crate::TraceTimestamp::from_system_time(std::time::SystemTime::now())
                .unwrap_or(crate::TraceTimestamp::from_unix_ms(0)),
        )
    }

    /// A record whose id is its [`TraceRecordIdentity`] and whose timestamp is
    /// the retained time of the fact it reports, so reconstructing the fact
    /// rebuilds the same record. It carries `event` as built, with no
    /// position ([`Self::governed`] applies a content policy).
    pub fn identified(
        identity: &TraceRecordIdentity,
        context: crate::TraceContext,
        event: crate::TraceEvent,
        timestamp: crate::TraceTimestamp,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            schema_version: crate::TRACE_SCHEMA_VERSION,
            id: identity.record_id()?,
            timestamp,
            content: crate::ContentPolicy::captured().into(),
            context,
            position: None,
            omissions: crate::TraceOmissionCounts::default(),
            event,
        })
    }

    /// This record at `position`.
    #[must_use]
    pub fn at(mut self, position: Option<crate::TracePosition>) -> Self {
        self.position = position;
        self
    }

    /// This record under the host's content policy: as built when content is
    /// captured, and with fields emptied and marked for omitted categories.
    /// Uses the same exhaustive visitor and bounds as the runtime. Every record the runtime hands a
    /// sink or the adapter passes through here.
    #[must_use]
    pub fn governed(mut self, content: crate::ContentPolicy) -> Self {
        self.apply_content(
            &crate::TracePolicySnapshot {
                content,
                ..Default::default()
            },
            None,
        );
        self
    }
}

/// The durable-substrate attempt a handler is executing under, as that
/// substrate's transport delivered it.
///
/// It is an optional capability of the substrate seam: a substrate with no
/// attempt notion reports none. A record it makes carries it in its
/// position, and its span links the attempt's context; it never becomes an
/// operation's parent and is never stored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceAttemptLink {
    /// The attempt span the substrate injected, absent when its tracing is
    /// off or the header was malformed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<TraceCarrier>,
    /// The substrate's invocation id, when the delivery names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
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

mod identity;
pub use identity::{
    LashIdentity, ProducerOutcome, ProducerSpan, SpanKey, TraceIdentity, derive_span,
};

#[cfg(test)]
mod tests;

/// Injected operational instruments, with no-op handles when telemetry is absent.
#[path = "otel/metrics.rs"]
pub mod metrics;
