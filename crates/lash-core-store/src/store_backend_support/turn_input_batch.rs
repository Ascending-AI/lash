//! The verdict both SQL backends reach for each draft of a turn-input batch,
//! inside the one transaction that admits the batch (FIG-3842).
//!
//! A backend reads what the draft's names already hold under the session's
//! write authority, which it takes before the first read and keeps to its
//! commit: the row the session filed under the draft's source key, and
//! otherwise the row holding the draft's provisioned input id. The verdict
//! compares only admission-time submission digests, so deciding it never
//! decodes a stored input.

use crate::PendingTurnInputDraft;
use crate::store::StoreError;

/// How one draft of a batch is answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnInputDraftAdmission {
    /// A stored row is this very submission, filed earlier: the draft answers
    /// it, wherever it sits and whatever became of it.
    Existing { input_id: String },
    /// Nothing holds the draft's names: it is enqueued.
    New,
}

/// The verdict over one draft, from what its names hold: `by_source_key` is
/// the `(input_id, submission_digest)` of the row the session filed under the
/// draft's source key, and `by_input_id` the `(session_id,
/// submission_digest)` of the row holding its provisioned input id, in any
/// session. A backend reads `by_input_id` only when `by_source_key` is empty.
///
/// A source key answers a draft whose digest equals the row's; any other
/// content is [`StoreError::PendingTurnInputSourceKeyConflict`]. A provisioned
/// id names one admission (ADR 0069 §6): the same submission in the same
/// session re-runs it, and anything else, including the same id in another
/// session, is [`StoreError::PendingTurnInputIdConflict`].
pub fn decide_turn_input_draft_admission(
    draft: &PendingTurnInputDraft,
    submission_digest: &str,
    by_source_key: Option<(String, String)>,
    by_input_id: Option<(String, String)>,
) -> Result<TurnInputDraftAdmission, StoreError> {
    if let (Some(source_key), Some((input_id, existing_digest))) =
        (draft.source_key.as_deref(), by_source_key)
    {
        if existing_digest != submission_digest {
            return Err(StoreError::PendingTurnInputSourceKeyConflict {
                session_id: draft.session_id.clone(),
                source_key: source_key.to_string(),
                existing_input_id: input_id.into(),
            });
        }
        return Ok(TurnInputDraftAdmission::Existing { input_id });
    }
    if let (Some(input_id), Some((holder, existing_digest))) =
        (draft.input_id.as_deref(), by_input_id)
    {
        if holder != draft.session_id.as_str() || existing_digest != submission_digest {
            return Err(StoreError::PendingTurnInputIdConflict {
                session_id: draft.session_id.clone(),
                input_id: input_id.into(),
            });
        }
        return Ok(TurnInputDraftAdmission::Existing {
            input_id: input_id.to_string(),
        });
    }
    Ok(TurnInputDraftAdmission::New)
}

/// The digest `draft` is admitted and compared under.
pub fn turn_input_submission_digest(draft: &PendingTurnInputDraft) -> Result<String, StoreError> {
    draft.submission_digest().map_err(|err| {
        StoreError::Backend(format!(
            "failed to digest pending turn input submission: {err}"
        ))
    })
}
