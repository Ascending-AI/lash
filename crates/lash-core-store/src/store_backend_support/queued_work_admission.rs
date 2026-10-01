//! The verdicts both SQL backends reach inside the one transaction that
//! admits queued work or addressed input (ADR 0101 §5.1, §8).
//!
//! A backend reads what a draft's source key already holds, and the evidence
//! for a turn an input addresses, under the session's write authority, which
//! it takes before the first read and keeps to its commit. The verdicts
//! compare only admission-time digests and recorded turn evidence, so
//! deciding them never decodes a stored payload.

use crate::store::StoreError;
use crate::{BatchId, QueuedWorkBatchDraft, SessionId, TurnId};

fn require_source_key_kind(
    session_id: &SessionId,
    source_key: Option<&str>,
    kind: &'static str,
) -> Result<(), StoreError> {
    let Some(source_key) = source_key else {
        return Ok(());
    };
    let owner = if source_key.starts_with("command:") {
        Some("session_command")
    } else if source_key.starts_with("process:") {
        Some("process_wake")
    } else {
        None
    };
    if owner.is_some_and(|owner| owner != kind) {
        return Err(StoreError::IngressReservedSourceKey {
            session_id: session_id.clone(),
            kind,
            source_key: source_key.to_owned(),
        });
    }
    Ok(())
}

/// Validate a queued producer before deduplication or allocation. Reserved
/// prefixes belong to ingress kinds, and a wake must also prove its source.
pub fn validate_queued_work_draft(draft: &QueuedWorkBatchDraft) -> Result<(), StoreError> {
    let kind = match draft.kind() {
        crate::QueuedWorkKind::Control => "session_command",
        crate::QueuedWorkKind::Turn => "process_wake",
    };
    require_source_key_kind(&draft.session_id, draft.source_key.as_deref(), kind)?;
    draft
        .validate_process_wake_source()
        .map_err(StoreError::Backend)
}

/// Inputs cannot use the command or wake namespace, even on a retry.
pub fn validate_turn_input_source_key(
    draft: &crate::PendingTurnInputDraft,
) -> Result<(), StoreError> {
    require_source_key_kind(&draft.session_id, draft.source_key.as_deref(), "input")
}

/// How one queued-work draft is answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueuedWorkDraftAdmission {
    /// A stored batch, open or a tombstone, is this very submission: the
    /// draft answers it and reopens nothing.
    Existing { batch_id: BatchId },
    /// Nothing holds the draft's source key: it is enqueued.
    New,
}

/// The verdict over `draft`, admitted under `submission_digest`, from what
/// its source key holds: `by_source_key` is the `(batch_id,
/// submission_digest)` of the batch the session filed under it, open or
/// terminal. Equal digests answer that batch; any other content is
/// [`StoreError::QueuedWorkSourceKeyConflict`], and the stored batch is
/// never silently adopted as the answer to different content.
pub fn decide_queued_work_draft_admission(
    draft: &QueuedWorkBatchDraft,
    submission_digest: &str,
    by_source_key: Option<(String, String)>,
) -> Result<QueuedWorkDraftAdmission, StoreError> {
    match (draft.source_key.as_deref(), by_source_key) {
        (Some(source_key), Some((batch_id, existing_digest))) => {
            if existing_digest != submission_digest {
                return Err(StoreError::QueuedWorkSourceKeyConflict {
                    session_id: draft.session_id.clone(),
                    source_key: source_key.to_string(),
                    existing_batch_id: batch_id.into(),
                });
            }
            Ok(QueuedWorkDraftAdmission::Existing {
                batch_id: batch_id.into(),
            })
        }
        _ => Ok(QueuedWorkDraftAdmission::New),
    }
}

/// The digest `draft` is admitted and compared under.
pub fn queued_work_submission_digest(draft: &QueuedWorkBatchDraft) -> Result<String, StoreError> {
    draft.submission_digest().map_err(|err| {
        StoreError::Backend(format!("failed to digest queued work submission: {err}"))
    })
}

/// What a store records about a turn an input addresses, read in the
/// admitting transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAddressEvidence {
    /// The turn is one of the session's unfinished root's turns, or the
    /// follow-on its head owes.
    Running,
    /// The turn's final commit is recorded, or its root has terminal
    /// evidence.
    Ended,
    /// Nothing the session records names the turn.
    Unknown,
}

/// What the session records about `turn_id`: `running` names the
/// unfinished root's turns, if a root is unfinished; `owed_follow_on` is the
/// follow-on turn the head owes, if any; `ended` is whether the turn's final
/// commit or its root's terminal evidence is recorded.
#[must_use]
pub fn turn_address_evidence(
    turn_id: &TurnId,
    running: Option<&crate::store::RootTurns>,
    owed_follow_on: Option<&crate::store::PendingFollowOn>,
    ended: bool,
) -> TurnAddressEvidence {
    if ended {
        TurnAddressEvidence::Ended
    } else if running.is_some_and(|turns| turns.contains(turn_id))
        || owed_follow_on.is_some_and(|owed| owed.is_turn(turn_id))
    {
        TurnAddressEvidence::Running
    } else {
        TurnAddressEvidence::Unknown
    }
}

/// The verdict over an input addressed to `turn_id` of `session_id`
/// (ADR 0101 §5.1): the address is accepted only if the turn is running or
/// has ended; an unknown turn is [`StoreError::IngressTurnAddressUnknown`],
/// refused before any row or sequence number is allocated.
pub fn require_known_turn_address(
    session_id: &SessionId,
    turn_id: &TurnId,
    evidence: TurnAddressEvidence,
) -> Result<(), StoreError> {
    match evidence {
        TurnAddressEvidence::Running | TurnAddressEvidence::Ended => Ok(()),
        TurnAddressEvidence::Unknown => Err(StoreError::IngressTurnAddressUnknown {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        }),
    }
}

/// The tombstone a stored row's `terminal_cause` and `terminal_at_ms`
/// columns spell: both `None` for a live row, both set for a tombstone. Any
/// other pair, or an unknown cause, is stored data the backend refuses.
pub fn decode_ingress_terminal(
    record_kind: &'static str,
    cause: Option<&str>,
    at_ms: Option<u64>,
) -> Result<Option<crate::store::IngressTerminal>, StoreError> {
    match (cause, at_ms) {
        (None, None) => Ok(None),
        (Some(cause), Some(at_ms)) => crate::store::IngressTerminalCause::from_wire_str(cause)
            .map(|cause| Some(crate::store::IngressTerminal { cause, at_ms }))
            .ok_or_else(|| StoreError::StoredDataCorrupt {
                record_kind,
                message: format!("unknown ingress terminal cause `{cause}`"),
            }),
        (cause, at_ms) => Err(StoreError::StoredDataCorrupt {
            record_kind,
            message: format!(
                "ingress terminal cause {cause:?} and time {at_ms:?} must be both set or both absent"
            ),
        }),
    }
}
