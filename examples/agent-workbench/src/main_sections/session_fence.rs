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
    /// Refused while retiring or retired: work admitted during a delete would be work the
    /// delete has to cancel, or work that commits against a tombstone.
    Use,
    /// Refused only once retired: a delete whose earlier attempt ended ambiguously left the
    /// mark at `Retiring`, and the retry is the only way to settle it.
    Delete,
}

impl AppState {
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

    /// Admit an already-resolved id for deletion on `surface`. A durable
    /// tombstone is accepted so a retry can reconcile an unconfirmed close.
    pub(crate) async fn admit_session_id_for_delete(
        &self,
        session_id: &SessionId,
        surface: &'static str,
    ) -> Result<(), AppError> {
        self.admit(session_id, surface, SessionAdmission::Delete)
            .await
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
        let durable = match self.session_builder(session_id.clone()).durable().await {
            Ok(durable) => durable,
            // Audited: durable() builds no runtime and reads nothing today; a
            // failure here is a wiring error, not a session fact.
            Err(error) => {
                return Err(AppError::internal(format!(
                    "session admission read for `{session_id}` failed: {error}"
                )));
            }
        };
        match durable.was_deleted().await {
            Ok(false) => Ok(()),
            // An unconfirmed close keeps the fence Retiring. Its retry must
            // reach the delete's settlement even if the actor closed meanwhile;
            // only use admission refuses the durable tombstone here.
            Ok(true) if admission == SessionAdmission::Delete => Ok(()),
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
            // Audited: a failed tombstone read is an untyped store error; admission cannot proceed without the fact.
            Err(error) => Err(AppError::internal(format!(
                "session admission read for `{session_id}` failed: {error}"
            ))),
        }
    }

    /// The typed refusal for a session `durable` cannot stand in for on
    /// `surface`: the tombstone for a deleted id, `UnknownSession` for one
    /// the catalog does not hold, and `Ok(())` for a live one. Reads go
    /// through the Durable Session's settled reads, so this works beside a
    /// live writer (ADR 0119).
    pub(crate) async fn admit_live_session(
        &self,
        session_id: &SessionId,
        surface: &str,
    ) -> Result<(), AppError> {
        let durable = self
            .session_builder(session_id.clone())
            .durable()
            .await
            .map_err(|error| self.session_admission_error(session_id, surface, error))?;
        if durable
            .was_deleted()
            .await
            .map_err(|error| self.session_admission_error(session_id, surface, error))?
        {
            return Err(self.session_admission_error(
                session_id,
                surface,
                lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted {
                    session_id: session_id.clone(),
                }),
            ));
        }
        if !durable
            .exists()
            .await
            .map_err(|error| self.session_admission_error(session_id, surface, error))?
        {
            return Err(self.session_admission_error(
                session_id,
                surface,
                lash::EmbedError::UnknownSession {
                    session_id: session_id.clone(),
                },
            ));
        }
        Ok(())
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
                    .with_retirement(session_id, retirement)
            }
            SessionRetirement::Retiring => AppError::conflict(retiring_session_message(session_id))
                .with_retirement(session_id, retirement),
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
            Ok(()) => self.confirm_retirement_and_rotate(session_id),
            Err(error) if error.verdict == AppErrorVerdict::Ambiguous => {}
            Err(_) => match self.session_builder(session_id.clone()).durable().await {
                Err(_) => {}
                Ok(durable) => match durable.was_deleted().await {
                    Ok(true) => self.confirm_retirement_and_rotate(session_id),
                    Ok(false) => self.active_turns.abandon_retirement(session_id),
                    Err(_) => {}
                },
            },
        }
    }

    /// Confirm a retirement and take the roster off the tombstone in the same
    /// breath.
    ///
    /// These were two facts recorded by two different owners: the mark by
    /// whoever settled the delete, the rotation only by the reset route that
    /// happened to still be awaiting its delete. A delete that completed
    /// durably while that call's result was lost — the browser's request
    /// dropped, an ambiguous attach — left the mark `Retired` and the roster's
    /// current on the tombstoned id, which every session-bound surface then
    /// refuses forever (FIG-3136). The rotation is idempotent, so the route
    /// still gets the replacement it must hand back.
    fn confirm_retirement_and_rotate(&self, session_id: &SessionId) {
        self.active_turns.confirm_retirement(session_id);
        let (replacement, replaced_current) = self.sessions.replace_retired(session_id);
        self.trace_for_session(
            session_id,
            "session.retirement_settled",
            json!({
                "session_id": session_id,
                "replacement_session_id": replacement,
                "replaced_current": replaced_current,
            }),
        );
    }
}

pub(crate) fn retiring_session_message(session_id: &SessionId) -> String {
    format!("session `{session_id}` is being deleted; session ids cannot be reused in this store")
}

/// How long a delete waits for the session's close to write its tombstone
/// before it answers that the outcome could not be confirmed.
#[cfg(not(test))]
const SESSION_DELETE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const SESSION_DELETE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// Retire `session_id` through the session's durable close, fencing it first.
///
/// The order is the contract (FIG-2358). The mark goes down before anything
/// else so no turn can claim the session from here on. The cooperative cancel
/// runs before the close is requested, because the close's first act is to
/// cancel the session's open turn and revoke its waits, and a cancel issued
/// after that has no gate to land on. Only then is the delete requested, and
/// the mark is settled against how it ended.
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
    let driver = state.core.turn_work_driver();
    let cancellations = state
        .cancel_turns_for_session_with_driver(
            session_id,
            &driver,
            WorkbenchTurnCancelMode::Abort,
            TURN_TERMINAL_ATTACH_TIMEOUT,
        )
        .await?;
    state.trace_for_session(
        session_id,
        "api.session.delete.turns_cancelled",
        json!({ "session_id": session_id, "cancellations": cancellations }),
    );
    // The delete is the session's close request, its mail (ADR 0132 §12): a
    // refused request requested nothing, so the session stays live.
    let deletion = {
        let administration = state.core.session_administration().await;
        let context = administration
            .delete_context(session_id)
            .map_err(|error| AppError::session_delete_failed(session_id, error))?;
        lash::LashCore::delete_session(context)
            .await
            .map_err(|error| AppError::session_delete_failed(session_id, error))?
    };
    if matches!(deletion, lash::SessionDeletion::Requested { .. }) {
        await_session_tombstone(state, session_id).await?;
    }
    // A deleted session's registrations deliver to nobody.
    let subscriptions = match state.host_triggers.delete_owned_by(session_id).await {
        Ok(()) => json!("deleted"),
        Err(error) => json!({ "error": error.to_string() }),
    };
    // The close detaches the global process rows the session originated
    // rather than deleting them; reclaim its finished ones so the work rail
    // does not keep a deleted session's work forever (FIG-989).
    let retention = match state.prune_processes_originated_by(session_id).await {
        Ok(report) => json!({
            "pruned_processes": report.pruned_processes,
            "pruned_events": report.pruned_events,
        }),
        Err(error) => json!({ "error": error.to_string() }),
    };
    state.trace_for_session(
        session_id,
        "api.session.delete.deleted",
        json!({
            "session_id": session_id,
            "deletion": format!("{deletion:?}"),
            "host_trigger_registrations": subscriptions,
            "process_retention": retention,
        }),
    );
    Ok(())
}

/// Wait for the close `session_id`'s delete requested to write its tombstone.
///
/// The close runs on the session's actor, one durable step at a time, and
/// owes nothing to this request: a wait that runs out leaves the delete
/// requested, so its answer is ambiguous, never "the session remains live".
async fn await_session_tombstone(state: &AppState, session_id: &SessionId) -> Result<(), AppError> {
    match tokio::time::timeout(
        SESSION_DELETE_CONFIRM_TIMEOUT,
        state.core.await_session_deletion(session_id),
    )
    .await
    {
        Ok(Ok(lash::SessionDeleteCompletion::Deleted)) => Ok(()),
        Ok(Ok(completion)) => Err(AppError::session_delete_unconfirmed(
            session_id,
            format!("its close was requested but the session reads {completion:?}"),
        )),
        Ok(Err(error)) => Err(AppError::session_delete_unconfirmed(session_id, error)),
        Err(_) => Err(AppError::session_delete_unconfirmed(
            session_id,
            format!("its close did not finish within {SESSION_DELETE_CONFIRM_TIMEOUT:?}"),
        )),
    }
}
