//! Persistence entry points shared by aborting roots and engine recovery.
use crate::StoreError;

/// Record a root park through the store's terminal-aware transaction.
pub async fn record_root_park(
    store: &dyn crate::store::RuntimePersistence,
    write: &crate::store::TurnParkWrite,
) -> Result<crate::store::TurnPark, StoreError> {
    store.record_turn_park(write).await
}

/// The store-backed [`ParkRecoveryWriter`](crate::engine::ParkRecoveryWriter):
/// what an engine's park reconcile records a stalled root through.
///
/// A root is parked in its session's store, by the same write the aborting
/// execution itself uses, so the two converge: a divergence park the
/// execution recorded first keeps its reason and gains the engine's handle,
/// and a second reconcile pass over the same stalled execution writes
/// nothing. A root with terminal evidence answers `TargetTerminal`, and a
/// deleted session `TargetGone`: the engine releases the execution instead.
///
/// The engine names the execution by the root its drive admitted; a
/// follow-on's recovery is admitted under a name of its own, and is parked
/// under the logical root it continues, as its own abort would park it.
///
/// A park that names a redrive which already resumed the execution is
/// re-parked only when the engine confirms, after the park was read, that
/// the execution is still stopped: an engine listing read before the resume
/// is stale, and re-parking from it would clear the redrive while the root
/// runs.
///
/// Processes park through their registry, which the engine's own process
/// reconcile writes; this writer refuses a process target.
pub struct StoreParkRecovery<'a> {
    sessions: &'a dyn crate::SessionStoreFactory,
    clock: &'a dyn crate::Clock,
}

impl<'a> StoreParkRecovery<'a> {
    /// The writer over `sessions`, stamping parks with `clock`.
    pub fn new(sessions: &'a dyn crate::SessionStoreFactory, clock: &'a dyn crate::Clock) -> Self {
        Self { sessions, clock }
    }
}

#[async_trait::async_trait]
impl crate::engine::ParkRecoveryWriter for StoreParkRecovery<'_> {
    async fn record_engine_park(
        &self,
        target: &crate::engine::ParkTarget,
        reason: crate::store::ParkReason,
        engine: crate::store::EnginePark,
        execution: &dyn crate::engine::StalledExecution,
    ) -> Result<crate::engine::EngineParkRecorded, StoreError> {
        use crate::engine::{EngineParkRecorded, ParkTarget};
        let (session, root) = match target {
            ParkTarget::Root { session, root } => (session, root),
            ParkTarget::Drive { session } => {
                return self.record_drive_park(session, reason, execution).await;
            }
            ParkTarget::Process { .. } => {
                return Err(StoreError::UnsupportedStoreOperation {
                    operation: "record_engine_park: a process parks through its registry",
                });
            }
        };
        let Some(store) = self.sessions.open_existing_store_by_id(session).await? else {
            return Ok(
                if self.sessions.root_terminal(session, root).await?.is_some() {
                    EngineParkRecorded::TargetTerminal
                } else {
                    EngineParkRecorded::TargetGone
                },
            );
        };
        let root = match store.load_pending_follow_on().await? {
            Some(owed) if owed.names_recovery(root) => owed.root_turn_id(),
            _ => root.clone(),
        };
        if self.sessions.root_terminal(session, &root).await?.is_some() {
            return Ok(EngineParkRecorded::TargetTerminal);
        }
        let held = store
            .load_turn_park(session)
            .await?
            .filter(|park| park.turn_id == root);
        let mut after_redrive = None;
        if let Some(intent) = held.as_ref().and_then(|park| park.resume_intent)
            && !self
                .sessions
                .load_intent(intent)
                .await?
                .is_some_and(|intent| intent.state.is_open())
        {
            // The redrive already resumed the execution: the engine listed
            // it before or after. Only an execution still stopped now
            // stopped again after the resume.
            if !execution
                .still_stopped()
                .await
                .map_err(|refusal| StoreError::Backend(refusal.to_string()))?
            {
                return Ok(EngineParkRecorded::Redriven);
            }
            after_redrive = Some(intent);
        }
        let write = crate::store::TurnParkWrite {
            engine: Some(engine),
            after_redrive,
            ..crate::store::TurnParkWrite::refusal(
                session.clone(),
                root.clone(),
                reason,
                self.clock.timestamp_ms(),
            )
        };
        let held = held.map(|park| park.park_id);
        match store.record_turn_park(&write).await {
            Ok(park) if park.resume_intent.is_some() => Ok(EngineParkRecorded::Redriven),
            Ok(park) if held == Some(park.park_id) => {
                Ok(EngineParkRecorded::AttachedToExisting(park.park_id))
            }
            Ok(park) => {
                crate::operational_metrics::record_work_parked("turn", park.reason.code().as_str());
                tracing::warn!(
                    session_id = %session,
                    root = %root,
                    park_id = %park.park_id,
                    reason_code = park.reason.code().as_str(),
                    event = "turn.parked",
                    "a root whose engine stopped retrying is parked; redrive it, cancel it, or \
                     fork from before it"
                );
                Ok(EngineParkRecorded::Parked(park.park_id))
            }
            Err(StoreError::RootAlreadyTerminal { .. }) => Ok(EngineParkRecorded::TargetTerminal),
            Err(StoreError::SessionDeleted { .. }) => Ok(EngineParkRecorded::TargetGone),
            Err(error) => Err(error),
        }
    }
}

impl StoreParkRecovery<'_> {
    /// Park a session's stopped drive (ADR 0109 §3): a drive the engine
    /// stopped retrying in its admission is never resumed blindly. It waits
    /// on the session's park, and the park's operator verb resumes it.
    ///
    /// A session already parked keeps its park: the drive stopped behind it,
    /// and the verb that resolves it resumes the drive too. A redrive still
    /// open owns the session. A settled redrive after which the drive is
    /// still stopped re-parks its root, so the operator can act again.
    /// Otherwise the root the session's next admission names is parked, with
    /// no engine handle: the engine finds the stopped drive by its session.
    async fn record_drive_park(
        &self,
        session: &crate::SessionId,
        reason: crate::store::ParkReason,
        execution: &dyn crate::engine::StalledExecution,
    ) -> Result<crate::engine::EngineParkRecorded, StoreError> {
        use crate::engine::EngineParkRecorded;
        let Some(store) = self.sessions.open_existing_store_by_id(session).await? else {
            return Ok(EngineParkRecorded::TargetGone);
        };
        let still_stopped = || async {
            execution
                .still_stopped()
                .await
                .map_err(|refusal| StoreError::Backend(refusal.to_string()))
        };
        let (root, after_redrive) = match store.load_turn_park(session).await? {
            Some(park) => match park.resume_intent {
                None => return Ok(EngineParkRecorded::AttachedToExisting(park.park_id)),
                Some(intent) => {
                    let open = self
                        .sessions
                        .load_intent(intent)
                        .await?
                        .is_some_and(|intent| intent.state.is_open());
                    if open || !still_stopped().await? {
                        return Ok(EngineParkRecorded::Redriven);
                    }
                    (park.turn_id, Some(intent))
                }
            },
            None => {
                let Some(root) = next_admission_root(store.as_ref(), session).await? else {
                    return Ok(EngineParkRecorded::NothingToPark);
                };
                // A listing read before an operator resumed the drive is
                // stale: parking from it would park a running session.
                if !still_stopped().await? {
                    return Ok(EngineParkRecorded::Redriven);
                }
                (root, None)
            }
        };
        let write = crate::store::TurnParkWrite {
            after_redrive,
            ..crate::store::TurnParkWrite::refusal(
                session.clone(),
                root.clone(),
                reason,
                self.clock.timestamp_ms(),
            )
        };
        match store.record_turn_park(&write).await {
            Ok(park) if park.resume_intent.is_some() => Ok(EngineParkRecorded::Redriven),
            Ok(park) => {
                crate::operational_metrics::record_work_parked("turn", park.reason.code().as_str());
                tracing::warn!(
                    session_id = %session,
                    root = %root,
                    park_id = %park.park_id,
                    reason_code = park.reason.code().as_str(),
                    event = "session.drive.parked",
                    "a session drive the engine stopped retrying is parked on its next root; \
                     redrive it, cancel it, or fork from before it"
                );
                Ok(EngineParkRecorded::Parked(park.park_id))
            }
            Err(StoreError::RootAlreadyTerminal { .. }) => Ok(EngineParkRecorded::NothingToPark),
            Err(StoreError::SessionDeleted { .. }) => Ok(EngineParkRecorded::TargetGone),
            Err(error) => Err(error),
        }
    }
}

/// The root the session's next admission names, when its next work carries
/// one: an unfinished queued run, then an owed follow-on's recovery, then the
/// head turn input — unless a session command precedes it, whose queued run
/// is named by the admission that mints it. Mirrors the drive's own
/// admission order (ADR 0101 §4), so a park written here is cleared by the
/// commit of the root the resumed drive runs.
async fn next_admission_root(
    store: &dyn crate::store::RuntimePersistence,
    session: &crate::SessionId,
) -> Result<Option<crate::TurnId>, StoreError> {
    if let Some(run) = store.pending_queued_run(session).await? {
        return Ok(Some(crate::TurnId::from(run.scope.id())));
    }
    if let Some(owed) = store.load_pending_follow_on().await? {
        return Ok(Some(owed.recovery_root()));
    }
    if store
        .pending_session_work_ordering(session)
        .await?
        .session_command_precedes_turn_input()
    {
        return Ok(None);
    }
    let open = store.list_pending_turn_inputs(session).await?;
    let Some(head) = head_input(&open) else {
        return Ok(None);
    };
    let bound = store.root_binding(session, &head.input.input_id).await?;
    Ok(Some(head_input_root(head, bound)))
}

/// The head of the session's accepted next-turn input, among its `open`
/// inputs: the oldest one deferred to the next turn.
#[must_use]
pub fn head_input(open: &[crate::PendingTurnInputRead]) -> Option<&crate::PendingTurnInputRead> {
    open.iter()
        .filter(|read| read.input.state == crate::TurnInputState::DeferredNextTurn)
        .min_by_key(|read| read.input.enqueue_seq)
}

/// The root the head input `head` runs under, given the root its store
/// binding names (`bound`): that root (the root whose claim took it, or the
/// new root a fork bound it to, FIG-3600 S7), then the turn an aborted
/// execution bound it to (FIG-3589), then [`input_root`].
#[must_use]
pub fn head_input_root(
    head: &crate::PendingTurnInputRead,
    bound: Option<crate::TurnId>,
) -> crate::TurnId {
    match (bound, &head.status) {
        (Some(root), _) => root,
        (None, crate::PendingTurnInputReadStatus::TurnBound { turn_id, .. }) => turn_id.clone(),
        (None, _) => input_root(&head.input),
    }
}

/// The root of a drive that starts with `input`: the host's id for it (its
/// source key) when it has one, else its input id (FIG-3600, ruling Q4).
#[must_use]
pub fn input_root(input: &crate::PendingTurnInput) -> crate::TurnId {
    crate::TurnId::from(
        input
            .source_key
            .as_deref()
            .unwrap_or_else(|| input.input_id.as_str()),
    )
}

/// Refuse destructive control while the root owns live or closing groups.
pub async fn require_root_groups_closed(
    host: Option<&dyn crate::EffectHost>,
    request: &crate::store::RootIntentRequest,
) -> Result<(), crate::store::RootIntentRefused> {
    if request.verb == crate::store::RootVerb::Redrive {
        return Ok(());
    }
    if let Some(closing) = host.and_then(|host| host.effect_group_closing()) {
        let mut count = 0;
        for scope in [
            crate::ExecutionScope::turn(request.session_id.clone(), request.root.clone()),
            crate::ExecutionScope::queue_drain(
                request.session_id.clone(),
                request.root.to_string(),
            ),
        ] {
            count += closing
                .read_unsettled_groups(&scope)
                .await
                .map_err(|error| crate::StoreError::Backend(error.to_string()))?
                .len();
        }
        if count != 0 {
            return Err(crate::store::RootIntentRefused::EffectGroupsOpen { count });
        }
    }
    Ok(())
}
