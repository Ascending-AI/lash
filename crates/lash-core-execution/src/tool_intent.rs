/// version_surface = "coexist"
/// version_guard(items(LASH_TOOL_INTENT_PAYLOAD_DOMAIN_VERSION, new))
const LASH_TOOL_INTENT_PAYLOAD_DOMAIN_VERSION: &str = "lash-tool-intent-payload/v3";

use crate::ProcessId;
use crate::RuntimeOwner;
use crate::{ToolIntentIdentity, ToolIntentKind};
use serde::{Deserialize, Serialize};

/// The only intent-to-command protocol understood by this build.
///
/// Version 3 replaces the start declaration's full `ProcessStartRequest` with
/// an id-less [`crate::ProcessStartDeclaration`]: the process id is derived
/// from the declaring attempt's intent identity instead of being carried and
/// then overwritten. A v2 batch or durable submission row therefore decodes to
/// a shape this build cannot realize, so both are refused before any
/// declaration effect rather than reinterpreted.
/// **Integrator class 3: protocol and process-engine implementors.**
/// version_surface = "coexist"
pub const TOOL_INTENT_PROTOCOL_V3: u16 = 3;
pub const TOOL_INTENT_MAX_COUNT: usize = 32;
/// Maximum canonical JSON bytes one recorded intent batch may declare.
/// Captured environments are stored separately; their digest references count.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_CANONICAL_BYTES: usize = 64 * 1024;
pub const TOOL_INTENT_MAX_PER_KIND: usize = 16;

/// Deployment admission bounds for one tool's declared intent batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolIntentLimits {
    max_count: usize,
    max_canonical_bytes: usize,
    max_per_kind: usize,
}
impl ToolIntentLimits {
    /// Count cannot exceed the u32 intent-index representation; bytes are host policy.
    pub fn new(max_count: usize, max_canonical_bytes: usize, max_per_kind: usize) -> Option<Self> {
        u32::try_from(max_count).ok().map(|_| Self {
            max_count,
            max_canonical_bytes,
            max_per_kind,
        })
    }
    pub const fn max_count(self) -> usize {
        self.max_count
    }
    pub const fn max_canonical_bytes(self) -> usize {
        self.max_canonical_bytes
    }
    pub const fn max_per_kind(self) -> usize {
        self.max_per_kind
    }
    /// Historical defaults: 32 intents, 64 KiB canonical JSON, 16 per kind.
    pub const fn standard() -> Self {
        Self {
            max_count: TOOL_INTENT_MAX_COUNT,
            max_canonical_bytes: TOOL_INTENT_MAX_CANONICAL_BYTES,
            max_per_kind: TOOL_INTENT_MAX_PER_KIND,
        }
    }
}

/// Recorded declarations returned by a leaf tool attempt.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolIntents {
    /// Version selecting the literal admission and realization contract.
    pub protocol_version: u16,
    /// Ordered declarations whose indexes participate in durable identity.
    pub intents: Vec<ToolIntent>,
}

impl Default for ToolIntents {
    fn default() -> Self {
        Self::v3(Vec::new())
    }
}

impl ToolIntents {
    pub fn v3(intents: Vec<ToolIntent>) -> Self {
        Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V3,
            intents,
        }
    }

    /// This is an **integrator class 3: protocol and process-engine implementor** seam.
    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }

    /// The canonical JSON size of the complete declaration batch.
    pub fn declared_canonical_bytes(&self) -> Result<usize, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| bytes.len())
    }
}

impl ToolIntent {
    /// The environment the declaration's launch names, without its stored
    /// bytes.
    pub fn env_ref(&self) -> Option<&crate::ProcessEnvRef> {
        match self {
            Self::StartProcess(start) => {
                start.declaration.input().launch().map(|launch| &launch.env)
            }
            Self::PublishDefinition(_) | Self::GetDefinition(_) | Self::CancelProcess(_) => None,
        }
    }
}

/// The payload type each generated [`ToolIntent`] variant carries.
///
/// One arm per variant of [`lash_protocol_types::tool_intent_variants!`]: a variant
/// added to that list without a payload here is a compile error, so the enum
/// and the kind set cannot diverge.
macro_rules! tool_intent_payload {
    (StartProcess) => { Box<StartProcessIntent> };
    (CancelProcess) => { CancelProcessIntent };
    (GetDefinition) => { GetDefinitionIntent };
    (PublishDefinition) => { Box<PublishDefinitionIntent> };
}

/// The stored form each [`ToolIntent`] variant decodes from: its payload,
/// except a start's, which decodes through the crate-private
/// [`StartProcessIntentWire`].
macro_rules! tool_intent_wire_payload {
    (StartProcess) => { Box<StartProcessIntentWire> };
    (CancelProcess) => { CancelProcessIntent };
    (GetDefinition) => { GetDefinitionIntent };
    (PublishDefinition) => { Box<PublishDefinitionIntent> };
}

macro_rules! define_tool_intent {
    ($($variant:ident $wire:literal,)*) => {
        /// Durable follow-on work a recorded leaf attempt may request.
        ///
        /// Generated from [`lash_protocol_types::tool_intent_variants!`] together with
        /// [`ToolIntentKind`], so `kind()` is a projection of one list rather
        /// than a hand-kept mirror.
        /// **Integrator class 3: protocol and process-engine implementors.**
        #[derive(Clone, Debug, Serialize, Deserialize)]
        #[serde(
            tag = "kind",
            content = "intent",
            rename_all = "snake_case",
            from = "ToolIntentWire"
        )]
        pub enum ToolIntent {
            $($variant(tool_intent_payload!($variant)),)*
        }

        /// What a [`ToolIntent`] decodes from.
        #[derive(Deserialize)]
        #[serde(tag = "kind", content = "intent", rename_all = "snake_case")]
        enum ToolIntentWire {
            $($variant(tool_intent_wire_payload!($variant)),)*
        }

        impl From<ToolIntentWire> for ToolIntent {
            fn from(wire: ToolIntentWire) -> Self {
                match wire {
                    $(ToolIntentWire::$variant(payload) => Self::$variant(payload.into()),)*
                }
            }
        }

        impl ToolIntent {
            pub fn kind(&self) -> ToolIntentKind {
                match self {
                    $(Self::$variant(_) => ToolIntentKind::$variant,)*
                }
            }

            /// The runtime whose authority declared this intent.
            pub fn owner(&self) -> &RuntimeOwner {
                match self {
                    $(Self::$variant(intent) => &intent.owner,)*
                }
            }
        }
    };
}

lash_protocol_types::tool_intent_variants!(define_tool_intent);

/// Durable submission-ledger row for one host-submitted tool-intent identity.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam. Process registries persist it so independent facade handles and
/// crash redrives see the same first writer: a cancel binds its target here
/// before realization, and every submission retains its first outcome here.
///
/// The row is a replay envelope (protocol, identity, command kind, payload
/// hash, trace) and a first settlement, both kept while the row lives, beside
/// the first payload, which follows its owning run's revision: once a
/// settled submission's run published a revision its session no longer
/// retains, [`Self::intent`] reads [`ToolIntentSubmissionPayload::Expired`]
/// (FIG-6612). Replay and changed-content refusal read only the envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "ToolIntentSubmissionWire")]
pub struct ToolIntentSubmissionRecord {
    /// Version selecting the admission and realization contract.
    pub protocol_version: u16,
    /// Canonical `(session, scope, call, index)` identity and replay key.
    pub identity: ToolIntentIdentity,
    /// The command kind the first payload declared, kept beside it so it
    /// outlives the payload.
    kind: ToolIntentKind,
    /// Hash of the first serialized payload.
    pub payload_hash: String,
    /// First payload retained for crash redrive, while its run's revision is
    /// retained.
    pub intent: ToolIntentSubmissionPayload,
    /// First typed realization outcome, absent while admission is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<ToolIntentSubmissionSettlement>,
    /// The submission's trace scope: the cause and span its first
    /// submission offered and when it was made. The ledger's first writer
    /// retains it with the row; a later submission of the identity reads it
    /// back. It is beside the intent, never inside it, so the payload hash
    /// does not cover it. `None` for a runtime-minted intent, which runs
    /// under its tool call's scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<lash_trace::DurableTraceScope>,
    /// The logical run of the owner session the submission was made under,
    /// when its execution scope named one: the run whose published revision
    /// retains the payload. `None` keeps the payload while the row lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_run: Option<crate::TurnId>,
}

/// A submission's first payload: retained, or expired with the revision of
/// the run that owned it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "intent", rename_all = "snake_case")]
pub enum ToolIntentSubmissionPayload {
    Retained(ToolIntent),
    /// The submission settled and its owning run's revision was released
    /// under its session's retention: only the envelope and the first
    /// settlement remain.
    Expired,
}

impl ToolIntentSubmissionPayload {
    /// The payload, while it is retained.
    pub fn retained(&self) -> Option<&ToolIntent> {
        match self {
            Self::Retained(intent) => Some(intent),
            Self::Expired => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolIntentSubmissionWire {
    protocol_version: u16,
    identity: ToolIntentIdentity,
    kind: ToolIntentKind,
    payload_hash: String,
    intent: ToolIntentSubmissionPayload,
    settlement: Option<ToolIntentSubmissionSettlement>,
    trace: Option<lash_trace::DurableTraceScope>,
    owning_run: Option<crate::TurnId>,
}

impl TryFrom<ToolIntentSubmissionWire> for ToolIntentSubmissionRecord {
    type Error = crate::PluginError;

    fn try_from(row: ToolIntentSubmissionWire) -> Result<Self, Self::Error> {
        let record = Self {
            protocol_version: row.protocol_version,
            identity: row.identity,
            kind: row.kind,
            payload_hash: row.payload_hash,
            intent: row.intent,
            settlement: row.settlement,
            trace: row.trace,
            owning_run: row.owning_run,
        };
        record.validate()?;
        Ok(record)
    }
}

/// The replay envelope a ledger row stores for as long as the row lives.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolIntentSubmissionEnvelope {
    protocol_version: u16,
    identity: ToolIntentIdentity,
    kind: ToolIntentKind,
    payload_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trace: Option<lash_trace::DurableTraceScope>,
}

/// The stored columns of one submission-ledger row.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam: a SQL process registry stores the envelope, the first payload and the
/// first settlement as separate columns, so completing a row writes only its
/// settlement and expiring it clears only its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolIntentLedgerColumns {
    /// The replay envelope: protocol, identity, command kind, payload hash
    /// and trace.
    pub envelope_json: String,
    /// The first payload; `None` once it expired.
    pub intent_json: Option<String>,
    /// The first settlement; `None` while the submission is pending.
    pub settlement_json: Option<String>,
    /// The session and logical run that own the payload, both or neither.
    pub owning_run: Option<(crate::SessionId, crate::TurnId)>,
}

/// One completed submission: its instant and outcome are inseparable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolIntentSubmissionSettlement {
    pub at_ms: u64,
    pub outcome: ToolIntentSubmissionOutcome,
}

/// A per-identity result holds neither a second identity nor a batch refusal.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "result",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ToolIntentSubmissionOutcome {
    Executed(crate::ToolIntentRealized),
    Refused(crate::ToolIntentRefusalReason),
}

impl ToolIntentSubmissionOutcome {
    fn try_from_execution(
        outcome: crate::ToolIntentExecutionOutcome,
        identity: &ToolIntentIdentity,
        kind: ToolIntentKind,
    ) -> Result<Self, crate::PluginError> {
        use crate::ToolIntentExecutionOutcome as Answer;
        match outcome {
            Answer::Executed {
                identity: recorded,
                realized,
            } if &recorded == identity && realized.kind() == kind => Ok(Self::Executed(realized)),
            Answer::Refused {
                identity: Some(recorded),
                intent_index,
                kind: recorded_kind,
                refusal,
            } if &recorded == identity
                && intent_index == identity.intent_index
                && recorded_kind == kind =>
            {
                Ok(Self::Refused(refusal))
            }
            _ => Err(crate::durable_identity_conflict(
                "submission settlement must name its retained identity and command kind",
            )),
        }
    }
}

impl ToolIntentSubmissionRecord {
    /// Builds the canonical first-submission row for protocol and
    /// process-engine implementors before any intent realization occurs.
    pub fn new(
        identity: ToolIntentIdentity,
        intent: ToolIntent,
    ) -> Result<Self, serde_json::Error> {
        // The hash is the intent's business identity.
        let payload_hash = crate::stable_hash::blake3_hex(
            LASH_TOOL_INTENT_PAYLOAD_DOMAIN_VERSION,
            &serde_json::to_vec(&intent)?,
        );
        Ok(Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V3,
            identity,
            kind: intent.kind(),
            payload_hash,
            intent: ToolIntentSubmissionPayload::Retained(intent),
            settlement: None,
            trace: None,
            owning_run: None,
        })
    }

    /// The same submission made under `run`, a logical run of its owner
    /// session: once the submission settles, its payload follows the
    /// revision that run publishes.
    #[must_use]
    pub fn owned_by_run(mut self, run: crate::TurnId) -> Self {
        self.owning_run = Some(run);
        self
    }

    /// The command kind the first payload declared.
    pub fn kind(&self) -> ToolIntentKind {
        self.kind
    }

    /// Refuse a malformed per-identity settlement at admission and decode.
    pub fn validate_settlement(&self) -> Result<(), crate::PluginError> {
        if let Some(ToolIntentSubmissionSettlement {
            outcome: ToolIntentSubmissionOutcome::Executed(realized),
            ..
        }) = &self.settlement
            && realized.kind() != self.kind
        {
            return Err(crate::durable_identity_conflict(
                "submission realization differs from its retained command kind",
            ));
        }
        Ok(())
    }

    /// Refuse a row whose parts disagree: a payload of another kind, an
    /// expired payload that never settled, or a run owned by no session.
    fn validate(&self) -> Result<(), crate::PluginError> {
        self.validate_settlement()?;
        match &self.intent {
            ToolIntentSubmissionPayload::Retained(intent) if intent.kind() != self.kind => {
                return Err(crate::durable_identity_conflict(
                    "submission payload differs from its retained command kind",
                ));
            }
            ToolIntentSubmissionPayload::Expired if self.settlement.is_none() => {
                return Err(crate::durable_identity_conflict(
                    "a pending submission's payload cannot expire",
                ));
            }
            _ => {}
        }
        if self.owning_run.is_some() && !matches!(self.identity.owner, RuntimeOwner::Session(_)) {
            return Err(crate::durable_identity_conflict(
                "only a session-owned submission names an owning run",
            ));
        }
        Ok(())
    }

    /// The stored columns of this row.
    ///
    /// # Errors
    ///
    /// A row whose parts disagree, or that fails to encode.
    pub fn ledger_columns(&self) -> Result<ToolIntentLedgerColumns, crate::PluginError> {
        self.validate()?;
        let encode = |error: serde_json::Error| {
            crate::PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::RecordEncodingFailed,
                format!("failed to encode a tool-intent submission row: {error}"),
            ))
        };
        let envelope_json = serde_json::to_string(&ToolIntentSubmissionEnvelope {
            protocol_version: self.protocol_version,
            identity: self.identity.clone(),
            kind: self.kind,
            payload_hash: self.payload_hash.clone(),
            trace: self.trace.clone(),
        })
        .map_err(encode)?;
        let intent_json = self
            .intent
            .retained()
            .map(serde_json::to_string)
            .transpose()
            .map_err(encode)?;
        let settlement_json = self
            .settlement
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(encode)?;
        let owning_run = match (&self.identity.owner, &self.owning_run) {
            (RuntimeOwner::Session(session), Some(run)) => Some((session.clone(), run.clone())),
            _ => None,
        };
        Ok(ToolIntentLedgerColumns {
            envelope_json,
            intent_json,
            settlement_json,
            owning_run,
        })
    }

    /// The row the stored `columns` hold.
    ///
    /// # Errors
    ///
    /// Columns that do not decode, or whose parts disagree.
    pub fn from_ledger_columns(
        columns: ToolIntentLedgerColumns,
    ) -> Result<Self, crate::PluginError> {
        let decode = |error: serde_json::Error| {
            crate::PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::RecordEncodingFailed,
                format!("failed to decode a tool-intent submission row: {error}"),
            ))
        };
        let envelope: ToolIntentSubmissionEnvelope =
            serde_json::from_str(&columns.envelope_json).map_err(decode)?;
        let intent = match columns.intent_json {
            Some(intent) => ToolIntentSubmissionPayload::Retained(
                serde_json::from_str(&intent).map_err(decode)?,
            ),
            None => ToolIntentSubmissionPayload::Expired,
        };
        let settlement = columns
            .settlement_json
            .map(|settlement| serde_json::from_str(&settlement))
            .transpose()
            .map_err(decode)?;
        let owning_run = match columns.owning_run {
            Some((session, run)) if matches!(&envelope.identity.owner, RuntimeOwner::Session(owner) if *owner == session) => {
                Some(run)
            }
            Some(_) => {
                return Err(crate::durable_identity_conflict(
                    "a submission's owning run belongs to another session",
                ));
            }
            None => None,
        };
        let record = Self {
            protocol_version: envelope.protocol_version,
            identity: envelope.identity,
            kind: envelope.kind,
            payload_hash: envelope.payload_hash,
            intent,
            settlement,
            trace: envelope.trace,
            owning_run,
        };
        record.validate()?;
        Ok(record)
    }

    /// Rebuild the external answer using this row's identity and kind.
    pub fn execution_outcome(&self) -> Option<crate::ToolIntentExecutionOutcome> {
        use crate::ToolIntentExecutionOutcome as Answer;
        self.settlement
            .as_ref()
            .map(|settlement| match &settlement.outcome {
                ToolIntentSubmissionOutcome::Executed(realized) => Answer::Executed {
                    identity: self.identity.clone(),
                    realized: realized.clone(),
                },
                ToolIntentSubmissionOutcome::Refused(refusal) => Answer::Refused {
                    identity: Some(self.identity.clone()),
                    intent_index: self.identity.intent_index,
                    kind: self.kind,
                    refusal: refusal.clone(),
                },
            })
    }

    /// Validate the answer at the ledger boundary before keeping the first settlement.
    pub fn complete(
        &mut self,
        at_ms: u64,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<bool, crate::PluginError> {
        self.validate_settlement()?;
        let outcome =
            ToolIntentSubmissionOutcome::try_from_execution(outcome, &self.identity, self.kind)?;
        if self.settlement.is_some() {
            return Ok(false);
        }
        self.settlement = Some(ToolIntentSubmissionSettlement { at_ms, outcome });
        Ok(true)
    }

    /// The scope id of this submission's trace scope: its owning runtime
    /// and replay key.
    pub fn trace_scope_id(&self) -> lash_trace::TraceScopeId {
        lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::ToolIntent {
            owner: self.identity.owner.clone(),
            replay_key: self.identity.replay_key.clone(),
        })
    }

    /// Offers `offer` as a host submission's trace scope, started at
    /// `submitted_at_ms`. The ledger keeps it only when this record is the
    /// identity's first writer.
    pub fn with_trace_offer(
        mut self,
        offer: lash_trace::TraceScopeOffer,
        submitted_at_ms: u64,
    ) -> Self {
        self.trace = Some(offer.into_scope(self.trace_scope_id(), submitted_at_ms));
        self
    }
}

/// Atomic result of claiming a tool-intent identity in the submission ledger.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam returned by [`crate::ProcessRegistry`] implementations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolIntentSubmissionAdmission {
    /// This caller durably installed the first submission.
    Admitted,
    /// Another caller or an earlier crash installed the returned first submission.
    Existing(Box<ToolIntentSubmissionRecord>),
    /// The identity's owner session was durably deleted and the retained-evidence
    /// lever reclaimed its ledger (FIG-1509). The owner's fence outlives its
    /// rows, so nothing may claim or realize an identity of that owner again.
    Reclaimed,
}

#[derive(Clone, Debug, Serialize)]
/// The declaration carries no process id and no key. Realization derives the
/// key with [`crate::StartKeyDerivation::for_tool_intent`] from this declaration's own
/// intent identity, so every redrive presents the same key and starts the
/// same process; the registrar mints the id and the realized result carries
/// it (FIG-2994, ADR 0107).
pub struct StartProcessIntent {
    /// The runtime whose authority owns the child.
    pub owner: RuntimeOwner,
    /// Durable process-start declaration, minus the derived key.
    pub declaration: crate::ProcessStartDeclaration,
}

/// The stored form of a [`StartProcessIntent`]: it decodes only inside a
/// [`ToolIntent`] or a [`DeclaredStart`](crate::DeclaredStart), whose
/// declaration the runtime checks before it records or realizes it.
#[derive(Deserialize)]
pub(crate) struct StartProcessIntentWire {
    owner: RuntimeOwner,
    declaration: crate::runtime::process::model::ProcessStartDeclarationWire,
}

impl From<StartProcessIntentWire> for StartProcessIntent {
    fn from(wire: StartProcessIntentWire) -> Self {
        Self {
            owner: wire.owner,
            declaration: wire.declaration.into(),
        }
    }
}

impl From<Box<StartProcessIntentWire>> for Box<StartProcessIntent> {
    fn from(wire: Box<StartProcessIntentWire>) -> Self {
        Box::new((*wire).into())
    }
}

impl StartProcessIntent {
    /// Both realization routes call this and nothing else, so every redrive
    /// of one declaration presents the same start key (ADR 0107).
    pub fn into_request(&self, identity: &ToolIntentIdentity) -> crate::ProcessStartRequest {
        self.declaration
            .clone()
            .into_request(crate::StartKeyDerivation::LASH_START_PATHS.for_tool_intent(identity))
    }
}

/// Publication of an immutable descriptor after the attempt commits.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishDefinitionIntent {
    pub owner: RuntimeOwner,
    pub draft: crate::ProcessDefinitionDraft,
}

/// Acquire a definition under the realizing execution before returning it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetDefinitionIntent {
    pub owner: RuntimeOwner,
    pub definition_id: crate::ProcessDefinitionId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Cancellation declaration consumed by protocol and process-engine implementors.
#[serde(deny_unknown_fields)]
pub struct CancelProcessIntent {
    /// The runtime whose authority owns the cancellation.
    pub owner: RuntimeOwner,
    /// Target process id.
    pub process_id: ProcessId,
}

/// version_surface = "coexist"
/// version_guard(items(TOOL_INTENT_IDENTITY_FAMILY_VERSION, derive_tool_intent_identity_inner))
const TOOL_INTENT_IDENTITY_FAMILY_VERSION: u8 = 2;

/// The public identity seam for host-submitted intents.
///
/// Runtime-minted declarations use the same v2 family through
/// [`derive_tool_intent_identity_under`], which additionally binds the identity
/// to the durable invocation that minted the declaration.
pub fn derive_tool_intent_identity(
    owner: &RuntimeOwner,
    execution_scope_id: &str,
    tool_call_id: &crate::ToolCallId,
    intent_index: u32,
) -> ToolIntentIdentity {
    derive_tool_intent_identity_inner(owner, execution_scope_id, tool_call_id, intent_index, None)
}

/// The one derivation every runtime-side declaration site uses.
///
/// An intent is named by the call that declared it and its index among the
/// call's intents (ADR 0117 §6). A declaration minted under a durable
/// invocation also binds that invocation's replay key, the final-emission
/// attribution that fences it; one minted outside any invocation binds
/// nothing. Both the attempt's own `AttemptContext::intent_identity` and the
/// intent executor's realization pass their parent invocation here, so the
/// identity an attempt reports and the identity the executor realizes under
/// cannot be derived by two rules (FIG-2994).
pub fn derive_tool_intent_identity_under(
    owner: &RuntimeOwner,
    execution_scope_id: &str,
    tool_call_id: &crate::ToolCallId,
    intent_index: u32,
    parent_invocation: Option<&crate::RuntimeInvocation>,
) -> ToolIntentIdentity {
    derive_tool_intent_identity_inner(
        owner,
        execution_scope_id,
        tool_call_id,
        intent_index,
        parent_invocation.and_then(crate::RuntimeInvocation::effect_replay_key),
    )
}

fn derive_tool_intent_identity_inner(
    owner: &RuntimeOwner,
    execution_scope_id: &str,
    tool_call_id: &crate::ToolCallId,
    intent_index: u32,
    minting_emission_replay_key: Option<&str>,
) -> ToolIntentIdentity {
    let mut encoder = crate::stable_identity::IdentityEncoder::new(
        "lash.tool-intent",
        TOOL_INTENT_IDENTITY_FAMILY_VERSION,
    );
    encode_owner(&mut encoder, owner);
    encoder.string(execution_scope_id);
    encoder.string(tool_call_id.as_str());
    encoder.u32(intent_index);
    encoder.optional(minting_emission_replay_key, |encoder, replay_key| {
        encoder.string(replay_key)
    });
    let replay_key = crate::stable_identity::rendered_hash(
        "tool-intent",
        TOOL_INTENT_IDENTITY_FAMILY_VERSION,
        &encoder.finish(),
    );
    ToolIntentIdentity {
        owner: owner.clone(),
        execution_scope_id: execution_scope_id.to_string(),
        tool_call_id: tool_call_id.clone(),
        intent_index,
        replay_key,
        minting_emission_replay_key: minting_emission_replay_key.map(str::to_string),
    }
}

/// Re-derive an identity from its own durable fields.
///
/// The v2 replay key hashes the minting-emission replay key, so the record
/// retains that input; a record whose `replay_key` does not equal the
/// re-derived one carries a forged or corrupted identity.
pub fn rederive_tool_intent_identity(identity: &ToolIntentIdentity) -> ToolIntentIdentity {
    derive_tool_intent_identity_inner(
        &identity.owner,
        &identity.execution_scope_id,
        &identity.tool_call_id,
        identity.intent_index,
        identity.minting_emission_replay_key.as_deref(),
    )
}

/// The owner's kind, then its id: a session named like a process id can
/// never share an identity with that process.
fn encode_owner(encoder: &mut crate::stable_identity::IdentityEncoder, owner: &RuntimeOwner) {
    match owner {
        RuntimeOwner::Session(session_id) => {
            encoder.string("session");
            encoder.string(session_id);
        }
        RuntimeOwner::Process(process_id) => {
            encoder.string("process");
            encoder.string(process_id.as_str());
        }
    }
}

/// A completed leaf-provider value. Unlike [`crate::ToolOutcome`], this type has
/// no deferred variant, so a completed result can be paired with intents
/// without making `Pending + intents` representable.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcomeDone(Box<DoneParts>);

/// The terminal output and the state commands returned with it, boxed
/// together so a completed value stays one pointer wide.
#[derive(Clone, Debug, PartialEq)]
struct DoneParts {
    output: crate::ToolCallOutput,
    state: crate::plugin::StateCommands,
}

impl ToolOutcomeDone {
    pub fn from_output(output: crate::ToolCallOutput) -> Self {
        Self(Box::new(DoneParts {
            output,
            state: crate::plugin::StateCommands::default(),
        }))
    }

    /// Return `commands` against the tool's owning plugin's namespace with
    /// this result (K10). They publish with the attempt's recorded outcome,
    /// and only when the call's final result, after its result checks, is a
    /// success.
    #[must_use]
    pub fn with_state(mut self, commands: crate::plugin::StateCommands) -> Self {
        self.0.state = commands;
        self
    }

    /// The terminal output and the state commands returned with it.
    pub(crate) fn into_parts(self) -> (crate::ToolCallOutput, crate::plugin::StateCommands) {
        let DoneParts { output, state } = *self.0;
        (output, state)
    }

    pub fn ok(result: serde_json::Value) -> Self {
        Self::from_output(crate::ToolCallOutput::success(result))
    }

    pub fn failure(failure: crate::ToolFailure) -> Self {
        Self::from_output(crate::ToolCallOutput::failure(failure))
    }

    /// Recover the terminal output for protocol and process-engine implementors.
    pub fn into_output(self) -> crate::ToolCallOutput {
        self.0.output
    }
}

/// Value returned by a leaf provider body before the attempt effect records it.
/// The enum is the law: only the completed variant has an intents field.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug)]
pub enum ToolAttemptOutcome {
    /// Terminal provider output with ordered durable declarations.
    Done {
        /// Completed provider result.
        result: ToolOutcomeDone,
        /// Follow-on declarations admitted only after the attempt is recorded.
        intents: ToolIntents,
    },
    /// A host fault that aborts the attempt before it has a recorded result.
    /// Its journal disposition determines whether the engine may retry it.
    HostFailed(Box<crate::RuntimeEffectControllerError>),
    /// Deferred provider output with no representable declarations.
    Pending(crate::PendingCompletion),
}

impl ToolAttemptOutcome {
    /// Pair a completed result with its declarations for protocol and process-engine implementors.
    pub fn done(result: ToolOutcomeDone, intents: ToolIntents) -> Self {
        Self::Done { result, intents }
    }

    pub fn done_without_intents(result: ToolOutcomeDone) -> Self {
        Self::done(result, ToolIntents::default())
    }

    pub fn host_failed(error: crate::RuntimeEffectControllerError) -> Self {
        Self::HostFailed(Box::new(error))
    }

    pub fn pending(pending: crate::PendingCompletion) -> Self {
        Self::Pending(pending)
    }
}

/// Lift a plain tool outcome into an attempt outcome at the execution seam.
/// A completed outcome becomes [`ToolAttemptOutcome::Done`] with no declared
/// intents; a pending outcome stays [`ToolAttemptOutcome::Pending`] with its
/// completion payload intact. There is deliberately no conversion in the
/// other direction: projecting away `Done` intents would silently drop
/// durable declarations.
impl From<crate::ToolOutcome> for ToolAttemptOutcome {
    fn from(result: crate::ToolOutcome) -> Self {
        match result {
            crate::ToolOutcome::Done(output) => {
                Self::done_without_intents(ToolOutcomeDone::from_output(*output))
            }
            crate::ToolOutcome::Pending(pending) => Self::Pending(*pending),
        }
    }
}

impl lash_core_store::store::DurableRecord for ToolIntents {
    const SURFACE: lash_core_store::store::SurfaceFormat =
        lash_core_store::surface_format!(crate::tool_intent::TOOL_INTENT_PROTOCOL_V3);
}

impl lash_core_store::store::DurableRecord for ToolIntentSubmissionRecord {
    const SURFACE: lash_core_store::store::SurfaceFormat =
        lash_core_store::surface_format!(crate::tool_intent::TOOL_INTENT_PROTOCOL_V3);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;

    fn sample_intent(kind: ToolIntentKind) -> ToolIntent {
        let session_id = SessionId::from("session");
        // An exhaustive match, so a variant added to the generated kind set
        // without a matching `ToolIntent` payload fails to compile here.
        match kind {
            ToolIntentKind::StartProcess => {
                ToolIntent::StartProcess(Box::new(StartProcessIntent {
                    owner: crate::RuntimeOwner::Session(session_id),
                    declaration: crate::testing::held_engine_declaration(
                        serde_json::Value::Null,
                        crate::ProcessOriginator::host(),
                        crate::Lifetime::Detached,
                    ),
                }))
            }
            ToolIntentKind::CancelProcess => ToolIntent::CancelProcess(CancelProcessIntent {
                owner: crate::RuntimeOwner::Session(session_id),
                process_id: crate::process_id_for_test("process"),
            }),
            ToolIntentKind::GetDefinition => ToolIntent::GetDefinition(GetDefinitionIntent {
                owner: RuntimeOwner::Session(session_id),
                definition_id: crate::ProcessDefinitionId::from_sha256_digest([0; 32]),
            }),
            ToolIntentKind::PublishDefinition => {
                ToolIntent::PublishDefinition(Box::new(PublishDefinitionIntent {
                    owner: RuntimeOwner::Session(session_id),
                    draft: crate::ProcessDefinitionDraft::new(
                        "engine",
                        serde_json::Value::Null,
                        [],
                    )
                    .expect("draft"),
                }))
            }
        }
    }

    /// Runtime owners of different kinds cannot alias even when their id bytes match.
    #[test]
    fn process_and_session_owners_with_equal_id_bytes_have_distinct_intent_keys() {
        let process = crate::process_id_for_test("shared-owner");
        let session = SessionId::fixture(process.as_str());
        let key = |owner| {
            derive_tool_intent_identity(
                &owner,
                "same-scope",
                &crate::ToolCallId::fixture("same-call"),
                0,
            )
            .replay_key
        };
        assert_ne!(
            key(RuntimeOwner::Process(process)),
            key(RuntimeOwner::Session(session)),
            "the owner kind is part of the intent's idempotency identity",
        );
    }

    #[test]
    fn intent_identity_has_a_literal_stable_oracle() {
        let identity = derive_tool_intent_identity(
            &crate::RuntimeOwner::Session(SessionId::from("session-fig1292")),
            "turn-7",
            &crate::ToolCallId::fixture("call-3"),
            2,
        );
        assert_eq!(
            identity,
            ToolIntentIdentity {
                owner: RuntimeOwner::Session(SessionId::from("session-fig1292")),
                execution_scope_id: "turn-7".to_string(),
                tool_call_id: crate::ToolCallId::fixture("call-3"),
                intent_index: 2,
                replay_key: "tool-intent:v2:blake3:cbd4d128fc11551063487aa7d35bf25457d31f6d9acc95ec90996e74cd102a9d".to_string(),
                minting_emission_replay_key: None,
            }
        );

        assert_eq!(rederive_tool_intent_identity(&identity), identity);
        let forged = ToolIntentIdentity {
            intent_index: 3,
            ..identity
        };
        assert_ne!(
            rederive_tool_intent_identity(&forged).replay_key,
            forged.replay_key,
            "a forged intent_index must not re-derive to the recorded replay key"
        );
    }

    /// The protocol discriminator is part of the row: a submission that
    /// names none is not a row of any protocol and does not decode.
    #[test]
    fn a_submission_row_without_its_protocol_version_fails_decode() {
        let intent = sample_intent(ToolIntentKind::CancelProcess);
        let identity = derive_tool_intent_identity(
            intent.owner(),
            "turn-7",
            &crate::ToolCallId::fixture("call"),
            0,
        );
        let record = ToolIntentSubmissionRecord::new(identity, intent).expect("build the row");
        let mut row = serde_json::to_value(&record).expect("encode the row");
        let decoded: ToolIntentSubmissionRecord =
            serde_json::from_value(row.clone()).expect("the stamped row decodes");
        assert_eq!(decoded.protocol_version, TOOL_INTENT_PROTOCOL_V3);

        row.as_object_mut()
            .expect("the row is an object")
            .remove("protocol_version")
            .expect("the row is stamped");
        let error = serde_json::from_value::<ToolIntentSubmissionRecord>(row)
            .expect_err("an unstamped row must not decode");
        assert!(
            error
                .to_string()
                .contains("missing field `protocol_version`"),
            "{error}"
        );
    }

    #[test]
    fn submission_settlement_refuses_foreign_identity_kind_and_batch_outcomes() {
        let intent = sample_intent(ToolIntentKind::CancelProcess);
        let identity = derive_tool_intent_identity(
            intent.owner(),
            "turn",
            &crate::ToolCallId::fixture("call"),
            0,
        );
        let mut record = ToolIntentSubmissionRecord::new(identity.clone(), intent).expect("row");
        let refused = |identity, kind, intent_index| crate::ToolIntentExecutionOutcome::Refused {
            identity,
            kind,
            intent_index,
            refusal: crate::ToolIntentRefusalReason::IntentIndexOverflow,
        };
        let mut foreign = identity.clone();
        foreign.replay_key.push_str("foreign");
        for answer in [
            refused(Some(foreign), record.kind(), 0),
            refused(Some(identity.clone()), ToolIntentKind::GetDefinition, 0),
            refused(Some(identity.clone()), record.kind(), 1),
            refused(None, record.kind(), 0),
            crate::ToolIntentExecutionOutcome::ProtocolRefused {
                refusal: crate::ToolIntentRefusalReason::IntentIndexOverflow,
            },
        ] {
            let error = record.complete(7, answer).expect_err("foreign settlement");
            assert!(crate::is_durable_identity_conflict(&error));
            assert!(record.settlement.is_none());
        }
        let wrong_realization =
            crate::ToolIntentRealized::CancelProcess(crate::ProcessCancelReceipt {
                process_id: crate::process_id_for_test("process"),
                status: crate::ProcessStatus::Running,
                origin: crate::CancelOrigin::ModelRequested,
            });
        let other_intent = sample_intent(ToolIntentKind::GetDefinition);
        let mut other =
            ToolIntentSubmissionRecord::new(identity.clone(), other_intent).expect("other row");
        other.settlement = Some(ToolIntentSubmissionSettlement {
            at_ms: 7,
            outcome: ToolIntentSubmissionOutcome::Executed(wrong_realization),
        });
        assert!(other.validate_settlement().is_err());
        assert!(
            serde_json::from_value::<ToolIntentSubmissionRecord>(
                serde_json::to_value(&other).expect("invalid bytes")
            )
            .is_err()
        );
        let answer = refused(Some(identity), record.kind(), 0);
        assert!(record.complete(7, answer.clone()).expect("settle"));
        assert!(
            !record
                .complete(9, answer.clone())
                .expect("first settlement wins")
        );
        assert_eq!(record.settlement.as_ref().expect("settled").at_ms, 7);
        assert_eq!(record.execution_outcome(), Some(answer));
        let encoded = serde_json::to_value(&record).expect("encode");
        assert!(encoded.get("completed_at_ms").is_none());
        assert!(encoded["settlement"]["outcome"].get("identity").is_none());
    }
}
