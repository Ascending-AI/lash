//! Durable turn-input vocabulary.
//!
//! The pending-input rows a store persists, their claim and completion
//! payloads, and the checkpoint boundary rule stores filter on. The ingress
//! driver that normalizes and applies them stays in `lash-core`.

pub use crate::run_spec::{
    BindingId, CapabilityRef, ContractRef, DefinitionRef, ResolvedRun, RunDefinition,
    RunDefinitions, RunOverrides, RunShapeError, RunSpec, RunSpecHash, SlotId,
};
use crate::{CheckpointKind, PluginMessage, SessionId, TurnCause, TurnId};
use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// The input id a journaled turn acceptance provisions before its body runs
/// (ADR 0069 §6).
///
/// It is a function of the acceptance effect's address alone, so every
/// execution of the same acceptance, including one re-run because the first
/// one's outcome was never recorded, names the same row, and the store adopts
/// that row instead of admitting a second one. The address is session-scoped
/// and collision-free ([`crate::EffectAddress::graph_key`]), which keeps the id
/// unique across sessions. Format: `ti:<blake3-hex>`, the shape every other
/// pending turn-input id has.
#[must_use]
pub fn provisioned_turn_input_id(acceptance: &crate::EffectAddress) -> String {
    format!(
        "ti:{}",
        crate::stable_hash::blake3_hex(
            "lash-accepted-turn-input/v1",
            acceptance.graph_key().as_bytes(),
        )
    )
}

/// Mint a newly created pending turn-input ID from explicit deterministic facts.
///
/// The stable format is `ti:<blake3-hex>`, where the digest input remains the
/// FIG-886 continuity seed
/// `{session_id}:{source_key:?}:{now_epoch_ms}:{nonce}`. Callers must supply a
/// `(now_epoch_ms, nonce)` pair unique per `(session_id, source_key)` across
/// every process writing the store; existing persisted IDs are never rewritten.
#[must_use]
pub fn derive_pending_turn_input_id(
    session_id: &SessionId,
    source_key: Option<&str>,
    now_epoch_ms: u64,
    nonce: u64,
) -> String {
    format!(
        "ti:{}",
        crate::stable_hash::blake3_hex(
            "lash-turn-input/v2",
            format!("{session_id}:{source_key:?}:{now_epoch_ms}:{nonce}").as_bytes(),
        )
    )
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum TurnInputIngress {
    ActiveTurn {
        turn_id: crate::TurnId,
        #[serde(default)]
        min_boundary: TurnInputCheckpointBoundary,
    },
    NextTurn,
}
impl TurnInputIngress {
    /// Routes an input to an active turn at or after the named checkpoint boundary for turn-input
    /// store implementors.
    pub fn active_turn(
        turn_id: impl Into<crate::TurnId>,
        min_boundary: TurnInputCheckpointBoundary,
    ) -> Self {
        Self::ActiveTurn {
            turn_id: turn_id.into(),
            min_boundary,
        }
    }

    /// Routes an input to the next idle turn for turn-input store implementors; it is never
    /// admitted at an active-turn checkpoint.
    pub fn next_turn() -> Self {
        Self::NextTurn
    }

    pub fn active_turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::ActiveTurn { turn_id, .. } => Some(turn_id),
            Self::NextTurn => None,
        }
    }

    /// Lets turn-input store implementors admit only active-turn ingress whose minimum boundary has
    /// been reached; next-turn ingress never enters a running turn.
    pub fn admits_checkpoint(&self, checkpoint: CheckpointKind) -> bool {
        match self {
            Self::ActiveTurn { min_boundary, .. } => min_boundary.admits(checkpoint),
            Self::NextTurn => false,
        }
    }
}
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputCheckpointBoundary {
    #[default]
    AfterWork,
    BeforeCompletion,
}
impl TurnInputCheckpointBoundary {
    /// Treats `AfterWork` as admitting both checkpoints and `BeforeCompletion` as admitting only
    /// that final checkpoint for turn-input store implementors.
    pub fn admits(self, checkpoint: CheckpointKind) -> bool {
        match self {
            Self::AfterWork => true,
            Self::BeforeCompletion => checkpoint == CheckpointKind::BeforeCompletion,
        }
    }
}
/// The `active_turn` admission scope's payload — carried by every
/// [`TurnInputState`] variant the persisted CHECKs pin to that scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ActiveTurnIngress {
    pub turn_id: crate::TurnId,
    #[serde(default)]
    pub min_boundary: TurnInputCheckpointBoundary,
}
impl From<ActiveTurnIngress> for TurnInputIngress {
    fn from(scope: ActiveTurnIngress) -> Self {
        Self::ActiveTurn {
            turn_id: scope.turn_id,
            min_boundary: scope.min_boundary,
        }
    }
}

/// The scope-free name of a persisted turn-input state.
///
/// This is the `state` column's vocabulary for SQL predicates and decoders;
/// the value type [`TurnInputState`] carries the scope each name is pinned
/// to, so the two can never disagree in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnInputStateKind {
    PendingActive,
    DeferredNextTurn,
    Accepted,
    Cancelled,
    Completed,
}
impl TurnInputStateKind {
    /// Returns whether this state name is settled and eligible for tombstone vacuum.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed)
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::PendingActive | Self::DeferredNextTurn)
    }

    pub fn is_next_turn_pending(self) -> bool {
        matches!(self, Self::DeferredNextTurn)
    }
}

/// A pending input's durable lifecycle state, carrying the admission scope
/// each variant is pinned to.
///
/// The scope lives inside the state so a value cannot disagree with the
/// persisted `ingress_json`/`state` column pair: `pending_active` and
/// `accepted` always carry `active_turn` scope, `deferred_next_turn` is
/// always `next_turn` scope, and the terminal variants keep whichever scope
/// the row was admitted under. The backend `CHECK` constraints remain as a
/// belt-and-braces check on a value the type can no longer contradict.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputState {
    PendingActive(ActiveTurnIngress),
    DeferredNextTurn,
    Accepted(ActiveTurnIngress),
    Cancelled(TurnInputIngress),
    Completed(TurnInputIngress),
}
impl TurnInputState {
    /// The only legal initial durable state for an input admitted under `ingress`.
    #[must_use]
    pub fn open(ingress: TurnInputIngress) -> Self {
        match ingress {
            TurnInputIngress::ActiveTurn {
                turn_id,
                min_boundary,
            } => Self::PendingActive(ActiveTurnIngress {
                turn_id,
                min_boundary,
            }),
            TurnInputIngress::NextTurn => Self::DeferredNextTurn,
        }
    }

    /// Returns `None` for an unknown spelling or a state/scope pair the
    /// persisted CHECKs forbid (`pending_active` under `next_turn` scope,
    /// `deferred_next_turn` under `active_turn` scope, or `accepted` under
    /// `next_turn` scope) — the same disagreement the type refuses to
    /// construct.
    pub fn from_persisted(state: &str, ingress: TurnInputIngress) -> Option<Self> {
        match (TurnInputStateKind::from_wire_str(state)?, ingress) {
            (
                TurnInputStateKind::PendingActive,
                TurnInputIngress::ActiveTurn {
                    turn_id,
                    min_boundary,
                },
            ) => Some(Self::PendingActive(ActiveTurnIngress {
                turn_id,
                min_boundary,
            })),
            (
                TurnInputStateKind::Accepted,
                TurnInputIngress::ActiveTurn {
                    turn_id,
                    min_boundary,
                },
            ) => Some(Self::Accepted(ActiveTurnIngress {
                turn_id,
                min_boundary,
            })),
            (TurnInputStateKind::DeferredNextTurn, TurnInputIngress::NextTurn) => {
                Some(Self::DeferredNextTurn)
            }
            (TurnInputStateKind::Cancelled, ingress) => Some(Self::Cancelled(ingress)),
            (TurnInputStateKind::Completed, ingress) => Some(Self::Completed(ingress)),
            _ => None,
        }
    }

    /// The scope-free name this state persists under — the `state` column's spelling.
    pub fn kind(&self) -> TurnInputStateKind {
        match self {
            Self::PendingActive(_) => TurnInputStateKind::PendingActive,
            Self::DeferredNextTurn => TurnInputStateKind::DeferredNextTurn,
            Self::Accepted(_) => TurnInputStateKind::Accepted,
            Self::Cancelled(_) => TurnInputStateKind::Cancelled,
            Self::Completed(_) => TurnInputStateKind::Completed,
        }
    }

    /// The stable wire spelling this state persists under.
    pub fn as_str(&self) -> &'static str {
        self.kind().as_str()
    }

    /// The admission scope this state carries — the `ingress_json` column's value.
    pub fn ingress(&self) -> TurnInputIngress {
        match self {
            Self::PendingActive(scope) | Self::Accepted(scope) => scope.clone().into(),
            Self::DeferredNextTurn => TurnInputIngress::NextTurn,
            Self::Cancelled(ingress) | Self::Completed(ingress) => ingress.clone(),
        }
    }

    /// The turn id this input is scoped to, when its scope is `active_turn`.
    pub fn active_turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::PendingActive(scope) | Self::Accepted(scope) => Some(&scope.turn_id),
            Self::Cancelled(ingress) | Self::Completed(ingress) => ingress.active_turn_id(),
            Self::DeferredNextTurn => None,
        }
    }

    /// Rebinds an `active_turn`-scoped open state to `accepted`, carrying its
    /// scope forward. Returns `None` for `next_turn`-scoped or terminal
    /// states — the pairs the persisted CHECKs refuse.
    pub fn accepted(&self) -> Option<Self> {
        match self {
            Self::PendingActive(scope) | Self::Accepted(scope) => {
                Some(Self::Accepted(scope.clone()))
            }
            _ => None,
        }
    }

    /// Lets store, effect-host, and protocol implementors test whether this `TurnInputState` is
    /// next turn pending while materializing, executing, or persisting a session turn.
    pub fn is_next_turn_pending(&self) -> bool {
        self.kind().is_next_turn_pending()
    }

    /// Returns whether this state is settled and eligible for tombstone vacuum.
    pub fn is_terminal(&self) -> bool {
        self.kind().is_terminal()
    }

    pub fn is_open(&self) -> bool {
        self.kind().is_open()
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInputDraft {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub ingress: TurnInputIngress,
    pub input: TurnInput,
    /// The shape the input runs under (FIG-3838). The default spec is the
    /// session config and is stored as no spec at all.
    #[serde(default, skip_serializing_if = "crate::run_spec::RunSpec::is_default")]
    pub run_spec: crate::run_spec::RunSpec,
}
impl PendingTurnInputDraft {
    /// Constructs a `PendingTurnInputDraft` for store and durable-substrate implementors while
    /// claiming and settling durable turn inputs.
    pub fn new(
        session_id: impl Into<SessionId>,
        ingress: TurnInputIngress,
        input: TurnInput,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            input_id: None,
            source_key: None,
            ingress,
            input,
            run_spec: crate::run_spec::RunSpec::default(),
        }
    }

    /// Sets the spec the input runs under (FIG-3838).
    pub fn with_run_spec(mut self, run_spec: crate::run_spec::RunSpec) -> Self {
        self.run_spec = run_spec;
        self
    }

    /// The hash the store interns this draft's spec under: `None` for the
    /// default spec, which is never interned.
    pub fn run_spec_hash(&self) -> Result<Option<crate::run_spec::RunSpecHash>, serde_json::Error> {
        self.run_spec.hash()
    }

    /// The input id a keyed submission is accepted under: a function of its
    /// session and source key alone, `ti:<blake3-hex>` like every minted id.
    /// A host that knows the key it sent under can re-attach to the input
    /// after a restart without any read (FIG-3837).
    #[must_use]
    pub fn keyed_input_id(session_id: &SessionId, source_key: &str) -> String {
        format!(
            "ti:{}",
            crate::stable_hash::blake3_hex(
                "lash-keyed-turn-input/v1",
                // Length-prefixed, so no session and key pair spells another.
                format!("{}:{session_id}:{source_key}", session_id.as_str().len()).as_bytes(),
            )
        )
    }

    /// Sets the input id carried by a `PendingTurnInputDraft` for store and durable-substrate
    /// implementors while claiming and settling durable turn inputs.
    pub fn with_input_id(mut self, input_id: impl Into<String>) -> Self {
        self.input_id = Some(input_id.into());
        self
    }

    /// Sets the source key carried by a `PendingTurnInputDraft` for store and durable-substrate
    /// implementors while claiming and settling durable turn inputs.
    pub fn with_source_key(mut self, source_key: impl Into<String>) -> Self {
        self.source_key = Some(source_key.into());
        self
    }

    /// The canonical digest of this submission: the immutable value source-key
    /// replay compares.
    ///
    /// Stores write it once, beside the submitted ingress, when they admit the
    /// row, and never rewrite it. A retry under the same source key is the same
    /// submission exactly when its digest equals the stored one; the row's
    /// *current* ingress and state (which a Defer rewrites to `next_turn` when
    /// the named turn ends) take no part in the verdict (ADR 0010).
    ///
    /// The digest covers the submission as the host made it and nothing the
    /// store generates or mutates:
    ///
    /// - the ingress as submitted — scope, and for `active_turn` the turn id
    ///   and minimum checkpoint boundary;
    /// - the input's canonical JSON (the persisted `TurnInput` serde form with
    ///   object keys sorted and `-0.0` folded to `0.0`), as one opaque leaf;
    /// - the input's [`RunSpecHash`](crate::run_spec::RunSpecHash) when its
    ///   spec is not the default. The default spec adds nothing, so an
    ///   omitted spec and an explicit default are the same submission.
    ///
    /// Excluded: the session id and source key (the row is found by them), the
    /// generated input id, the enqueue time, and every lifecycle and claim
    /// field. The preimage is the `lash.turn-input-submission` identity family
    /// at [`TURN_INPUT_SUBMISSION_FAMILY_VERSION`]; the rendered form is
    /// `turn-input-submission:v<family>:blake3:<hex>`.
    pub fn submission_digest(&self) -> Result<String, serde_json::Error> {
        let preimage = turn_input_submission_preimage(
            &self.ingress,
            &self.input,
            self.run_spec.hash()?.as_ref(),
        )?;
        Ok(crate::stable_identity::rendered_hash(
            "turn-input-submission",
            TURN_INPUT_SUBMISSION_FAMILY_VERSION,
            &preimage,
        ))
    }
}

/// The drafts one session admits as one request, in request order
/// (FIG-3842).
///
/// A store admits a batch in one transaction: every draft an existing row
/// already answers returns that row, wherever it sits and whatever became of
/// it, and every other draft is enqueued in request order as one contiguous
/// block of the session's ingress sequence, with no other producer's item in
/// between. A draft that conflicts with a stored row refuses the whole
/// request and nothing is stored, spec rows included.
///
/// Construction refuses a request that names one input twice, by source key
/// or input id, and a draft for another session: nothing a store could
/// answer position by position.
#[derive(Clone, Debug)]
pub struct PendingTurnInputBatch {
    session_id: SessionId,
    drafts: Vec<PendingTurnInputDraft>,
}

impl PendingTurnInputBatch {
    /// A batch of `session_id`'s `drafts`, in request order.
    pub fn new(
        session_id: impl Into<SessionId>,
        drafts: Vec<PendingTurnInputDraft>,
    ) -> Result<Self, crate::store::StoreError> {
        let session_id = session_id.into();
        let mut seen = std::collections::BTreeSet::new();
        for draft in &drafts {
            if draft.session_id != session_id {
                return Err(
                    crate::store::StoreError::PendingTurnInputBatchForeignSession {
                        session_id,
                        draft_session_id: draft.session_id.clone(),
                    },
                );
            }
            let names = [
                draft.source_key.as_deref().map(|key| ("source key", key)),
                draft.input_id.as_deref().map(|id| ("input id", id)),
            ];
            for (kind, name) in names.into_iter().flatten() {
                if !seen.insert((kind, name)) {
                    return Err(crate::store::StoreError::PendingTurnInputBatchDuplicate {
                        session_id,
                        name: format!("{kind} `{name}`"),
                    });
                }
            }
        }
        Ok(Self { session_id, drafts })
    }

    /// A batch of one draft: what a single enqueue admits.
    pub fn one(draft: PendingTurnInputDraft) -> Self {
        Self {
            session_id: draft.session_id.clone(),
            drafts: vec![draft],
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The drafts, in request order.
    pub fn drafts(&self) -> &[PendingTurnInputDraft] {
        &self.drafts
    }

    pub fn into_drafts(self) -> Vec<PendingTurnInputDraft> {
        self.drafts
    }

    /// The one row a batch of [`one`](Self::one) admitted.
    pub fn only(
        mut admitted: Vec<PendingTurnInput>,
    ) -> Result<PendingTurnInput, crate::store::StoreError> {
        match (admitted.pop(), admitted.is_empty()) {
            (Some(row), true) => Ok(row),
            _ => Err(crate::store::StoreError::Backend(
                "a batch of one did not admit exactly one pending turn input".to_string(),
            )),
        }
    }
}

/// Family version of the turn-input submission digest
/// ([`PendingTurnInputDraft::submission_digest`]).
///
/// Stored digests are compared for equality against freshly computed ones, so
/// any change to the preimage grammar or to the `TurnInput` serde form it
/// hashes must bump this version together with both SQL store schema versions:
/// a row admitted under the old grammar would otherwise refuse its own
/// identical retry as a conflict.
pub const TURN_INPUT_SUBMISSION_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for the turn-input submission preimage.
///
/// Ingress scope: 1 `active_turn` (followed by the turn id and the boundary),
/// 2 `next_turn`. Boundary: 1 `after_work`, 2 `before_completion`. The input
/// follows as one canonical JSON payload leaf. A non-default run spec follows
/// last as tag 1 and its hash; the default spec appends nothing. Retired tags
/// remain burned.
fn turn_input_submission_preimage(
    ingress: &TurnInputIngress,
    input: &TurnInput,
    run_spec: Option<&crate::run_spec::RunSpecHash>,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.turn-input-submission",
        TURN_INPUT_SUBMISSION_FAMILY_VERSION,
    );
    match ingress {
        TurnInputIngress::ActiveTurn {
            turn_id,
            min_boundary,
        } => {
            identity.tag(1);
            identity.string(turn_id.as_str());
            identity.tag(match min_boundary {
                TurnInputCheckpointBoundary::AfterWork => 1,
                TurnInputCheckpointBoundary::BeforeCompletion => 2,
            });
        }
        TurnInputIngress::NextTurn => identity.tag(2),
    }
    identity.bytes(&crate::identity_json::payload_leaf(&serde_json::to_value(
        input,
    )?));
    if let Some(run_spec) = run_spec {
        identity.tag(1);
        identity.string(run_spec.as_str());
    }
    Ok(identity.finish())
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInput {
    pub input_id: crate::InputId,
    pub session_id: SessionId,
    pub enqueue_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// The admission scope lives inside `state`: every open or accepted
    /// variant carries its scope, so the persisted `ingress_json`/`state`
    /// column pair cannot disagree.
    pub state: TurnInputState,
    pub enqueued_at_ms: u64,
    pub input: TurnInput,
    /// The interned spec the input runs under; `None` for the default spec
    /// (FIG-3838). A claim never mixes inputs whose specs differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_spec: Option<crate::run_spec::RunSpecHash>,
}

/// Host-facing projection of one open pending turn-input record.
///
/// This projection is separate from [`PendingTurnInput`] because a row's
/// durable lifecycle state and its read-time claim status answer different
/// questions. A live matching session-execution-lease generation makes the
/// row held; an expired, released, or mismatched generation leaves it pending
/// for successor reclaim under ADR 0029, unless an aborted direct turn's
/// binding reserves it for that turn (FIG-3589).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct PendingTurnInputRead {
    /// The durable admission record.
    pub input: PendingTurnInput,
    /// Its factual status at the read's store-clock instant.
    pub status: PendingTurnInputReadStatus,
}

impl PendingTurnInputRead {
    /// Project an open row that no live matching claim currently holds.
    pub fn pending(input: PendingTurnInput) -> Self {
        Self {
            input,
            status: PendingTurnInputReadStatus::Pending,
        }
    }

    /// Project an open row held by the currently live matching lease generation.
    pub fn held(input: PendingTurnInput, lease_expires_at_ms: u64) -> Self {
        Self {
            input,
            status: PendingTurnInputReadStatus::Held {
                lease_expires_at_ms,
            },
        }
    }
}

/// `Held` reports only durable lease facts. It does not assert that the holder
/// process is alive, and lease expiry does not itself supersede the holder's
/// completion authority.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingTurnInputReadStatus {
    /// No currently live matching lease generation holds the row.
    Pending,
    /// The row's claim matches the currently live session lease generation.
    Held {
        /// Exact expiry stored on that matching session-execution lease.
        lease_expires_at_ms: u64,
    },
}

/// Durable acceptance evidence returned to an ingress caller.
///
/// The receipt deliberately carries only stable routing and idempotency
/// identity. Queue dispatch and the pending row's mutable lifecycle state are
/// observed separately through the pending-input reconciliation surface.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputAcceptanceReceipt {
    pub input_id: crate::InputId,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub ingress: TurnInputIngress,
}
/// Durable evidence that an admitted input became canonical conversation input.
///
/// This is the application stage between [`TurnInputAcceptanceReceipt`]
/// (admission) and the terminal turn commit (settlement). It deliberately
/// carries identity only: hosts correlate an input to its canonical turn and
/// committed message without parsing or retaining display text.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct TurnInputApplication {
    pub input_id: crate::InputId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub turn_id: crate::TurnId,
    pub committed_message_id: String,
    /// Present for active-turn checkpoint application and absent when the
    /// input formed the initial canonical input of an idle queued turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointKind>,
}
impl From<&PendingTurnInput> for TurnInputAcceptanceReceipt {
    fn from(input: &PendingTurnInput) -> Self {
        Self {
            input_id: input.input_id.clone(),
            session_id: input.session_id.clone(),
            source_key: input.source_key.clone(),
            ingress: input.ingress(),
        }
    }
}
impl PendingTurnInput {
    /// The row's admission scope — derived from [`Self::state`], which carries
    /// it, so the persisted `ingress_json`/`state` pair cannot disagree.
    pub fn ingress(&self) -> TurnInputIngress {
        self.state.ingress()
    }

    /// Exposes accepted input to store and durable-substrate implementors while claiming and
    /// settling durable turn inputs.
    pub fn accepted_input(&self) -> Option<crate::AcceptedInjectedTurnInput> {
        plugin_message_from_turn_input(&self.input).map(|message| {
            crate::AcceptedInjectedTurnInput {
                id: self
                    .source_key
                    .as_deref()
                    .map(source_key_display_id)
                    .or_else(|| Some(self.input_id.to_string())),
                message,
            }
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PendingTurnInputCancelTarget {
    InputId(String),
    SourceKey(String),
}
impl PendingTurnInputCancelTarget {
    /// Targets one durable input ID for turn-input store implementors performing cancellation.
    pub fn input_id(input_id: impl Into<String>) -> Self {
        Self::InputId(input_id.into())
    }

    /// Targets the input admitted under a source idempotency key for turn-input store implementors
    /// performing cancellation.
    pub fn source_key(source_key: impl Into<String>) -> Self {
        Self::SourceKey(source_key.into())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInputClaimDiagnostics {
    pub state: TurnInputState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_owner: Option<crate::LeaseOwnerIdentity>,
    /// The session-execution-lease generation the live claim pins, when a claim
    /// holds the row. `None` when the row carries no claim (ADR 0029).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_session_lease_generation: Option<u64>,
    pub claim_fencing_token: u64,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub enum PendingTurnInputCancelOutcome {
    Cancelled(PendingTurnInput),
    AlreadyClaimed {
        input: PendingTurnInput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<PendingTurnInputClaimDiagnostics>,
    },
    AlreadyCompleted(PendingTurnInput),
    AlreadyCancelled(PendingTurnInput),
    NotFound,
}
impl PendingTurnInputCancelOutcome {
    /// Reports success to turn-input store implementors only for the transition performed by this
    /// cancellation attempt, not for an already-cancelled row.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }

    /// Returns the durable input for every found cancellation outcome and `None` only when the
    /// target was absent.
    pub fn input(&self) -> Option<&PendingTurnInput> {
        match self {
            Self::Cancelled(input)
            | Self::AlreadyClaimed { input, .. }
            | Self::AlreadyCompleted(input)
            | Self::AlreadyCancelled(input) => Some(input),
            Self::NotFound => None,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInputCancelReceipt {
    pub target: PendingTurnInputCancelTarget,
    pub outcome: PendingTurnInputCancelOutcome,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub enum PendingTurnInputSuffixCancelOutcome {
    AnchorNotFound {
        anchor: PendingTurnInputCancelTarget,
    },
    Outcomes {
        anchor: PendingTurnInputCancelTarget,
        outcomes: Vec<PendingTurnInputCancelOutcome>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnInputClaimMode {
    ActiveTurn {
        turn_id: crate::TurnId,
        checkpoint: CheckpointKind,
    },
    NextTurn,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputCompletionData {
    pub input_ids: Vec<crate::InputId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<TurnInputApplication>,
}
/// The claim a settling driver held over the rows it is settling.
///
/// Present only in the claimed regime; see [`TurnInputCompletion::claim`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputSettlementClaim {
    pub claim_id: String,
    pub lease_token: String,
}
/// A settlement receipt carrying settled turn-input identities, application
/// evidence, and the authority the settling driver holds over those rows.
///
/// Turn-input settlement has exactly two regimes and one authority. Both are a
/// conditional write decided by the head CAS the runtime commit already
/// performs; `claim` only *strengthens* that write's predicate:
///
/// * `Some(..)` — the driver holds a generation-fenced claim
///   ([ADR 0029](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0029-claims-are-generation-fenced-under-the-session-lease.md)),
///   and the row must still carry that claim id and lease token.
/// * `None` — the driver accepted these rows itself and drove them without the
///   session-execution lane
///   ([ADR 0069 §5](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)),
///   and the row must still be unclaimed and unsettled.
///
/// Every backend verifies that the settlement affected exactly one row and
/// reports a typed supersession error otherwise; zero rows is never silent
/// success.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputCompletion {
    pub session_id: SessionId,
    #[serde(default, flatten)]
    pub claim: Option<TurnInputSettlementClaim>,
    #[serde(flatten)]
    pub data: TurnInputCompletionData,
}
impl TurnInputCompletion {
    pub fn claim_id(&self) -> Option<&str> {
        self.claim.as_ref().map(|claim| claim.claim_id.as_str())
    }

    /// Exposes the settling lease token to store implementors, or `None` when
    /// the settlement is unclaimed.
    pub fn lease_token(&self) -> Option<&str> {
        self.claim.as_ref().map(|claim| claim.lease_token.as_str())
    }

    /// Names this settlement's rows for diagnostics that must report a
    /// settlement without assuming it had a claim id.
    pub fn settlement_identity(&self) -> String {
        match self.claim.as_ref() {
            Some(claim) => claim.claim_id.clone(),
            None => format!("unclaimed:{}", self.data.input_ids.join(",")),
        }
    }
}
impl std::ops::Deref for TurnInputCompletion {
    type Target = TurnInputCompletionData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}
impl std::ops::DerefMut for TurnInputCompletion {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnInputClaimData {
    pub mode: TurnInputClaimMode,
    pub inputs: Vec<PendingTurnInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<TurnInputApplication>,
}
/// A shared work claim carrying pending turn-input material.
pub type TurnInputClaim = crate::WorkClaim<TurnInputClaimData>;
impl crate::WorkClaim<TurnInputClaimData> {
    /// Exposes completion to store and durable-substrate implementors while claiming and settling
    /// durable queued work.
    pub fn completion(&self) -> TurnInputCompletion {
        TurnInputCompletion {
            session_id: self.session_id.clone(),
            claim: Some(TurnInputSettlementClaim {
                claim_id: self.claim_id.clone(),
                lease_token: self.lease_token.clone(),
            }),
            data: TurnInputCompletionData {
                input_ids: self
                    .inputs
                    .iter()
                    .map(|input| input.input_id.clone())
                    .collect(),
                applications: self.applications.clone(),
            },
        }
    }

    /// Updates initial turn application state for store and durable-substrate implementors while
    /// claiming and settling durable queued work.
    pub fn record_initial_turn_application(
        &mut self,
        turn_id: &crate::TurnId,
        committed_message_id: &str,
    ) {
        self.applications = initial_turn_applications(&self.inputs, turn_id, committed_message_id);
    }

    /// Records application evidence only for claimed inputs whose deterministic ingress message IDs
    /// appear in the committed checkpoint messages.
    pub fn record_checkpoint_applications(
        &mut self,
        turn_id: &crate::TurnId,
        checkpoint: CheckpointKind,
        committed_messages: &[crate::Message],
    ) {
        let committed_message_ids = committed_messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let recorded = self
            .inputs
            .iter()
            .filter_map(|input| {
                let committed_message_id = ingress_message_id(&input.input_id);
                committed_message_ids
                    .contains(committed_message_id.as_str())
                    .then(|| TurnInputApplication {
                        input_id: input.input_id.clone(),
                        source_key: input.source_key.clone(),
                        turn_id: turn_id.clone(),
                        committed_message_id,
                        checkpoint: Some(checkpoint),
                    })
            })
            .collect::<Vec<_>>();
        self.applications.retain(|application| {
            !recorded
                .iter()
                .any(|replacement| replacement.input_id == application.input_id)
        });
        self.applications.extend(recorded);
    }

    /// Exposes accepted turn inputs to store and durable-substrate implementors while claiming and
    /// settling durable queued work.
    pub fn accepted_turn_inputs(&self) -> Vec<crate::AcceptedInjectedTurnInput> {
        self.inputs
            .iter()
            .filter_map(PendingTurnInput::accepted_input)
            .collect()
    }

    /// Materializes claimed inputs in claim order for turn-input store implementors, resolving
    /// attachments and omitting inputs that produce no committed message.
    pub async fn materialize_checkpoint_turn_input(
        &self,
        turn_id: &crate::TurnId,
        attachment_store: &crate::SessionAttachmentStore,
        attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
    ) -> Result<QueuedCheckpointTurnInput, String> {
        let mut messages = Vec::new();
        for input in &self.inputs {
            if let Some(message) = committed_message_from_pending_input(
                input,
                turn_id,
                attachment_store,
                attachment_source_policy,
            )
            .await?
            {
                messages.push(message);
            }
        }
        Ok(QueuedCheckpointTurnInput {
            messages,
            turn_causes: Vec::new(),
        })
    }

    /// Materializes for turn data for store and durable-substrate implementors while claiming and
    /// settling durable queued work.
    pub fn materialize_turn_input(&self) -> TurnInput {
        materialize_turn_input(&self.inputs)
    }
}

pub(crate) fn source_key_display_id(source: &str) -> String {
    source
        .strip_prefix("host:")
        .or_else(|| source.strip_prefix("injection:"))
        .unwrap_or(source)
        .to_string()
}

/// The initial drive set of an accepted turn input, as the
/// `ClaimAcceptedTurnInput` runtime effect journals it (ADR 0069 §6).
///
/// It is a self-contained authority snapshot: a claimed drive carries the rows
/// with their content and claim token, and a refusal names why the root cedes.
/// A drive admits the head of the queue, so the claim always reaches its row
/// or refuses it (FIG-3600).
/// Replay returns this value and never reconstructs it from pending rows, so
/// `vacuum()` pruning terminal rows cannot change what a replayed turn does.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "drive", rename_all = "snake_case")]
pub enum AcceptedTurnInputDrive {
    /// The claim reached the accepted row: drive every claimed row and settle
    /// them under the claim predicate.
    ///
    /// This is the turn's admission, so it also records what the turn was
    /// admitted on: the session head (`base`) and the turn index. A replay
    /// rebuilds the turn's input state from `base` and addresses its effects
    /// under `turn_index`, never re-reading either from the live head, which
    /// the turn's own commit may already have advanced (FIG-3682). It also
    /// records the executable generation the turn runs under (FIG-3571). The
    /// session drive runs this claim as its root's recorded claim step.
    Claimed {
        claim: Box<TurnInputClaim>,
        base: crate::store::SessionHeadRef,
        turn_index: u64,
        /// The executable generation the turn was admitted under (FIG-3571):
        /// a redrive under another one is refused before any effect.
        generation: Option<crate::executable_generation::ExecutableGeneration>,
    },
    /// The accepted row cannot be driven by this turn.
    Refused { refusal: AcceptedTurnInputRefusal },
}

/// Why an accepted turn input's first execution ceded instead of driving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedTurnInputRefusal {
    /// The accepted row is held by a claim of the live lease generation that
    /// is not this drive's. The drive's turn holds that lease, so the holder is
    /// another claim of the same lane: an earlier run of this drive body, or a
    /// claim whose hand-back failed. The row stays claim-pinned until that
    /// claim settles or the generation turns over, and is then drained.
    HeldByLiveClaim,
    /// The accepted row is no longer open: another driver settled it, the host
    /// cancelled it, or `vacuum()` pruned it after either.
    SettledOrRemoved,
}

fn initial_turn_applications(
    inputs: &[PendingTurnInput],
    turn_id: &crate::TurnId,
    committed_message_id: &str,
) -> Vec<TurnInputApplication> {
    inputs
        .iter()
        .filter(|input| {
            input.input.items.iter().any(|item| match item {
                crate::InputItem::Text { text } => !text.is_empty(),
                crate::InputItem::Attachment { .. } => true,
            })
        })
        .map(|input| TurnInputApplication {
            input_id: input.input_id.clone(),
            source_key: input.source_key.clone(),
            turn_id: turn_id.clone(),
            committed_message_id: committed_message_id.to_string(),
            checkpoint: None,
        })
        .collect()
}
pub fn materialize_turn_input(inputs: &[PendingTurnInput]) -> TurnInput {
    let mut input_items = Vec::new();
    let mut trace_turn_id = None;
    for pending in inputs {
        input_items.extend(pending.input.items.clone());
        if trace_turn_id.is_none() {
            trace_turn_id = pending.input.trace_turn_id.clone();
        }
    }
    TurnInput {
        items: input_items,
        trace_turn_id,
        turn_context: crate::TurnContext::default(),
    }
}
#[derive(Clone, Debug, Default)]
pub struct QueuedCheckpointTurnInput {
    pub messages: Vec<crate::Message>,
    pub turn_causes: Vec<TurnCause>,
}
pub(crate) fn plugin_message_from_turn_input(input: &TurnInput) -> Option<PluginMessage> {
    let parts: Vec<_> = input
        .items
        .iter()
        .map(|item| match item {
            crate::InputItem::Text { text } => crate::Part::text(String::new(), text.clone(), None),
            crate::InputItem::Attachment { source } => crate::Part::attachment_part(
                String::new(),
                String::new(),
                Some(lash_sansio::PartAttachment {
                    source: source.clone(),
                }),
            ),
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(PluginMessage {
        id: None,
        role: crate::MessageRole::User,
        origin: None,
        parts,
    })
}

async fn committed_message_from_pending_input(
    pending: &PendingTurnInput,
    turn_id: &crate::TurnId,
    attachment_store: &crate::SessionAttachmentStore,
    attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
) -> Result<Option<crate::Message>, String> {
    let normalized = crate::input_normalization::normalize_input_items(
        &pending.input.items,
        attachment_store,
        attachment_source_policy,
    )
    .await?;
    let message_id = ingress_message_id(&pending.input_id);
    let mut parts = Vec::new();
    for item in normalized {
        match item {
            crate::NormalizedItem::Text(text) if !text.is_empty() => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part::text(part_id, text, None));
            }
            crate::NormalizedItem::Text(_) => {}
            crate::NormalizedItem::Attachment(source) => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part::attachment_part(
                    part_id,
                    String::new(),
                    Some(crate::session_model::message::PartAttachment { source }),
                ));
            }
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::Message {
        id: message_id,
        role: crate::MessageRole::User,
        // Same typed provenance the turn's opening input carries: the absorbing
        // turn plus the durable input this message came from (FIG-972).
        origin: Some(crate::MessageOrigin::TurnInput {
            turn_id: turn_id.clone(),
            input_id: Some(pending.input_id.clone()),
        }),
        parts: crate::shared_parts(parts),
    }))
}
pub fn ingress_message_id(input_id: &str) -> String {
    format!("m_ingress_{input_id}")
}

/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Text { text: String },
    Attachment { source: crate::AttachmentSource },
}
impl InputItem {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn attachment(source: crate::AttachmentSource) -> Self {
        Self::Attachment { source }
    }
}
/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TurnInput {
    pub items: Vec<InputItem>,
    /// Internal protocol transport carrier for the facade builder's turn ID.
    ///
    /// All non-advanced facade paths overwrite this field.
    /// Only low-level protocol transport should read this field directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_turn_id: Option<TurnId>,
    #[serde(skip)]
    pub turn_context: TurnContext,
}
impl TurnInput {
    pub fn empty() -> Self {
        Self::items(std::iter::empty())
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::items([InputItem::text(text)])
    }

    /// Collects mixed input items in caller order for protocol implementors materializing a turn.
    pub fn items(items: impl IntoIterator<Item = InputItem>) -> Self {
        Self {
            items: items.into_iter().collect(),
            trace_turn_id: None,
            turn_context: TurnContext::default(),
        }
    }

    pub fn with_attachment(mut self, source: crate::AttachmentSource) -> Self {
        self.items.push(InputItem::attachment(source));
        self
    }
}
/// How a running turn treats durable queued work.
///
/// Written as one fact: an automatic drain may keep claiming checkpoint
/// batches as it runs, while a Selected Queued-Work Drain is closed over the
/// host-pinned batch-id set — checkpoint pull-in is forbidden and the pinned
/// composition's cost bound is enforced. The remaining flag combinations are
/// unrepresentable.
#[derive(Clone, Copy)]
enum QueuedWorkDrainMode {
    Automatic,
    Selected,
}
#[derive(Clone)]
pub struct TurnContext {
    prompt: crate::PromptLayer,
    runtime_correlation: Option<Arc<dyn Any + Send + Sync>>,
    queued_work_drain: QueuedWorkDrainMode,
}
impl Default for TurnContext {
    fn default() -> Self {
        Self {
            prompt: crate::PromptLayer::default(),
            runtime_correlation: None,
            queued_work_drain: QueuedWorkDrainMode::Automatic,
        }
    }
}
impl TurnContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_selected_queued_work_drain(&mut self) {
        self.queued_work_drain = QueuedWorkDrainMode::Selected;
    }

    pub fn enforces_selected_queued_work_cost_bound(&self) -> bool {
        matches!(self.queued_work_drain, QueuedWorkDrainMode::Selected)
    }

    pub fn checkpoint_queued_work_limit(&self, default_limit: usize) -> usize {
        match self.queued_work_drain {
            QueuedWorkDrainMode::Automatic => default_limit,
            QueuedWorkDrainMode::Selected => 0,
        }
    }

    pub fn set_prompt_layer(&mut self, prompt: crate::PromptLayer) {
        self.prompt = prompt;
    }

    pub fn prompt_layer(&self) -> &crate::PromptLayer {
        &self.prompt
    }

    /// Installs one live runtime-owned correlation value.
    ///
    /// The value's private concrete type is its authority boundary: callers
    /// cannot fabricate or read a correlation type owned by another crate.
    #[doc(hidden)]
    pub fn set_runtime_correlation<T>(&mut self, value: T)
    where
        T: Any + Send + Sync,
    {
        self.runtime_correlation = Some(Arc::new(value));
    }

    #[doc(hidden)]
    pub fn runtime_correlation<T: Any>(&self) -> Option<&T> {
        self.runtime_correlation.as_ref()?.downcast_ref()
    }

    #[doc(hidden)]
    pub fn clear_runtime_correlation<T: Any>(&mut self) {
        if self
            .runtime_correlation
            .as_ref()
            .is_some_and(|value| value.is::<T>())
        {
            self.runtime_correlation = None;
        }
    }
}
impl facade_ops::TurnContextFacadeOps for TurnContext {
    fn set_prompt_template(&mut self, template: crate::PromptTemplate) {
        self.prompt.template = Some(template);
    }

    fn add_prompt_contribution(&mut self, contribution: crate::PromptContribution) {
        self.prompt.add_contribution(contribution);
    }

    fn replace_prompt_slot(
        &mut self,
        slot: crate::PromptSlot,
        contributions: impl IntoIterator<Item = crate::PromptContribution>,
    ) {
        self.prompt.replace_slot(slot, contributions);
    }

    fn clear_prompt_slot(&mut self, slot: crate::PromptSlot) {
        self.prompt.clear_slot(slot);
    }
}
impl fmt::Debug for TurnContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnContext")
            .field("has_prompt_layer", &(!self.prompt.is_empty()))
            .finish()
    }
}

/// Stable identifier for a semantic turn activity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnActivityId(pub Arc<str>);
impl TurnActivityId {
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }
}

pub mod facade_ops {

    /// Facade-internal operations for [`TurnContext`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait TurnContextFacadeOps {
        fn set_prompt_template(&mut self, template: crate::PromptTemplate);

        fn add_prompt_contribution(&mut self, contribution: crate::PromptContribution);

        // APIT is intentionally non-dyn-compatible; this trait has one static-dispatch impl.
        fn replace_prompt_slot(
            &mut self,
            slot: crate::PromptSlot,
            contributions: impl IntoIterator<Item = crate::PromptContribution>,
        );

        fn clear_prompt_slot(&mut self, slot: crate::PromptSlot);
    }
}

/// The generated encoder and decoder matches are exhaustive, so adding a variant requires its
/// persisted spelling here and necessarily extends `ALL`.
macro_rules! turn_input_wire {
    ($type:ident, $visibility:vis, $encoder:ident, $decoder:ident {
        $($variant:ident => $wire:literal),+ $(,)?
    }) => {
        impl $type {
            #[allow(dead_code)]
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            $visibility fn $encoder(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }

            #[allow(dead_code)]
            $visibility fn $decoder(value: &str) -> Option<Self> {
                match value {
                    $($wire => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

turn_input_wire!(TurnInputCheckpointBoundary, pub, as_wire_str, from_wire_str {
    AfterWork => "after_work",
    BeforeCompletion => "before_completion",
});

turn_input_wire!(TurnInputStateKind, pub, as_str, from_wire_str {
    PendingActive => "pending_active",
    DeferredNextTurn => "deferred_next_turn",
    Accepted => "accepted",
    Cancelled => "cancelled",
    Completed => "completed",
});

impl TurnInput {
    /// The part of this input a durable acceptance row can carry.
    ///
    /// The live `TurnContext` (a child turn's process correlation and
    /// lineage) is process-local state that no store can hold, so the
    /// acceptance commit records everything else and the caller driving the
    /// turn keeps the live state (ADR 0069). A worker that later recovers the
    /// row drives exactly this projection.
    ///
    /// `trace_turn_id` is dropped for the same reason: it labels one drive
    /// attempt, not the input. A recovered row is driven under the recovering
    /// worker's own execution scope, and a persisted trace id from the
    /// abandoned attempt would collide with it
    /// ([`RuntimeErrorCode::ExecutionScopeTurnIdMismatch`](crate::RuntimeErrorCode::ExecutionScopeTurnIdMismatch)),
    /// making an accepted direct turn unrecoverable — exactly the property
    /// ADR 0069 exists to guarantee.
    #[must_use]
    pub fn durable_projection(&self) -> Self {
        Self {
            items: self.items.clone(),
            trace_turn_id: None,
            turn_context: crate::TurnContext::default(),
        }
    }
}

#[cfg(test)]
#[path = "turn_input_vocabulary_tests.rs"]
mod tests;
