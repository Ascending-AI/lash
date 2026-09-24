//! The session-ingress row codec and admission decision both SQL backends
//! share (ADR 0101).
//!
//! A backend reads a row's columns with its own driver into
//! [`SessionIngressRowColumns`] and decodes them here, so the two backends
//! cannot drift on what a row means. Admission is decided here too, from the
//! facts the backend observed under its session lock.

use crate::session_ingress_vocabulary::{
    Delivery, IngressEnqueueOutcome, IngressItem, IngressItemDraft, IngressItemId, IngressKind,
    IngressPayload, IngressState, IngressTerminalCause,
};
use crate::store::session_ingress_plan::{
    IngressClaimCandidate, IngressRowClaim, IngressSettlementRow,
};
use crate::store::{AdmissionId, DriveFence, IngressClaimIdentity, StoreError};
use crate::{ProcessId, QueuedWorkAuthority, SessionId, TurnId};

const RECORD_KIND: &str = "SessionIngressItem";

/// The fence a backend's [`DriveEpochStore::seal_drive_epoch`] returns for
/// the epoch its compare-and-set raised, or the one a retried seal of the
/// same admission finds. It is the only constructor of a [`DriveFence`]
/// outside `lash-core-store`, and only store backends call it.
///
/// [`DriveEpochStore::seal_drive_epoch`]: crate::store::DriveEpochStore::seal_drive_epoch
#[must_use]
pub fn sealed_drive_fence(session_id: SessionId, epoch: u64, admission: AdmissionId) -> DriveFence {
    DriveFence::sealed_by_store(session_id, epoch, admission)
}

fn corrupt(message: impl Into<String>) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: RECORD_KIND,
        message: message.into(),
    }
}

fn unsigned(field: &str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| corrupt(format!("{field} must be non-negative, got {value}")))
}

fn decode_json<T: serde::de::DeserializeOwned>(field: &str, value: &str) -> Result<T, StoreError> {
    serde_json::from_str(value).map_err(|error| corrupt(format!("{field}: {error}")))
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::RecordEncodingFailed {
        record_kind: RECORD_KIND.to_string(),
        message: error.to_string(),
    })
}

/// One row's columns as a driver reads them, in
/// `lash_store_sql::session_ingress::ROW_COLUMNS` order.
#[derive(Clone, Debug)]
pub struct SessionIngressRowColumns {
    pub enqueue_seq: i64,
    pub item_id: String,
    pub session_id: String,
    pub kind: String,
    pub source_key: Option<String>,
    pub delivery_scope: String,
    pub delivery_turn_id: Option<String>,
    pub delivery_min_boundary: Option<String>,
    pub submission_digest: String,
    pub payload_json: String,
    pub authority_json: Option<String>,
    pub merge_key: Option<String>,
    pub state: String,
    pub terminal_cause_json: Option<String>,
    pub enqueued_at_ms: i64,
    pub terminal_at_ms: Option<i64>,
    pub claim_id: Option<String>,
    pub claim_token: Option<String>,
    pub claim_admission_id: Option<String>,
    pub claim_fencing_token: i64,
    pub claim_drive_epoch: Option<i64>,
    pub claim_turn_id: Option<String>,
}

/// The claim one decoded row carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIngressStoredClaim {
    pub identity: IngressClaimIdentity,
    /// The admission whose fence took the claim.
    pub admission: AdmissionId,
    /// The drive epoch the claim pins.
    pub drive_epoch: u64,
    /// The turn a checkpoint claim delivers into; `None` for an idle claim.
    pub claim_turn_id: Option<TurnId>,
}

/// One decoded row: the item and the claim facts beside it.
#[derive(Clone, Debug)]
pub struct SessionIngressStoredRow {
    pub item: IngressItem,
    pub claim: Option<SessionIngressStoredClaim>,
    pub claim_fencing_token: u64,
}

impl SessionIngressRowColumns {
    /// Decode the row, refusing one whose columns disagree with each other.
    pub fn decode(self) -> Result<SessionIngressStoredRow, StoreError> {
        let kind = IngressKind::from_wire_str(&self.kind)
            .ok_or_else(|| corrupt(format!("unknown kind `{}`", self.kind)))?;
        let delivery = Delivery::from_persisted(
            &self.delivery_scope,
            self.delivery_turn_id.as_deref(),
            self.delivery_min_boundary.as_deref(),
        )
        .ok_or_else(|| {
            corrupt(format!(
                "delivery columns disagree: scope `{}`, turn {:?}, boundary {:?}",
                self.delivery_scope, self.delivery_turn_id, self.delivery_min_boundary
            ))
        })?;
        let payload: IngressPayload = decode_json("payload_json", &self.payload_json)?;
        if payload.kind() != kind {
            return Err(corrupt(format!(
                "kind `{}` carries a `{}` payload",
                kind.as_str(),
                payload.kind().as_str()
            )));
        }
        let state = IngressState::from_wire_str(&self.state)
            .ok_or_else(|| corrupt(format!("unknown state `{}`", self.state)))?;
        let terminal_cause = self
            .terminal_cause_json
            .as_deref()
            .map(|cause| decode_json::<IngressTerminalCause>("terminal_cause_json", cause))
            .transpose()?;
        let item = IngressItem {
            item_id: IngressItemId::new(self.item_id),
            session_id: SessionId::from(self.session_id),
            enqueue_seq: unsigned("enqueue_seq", self.enqueue_seq)?,
            source_key: self.source_key,
            delivery,
            submission_digest: self.submission_digest,
            payload,
            authority: self
                .authority_json
                .as_deref()
                .map(|authority| decode_json::<QueuedWorkAuthority>("authority_json", authority))
                .transpose()?,
            merge_key: self.merge_key,
            state,
            terminal_cause,
            enqueued_at_ms: unsigned("enqueued_at_ms", self.enqueued_at_ms)?,
            terminal_at_ms: self
                .terminal_at_ms
                .map(|value| unsigned("terminal_at_ms", value))
                .transpose()?,
        };
        item.validate_lifecycle().map_err(corrupt)?;
        let claim = match (
            self.claim_id,
            self.claim_token,
            self.claim_admission_id,
            self.claim_drive_epoch,
        ) {
            (None, None, None, None) if self.claim_turn_id.is_none() => None,
            (Some(claim_id), Some(claim_token), Some(admission), Some(drive_epoch)) => {
                Some(SessionIngressStoredClaim {
                    identity: IngressClaimIdentity {
                        claim_id,
                        claim_token,
                    },
                    admission: AdmissionId::new(admission),
                    drive_epoch: unsigned("claim_drive_epoch", drive_epoch)?,
                    claim_turn_id: self.claim_turn_id.map(TurnId::from),
                })
            }
            _ => return Err(corrupt("claim columns are neither all set nor all null")),
        };
        if claim.is_some() != (state == IngressState::Accepted) {
            return Err(corrupt(format!(
                "state `{}` disagrees with the claim columns",
                state.as_str()
            )));
        }
        Ok(SessionIngressStoredRow {
            item,
            claim,
            claim_fencing_token: unsigned("claim_fencing_token", self.claim_fencing_token)?,
        })
    }
}

impl SessionIngressStoredRow {
    /// This row as a claim attempt sees it. `addressed_turn_ended` and
    /// `claim_turn_ended` are the backend's reads of the turns the row's
    /// delivery and its claim name.
    #[must_use]
    pub fn into_candidate(
        self,
        addressed_turn_ended: bool,
        claim_turn_ended: bool,
    ) -> IngressClaimCandidate {
        IngressClaimCandidate {
            item: self.item,
            claim_fencing_token: self.claim_fencing_token,
            claim: self.claim.map(|claim| IngressRowClaim {
                identity: claim.identity,
                drive_epoch: claim.drive_epoch,
                claim_turn_id: claim.claim_turn_id,
            }),
            addressed_turn_ended,
            claim_turn_ended,
        }
    }

    /// This row as a settlement observes it.
    #[must_use]
    pub fn settlement_row(&self) -> IngressSettlementRow {
        IngressSettlementRow {
            item: self.item.clone(),
            claim: self.claim.as_ref().map(|claim| claim.identity.clone()),
            claim_epoch: self.claim_epoch(),
        }
    }

    /// The drive epoch the row's claim pins, if a claim holds it.
    #[must_use]
    pub fn claim_epoch(&self) -> Option<u64> {
        self.claim.as_ref().map(|claim| claim.drive_epoch)
    }
}

/// The column values one admission writes, in
/// `lash_store_sql::session_ingress::INSERT_COLUMNS` order after the item and
/// session ids.
#[derive(Clone, Debug)]
pub struct SessionIngressInsert {
    pub lane: &'static str,
    pub kind: &'static str,
    pub source_key: Option<String>,
    pub delivery_scope: &'static str,
    pub delivery_turn_id: Option<String>,
    pub delivery_min_boundary: Option<&'static str>,
    pub submission_digest: String,
    pub payload_json: String,
    pub authority_json: Option<String>,
    pub merge_key: Option<String>,
    pub wake_process_id: Option<String>,
    pub wake_sequence: Option<u64>,
}

impl SessionIngressInsert {
    /// The columns `draft` is admitted with, under `submission_digest`.
    pub fn of(draft: &IngressItemDraft, submission_digest: String) -> Result<Self, StoreError> {
        let wake_source = draft.payload().wake_source();
        Ok(Self {
            lane: draft.kind().lane().as_str(),
            kind: draft.kind().as_str(),
            source_key: draft.source_key().map(str::to_string),
            delivery_scope: draft.delivery().scope_str(),
            delivery_turn_id: draft
                .delivery()
                .addressed_turn()
                .map(|turn_id| turn_id.as_str().to_string()),
            delivery_min_boundary: draft.delivery().min_boundary_str(),
            submission_digest,
            payload_json: encode_json(draft.payload())?,
            authority_json: draft.authority().map(encode_json).transpose()?,
            merge_key: draft.merge_key().map(str::to_string),
            wake_process_id: wake_source.map(|(process_id, _)| process_id.as_str().to_string()),
            wake_sequence: wake_source.map(|(_, sequence)| sequence),
        })
    }
}

/// Encode a tombstone's cause for `terminal_cause_json`.
pub fn encode_ingress_terminal_cause(cause: &IngressTerminalCause) -> Result<String, StoreError> {
    encode_json(cause)
}

/// Everything admission observed under the session lock, before deciding.
pub struct SessionIngressAdmissionFacts {
    /// The row the draft's source key names, open or tombstoned.
    pub by_source_key: Option<IngressItem>,
    /// The row the draft's provisioned item id names, in any session.
    pub by_item_id: Option<IngressItem>,
    /// For a wake: the session's redelivery floor for its process.
    pub wake_floor: Option<u64>,
    /// For a turn-addressed draft: whether the addressed turn is the
    /// session's running turn or one of its ended turns.
    pub turn_address_known: bool,
}

/// What admission does, decided from its facts.
#[derive(Debug)]
pub enum SessionIngressAdmission {
    /// Answer without writing.
    Answer(Box<IngressEnqueueOutcome>),
    /// A wake's conflict: raise its floor, then answer `Conflict`.
    WakeConflict {
        process_id: ProcessId,
        sequence: u64,
        existing_item_id: IngressItemId,
    },
    /// Insert the row.
    Insert,
}

/// Decide one admission (ADR 0101 §5.1, §8, §9).
///
/// The reserved-prefix refusal comes first and writes nothing. A replay
/// under the same source key or provisioned id compares digests only. A wake
/// with no row left at or below its floor is `WakeRewound`. A turn address
/// neither running nor ended is refused with nothing stored.
pub fn decide_session_ingress_admission(
    draft: &IngressItemDraft,
    submission_digest: &str,
    facts: SessionIngressAdmissionFacts,
) -> Result<SessionIngressAdmission, StoreError> {
    if let Some(source_key) = draft.reserved_source_key_violation() {
        return Err(StoreError::IngressReservedSourceKey {
            session_id: draft.session_id().clone(),
            kind: draft.kind().as_str(),
            source_key: source_key.to_string(),
        });
    }
    let matched = facts.by_source_key.or(facts.by_item_id);
    if let Some(existing) = matched {
        if existing.session_id == *draft.session_id()
            && existing.submission_digest == submission_digest
        {
            return Ok(SessionIngressAdmission::Answer(Box::new(
                IngressEnqueueOutcome::Existing(existing),
            )));
        }
        return Ok(match draft.payload().wake_source() {
            Some((process_id, sequence)) if existing.session_id == *draft.session_id() => {
                SessionIngressAdmission::WakeConflict {
                    process_id: process_id.clone(),
                    sequence,
                    existing_item_id: existing.item_id,
                }
            }
            _ => SessionIngressAdmission::Answer(Box::new(IngressEnqueueOutcome::Conflict {
                existing_item_id: existing.item_id,
            })),
        });
    }
    if let (Some((process_id, sequence)), Some(floor)) =
        (draft.payload().wake_source(), facts.wake_floor)
        && sequence <= floor
    {
        return Ok(SessionIngressAdmission::Answer(Box::new(
            IngressEnqueueOutcome::WakeRewound {
                process_id: process_id.clone(),
                sequence,
                floor,
            },
        )));
    }
    if let Some(turn_id) = draft.delivery().addressed_turn()
        && !facts.turn_address_known
    {
        return Err(StoreError::IngressTurnAddressUnknown {
            session_id: draft.session_id().clone(),
            turn_id: turn_id.clone(),
        });
    }
    Ok(SessionIngressAdmission::Insert)
}
