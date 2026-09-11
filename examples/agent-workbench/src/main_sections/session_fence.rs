use super::*;
use lash::SessionId;

// The workbench's session fence: one admission read that every session-bound
// surface resolves its id through, and the delete sequence that orders itself
// through the same fence.
//
// Two authorities decide whether a session may still be used, and both are
// consulted here and nowhere else:
//
// * the in-process retirement mark in `ActiveTurns`, which a delete places
//   before it starts and which the turn claim reads under the same lock, so a
//   submit that races a delete is either cancelled by that delete or refused;
// * the durable session tombstone, which outlives this process and is the
//   authority after a restart.
//
// A refusal from either is the same typed `409` the turn workflows already
// return for a deleted session, so a client sees one shape whether it was
// refused at the route, at the claim, or inside the durable turn.

/// What a surface intends to do with the session it is admitting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionAdmission {
    /// Read the session or submit work to it. Refused while retiring or
    /// retired: work admitted during a delete would be work the delete has to
    /// cancel, or work that commits against a tombstone.
    Use,
    /// Delete the session. Refused only once retired: a delete whose earlier
    /// attempt ended ambiguously left the mark at `Retiring`, and the retry is
    /// the only way to settle it.
    Delete,
}

impl AppState {
    /// Resolve `query` to a session id and admit it for use on `surface`.
    ///
    /// This is the one admission read. Session-bound routes call it first, so a
    /// retired id is refused before the route reads state, pushes a message,
    /// delivers mail, or submits a workflow.
    pub(crate) async fn admit_session(
        &self,
        query: &SessionQuery,
        surface: &'static str,
    ) -> Result<SessionId, AppError> {
        let session_id = query.resolve(self)?;
        self.admit_session_id(&session_id, surface).await?;
        Ok(session_id)
    }

    /// Admit an already-resolved id for use on `surface`; the workflow entry
    /// points use this so a session retired between ingress and execution is a
    /// typed refusal rather than a commit against a tombstone.
    pub(crate) async fn admit_session_id(
        &self,
        session_id: &SessionId,
        surface: &str,
    ) -> Result<(), AppError> {
        self.admit(session_id, surface, SessionAdmission::Use).await
    }

    /// Resolve `query` and admit the id for deletion on `surface`.
    pub(crate) async fn admit_session_for_delete(
        &self,
        query: &SessionQuery,
        surface: &'static str,
    ) -> Result<SessionId, AppError> {
        let session_id = query.resolve(self)?;
        self.admit(&session_id, surface, SessionAdmission::Delete)
            .await?;
        Ok(session_id)
    }

    async fn admit(
        &self,
        session_id: &SessionId,
        surface: &str,
        admission: SessionAdmission,
    ) -> Result<(), AppError> {
        match (self.active_turns.retirement(session_id), admission) {
            (Some(retirement @ SessionRetirement::Retired), _)
            | (Some(retirement @ SessionRetirement::Retiring), SessionAdmission::Use) => {
                return Err(self.retirement_fence_refusal(session_id, surface, retirement));
            }
            (Some(SessionRetirement::Retiring), SessionAdmission::Delete) | (None, _) => {}
        }
        match self
            .session_store_factory
            .session_was_deleted(session_id)
            .await
        {
            Ok(false) => Ok(()),
            // Not memoized into the in-process mark: the evidence a refusal
            // records names the authority that was consulted, and for a
            // tombstoned session that authority is the store.
            Ok(true) => Err(self.session_admission_error(
                session_id,
                surface,
                lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted {
                    session_id: session_id.clone(),
                }),
            )),
            // Audited: a failed tombstone read is an untyped factory/backend error; admission cannot proceed without the fact.
            Err(error) => Err(AppError::internal(format!(
                "session admission read for `{session_id}` failed: {error}"
            ))),
        }
    }

    pub(crate) fn retirement_fence_refusal(
        &self,
        session_id: &SessionId,
        surface: &str,
        retirement: SessionRetirement,
    ) -> AppError {
        self.trace_for_session(
            session_id,
            "session.admission_refused",
            json!({
                "session_id": session_id,
                "surface": surface,
                "consulted_state": {
                    "kind": "workbench_retirement_fence",
                    "freshness": "admission_read",
                    "session_id": session_id,
                },
                "tombstone_outcome": retirement,
                "outcome": "refused",
                "store_context": Value::Null,
            }),
        );
        match retirement {
            SessionRetirement::Retired => {
                log_deleted_session_refusal(session_id, Some("workbench_retirement_fence"));
                AppError::conflict(deleted_session_message(session_id))
            }
            SessionRetirement::Retiring => AppError::conflict(retiring_session_message(session_id)),
        }
    }

    /// Bring the in-process mark in line with how a delete attempt ended.
    ///
    /// Success confirms the mark. An ambiguous outcome keeps it at `Retiring`,
    /// which is what refuses new work until a retry settles the question. Any
    /// other failure follows the durable fact: a tombstone that did commit is
    /// confirmed, and a session that is provably still live has its mark lifted
    /// so it can be used again.
    pub(crate) async fn settle_retirement_mark(
        &self,
        session_id: &SessionId,
        outcome: &Result<(), AppError>,
    ) {
        match outcome {
            Ok(()) => self.active_turns.confirm_retirement(session_id),
            Err(error) if error.verdict == AppErrorVerdict::Ambiguous => {}
            Err(_) => match self
                .session_store_factory
                .session_was_deleted(session_id)
                .await
            {
                Ok(true) => self.active_turns.confirm_retirement(session_id),
                Ok(false) => self.active_turns.abandon_retirement(session_id),
                Err(_) => {}
            },
        }
    }
}

pub(crate) fn retiring_session_message(session_id: &SessionId) -> String {
    format!("session `{session_id}` is being deleted; session ids cannot be reused in this store")
}

/// Retire `session_id` through the durable delete workflow, fencing it first.
///
/// The order is the contract (FIG-2358). The mark goes down before anything
/// else so no turn can claim the session from here on. The cooperative cancel
/// runs before the workflow, because the workflow's first act is to revoke the
/// session's await gates and a cancel issued after that revoke has no gate to
/// land on. Only then is the delete submitted, and the mark is settled against
/// how it ended.
pub(crate) async fn retire_session(
    state: &AppState,
    session_id: &SessionId,
) -> Result<(), AppError> {
    state.active_turns.begin_retirement(session_id);
    let outcome = retire_session_attempt(state, session_id).await;
    state.settle_retirement_mark(session_id, &outcome).await;
    outcome
}

async fn retire_session_attempt(state: &AppState, session_id: &SessionId) -> Result<(), AppError> {
    restate::cancel_cron_jobs_for_session(state, session_id, "reset").await?;
    let driver = state.core.turn_work_driver().map_err(AppError::internal)?;
    let cancellations = state
        .cancel_turns_for_session_with_driver(session_id, &driver, WorkbenchTurnCancelMode::Abort)
        .await?;
    state.trace_for_session(
        session_id,
        "api.session.delete.turns_cancelled",
        json!({ "session_id": session_id, "cancellations": cancellations }),
    );
    let execution_scope = lash::runtime::ExecutionScope::session_delete(session_id);
    restate::call_session_delete(
        state,
        restate::WorkbenchSessionDeleteWorkflowRequest {
            operation_id: format!("workbench-delete-{}", uuid::Uuid::new_v4()),
            session_id: session_id.clone(),
            execution_scope,
        },
    )
    .await
}
