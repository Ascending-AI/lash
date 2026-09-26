//! The reconcile sweep (ADR 0104 O2, FIG-3600 Q11): the guaranteed owner of
//! a drive whose schedule was lost.
//!
//! Acceptance commits a row, then asks the engine to drive the session. The
//! ask is fire-and-forget, so a process that dies between the commit and the
//! ask leaves a durable row nothing drives. The sweep asks again for every
//! session that still has open ingress: the core runs it when it boots and
//! on every `drain_status`, so a lost ask is healed by the next boot, the
//! next status read, or the session's next accepted row, whichever comes
//! first. There is no timer.
//!
//! Every ask names its own drive request, derived from the sweep and the
//! row it answers, never from the session alone: the engine dedupes a
//! request id across its runs, so an ask keyed by the session could be
//! swallowed by a drive already past its last admission, and the row would
//! strand.
//!
//! The sweep reads the store; it never runs inside a drive, and it takes its
//! sweep id from the caller, which owns the clock.
//!
//! Today's open ingress is the `pending_turn_inputs` and `queued_work` rows,
//! read per session because no store answers "sessions with open ingress"
//! across the catalog. S8's switch to the one session-ingress table replaces
//! this scan with that table's cross-session read.

use crate::engine::DriveRequestId;
use crate::{SessionId, SessionListFilter, SessionStoreFactory, SessionWorkEngine, StoreError};

/// What one sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Live sessions the sweep read.
    pub scanned: usize,
    /// Sessions with open ingress, each asked for a drive, in catalog order.
    pub scheduled: Vec<SessionId>,
    /// Sessions the sweep could not read, with why. They are asked again by
    /// the next sweep.
    pub unreadable: Vec<(SessionId, String)>,
}

/// The drive request a sweep asks for when `session`'s oldest open row is
/// `row`: unique per sweep and row.
#[must_use]
pub fn reconcile_drive_request(sweep: &str, row: &str) -> DriveRequestId {
    DriveRequestId::new(format!("reconcile:{sweep}:{row}"))
}

/// Ask `engine` to drive every live session of `sessions` that still has
/// open ingress. `sweep` names this sweep (the caller's clock reading at boot
/// or at `drain_status`); asks from one sweep dedupe, asks from two do not.
///
/// A session whose store cannot be read is reported, not fatal: one broken
/// session never stops the others' recovery. Only a catalog that cannot be
/// listed fails the sweep.
pub async fn reconcile_session_work(
    sessions: &dyn SessionStoreFactory,
    engine: &dyn SessionWorkEngine,
    sweep: &str,
) -> Result<ReconcileReport, StoreError> {
    let live = sessions
        .list_sessions(&SessionListFilter {
            deleted: Some(false),
            ..SessionListFilter::default()
        })
        .await?;
    let mut report = ReconcileReport::default();
    for summary in live {
        report.scanned += 1;
        let session = summary.session_id;
        match oldest_open_row(sessions, &session).await {
            Ok(Some(row)) => {
                engine.schedule_drive(&session, reconcile_drive_request(sweep, &row));
                report.scheduled.push(session);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    session_id = session.as_str(),
                    error = %error,
                    "reconcile sweep could not read the session's open ingress"
                );
                report.unreadable.push((session, error.to_string()));
            }
        }
    }
    Ok(report)
}

/// The id of one of `session`'s open ingress rows (its oldest turn input,
/// else its oldest queued batch), if it has any.
async fn oldest_open_row(
    sessions: &dyn SessionStoreFactory,
    session: &SessionId,
) -> Result<Option<String>, StoreError> {
    let Some(store) = sessions.open_existing_store_by_id(session).await? else {
        return Ok(None);
    };
    // The row only names the ask; the drive admits whatever is open.
    let inputs = store.list_pending_turn_inputs(session).await?;
    if let Some(read) = inputs.iter().min_by_key(|read| read.input.enqueue_seq) {
        return Ok(Some(format!("input:{}", read.input.input_id)));
    }
    let batches = store.list_pending_queued_work(session).await?;
    Ok(batches
        .iter()
        .min_by_key(|batch| batch.enqueue_seq)
        .map(|batch| format!("batch:{}", batch.batch_id)))
}
