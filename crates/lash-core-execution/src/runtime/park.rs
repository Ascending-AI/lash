//! Persistence entry points shared by aborting roots and engine recovery.
use crate::StoreError;

/// Woken once per root park this process records, after the write committed.
static ROOT_PARK_RECORDED: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

/// The next root park this process records, in any session: a wake, never an
/// answer. Whoever waits on a root reads its park from the store; enable the
/// wake before that read, so a park committed between the read and the wait
/// is not missed. A park recorded in another process wakes nothing here.
pub fn root_park_recorded() -> tokio::sync::futures::Notified<'static> {
    ROOT_PARK_RECORDED.notified()
}

/// Record a root park through the store's terminal-aware transaction, and
/// wake whoever waits on [`root_park_recorded`] once it committed.
pub async fn record_root_park(
    store: &dyn crate::store::RuntimeStore,
    write: &crate::store::TurnParkWrite,
) -> Result<crate::store::TurnPark, StoreError> {
    let park = store.record_turn_park(write).await?;
    ROOT_PARK_RECORDED.notify_waiters();
    Ok(park)
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
/// Stopped work a root waits on ([`ParkTarget::RootChild`](crate::engine::ParkTarget::RootChild))
/// parks its root and is recorded on the park by its engine handle
/// (FIG-4630). A root already parked keeps its park and gains the handle, so
/// any number of stopped children of one root, over any number of passes,
/// write one park that names each of them once. A redrive resumes the
/// children its park recorded when it was requested, and owns them until it
/// did; a child still stopped after a settled redrive re-parks the root, so
/// the operator can act again.
///
/// Every write here is a reconcile write
/// ([`TurnParkOrigin::Reconcile`](crate::store::TurnParkOrigin::Reconcile)),
/// decided against the stored park in the store's own transaction
/// (FIG-4626). What this writer reads before it only spares a write or
/// probes the engine: a park or a redrive that lands between its read and
/// its write is the store's to see, so passes that race each other, or an
/// operator's redrive, never count a refusal twice or settle a redrive
/// whose engine half has not run.
///
/// Processes park through their registry, which the engine's own process
/// reconcile writes; this writer refuses a process target.
pub struct StoreParkRecovery<'a> {
    sessions: &'a dyn crate::DeploymentStore,
    clock: &'a dyn crate::Clock,
}

impl<'a> StoreParkRecovery<'a> {
    /// The writer over `sessions`, stamping parks with `clock`.
    pub fn new(sessions: &'a dyn crate::DeploymentStore, clock: &'a dyn crate::Clock) -> Self {
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
        let (session, root, engine, child) = match target {
            ParkTarget::Root { session, root } => (session, root, Some(engine), None),
            ParkTarget::RootChild { session, root } => (session, root, None, Some(engine)),
            ParkTarget::Drive { session } => {
                return self.record_drive_park(session, reason, execution).await;
            }
            ParkTarget::Process { .. } => {
                return Err(StoreError::UnsupportedStoreOperation {
                    operation: "record_engine_park: a process parks through its registry",
                });
            }
        };
        if !super::session_is_live(self.sessions, session).await? {
            return Ok(
                if self.sessions.root_terminal(session, root).await?.is_some() {
                    EngineParkRecorded::TargetTerminal
                } else {
                    EngineParkRecorded::TargetGone
                },
            );
        }
        let store = self.sessions;
        let root = match store.load_pending_follow_on(session).await? {
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
        let still_stopped = || async {
            execution
                .still_stopped()
                .await
                .map_err(|refusal| StoreError::Backend(refusal.to_string()))
        };
        let mut after_redrive = None;
        match held.as_ref() {
            Some(park) => match park.resume_intent {
                Some(intent) => {
                    let open = self
                        .sessions
                        .load_intent(intent)
                        .await?
                        .is_some_and(|intent| intent.engine_half_owed());
                    if open {
                        // The redrive owns the stopped work until it resumes
                        // it: the store would leave the park as it is.
                        return Ok(EngineParkRecorded::Redriven);
                    } else {
                        // The redrive already resumed the execution: the
                        // engine listed it before or after. Only an execution
                        // still stopped now stopped again after the resume.
                        if !still_stopped().await? {
                            return Ok(EngineParkRecorded::Redriven);
                        }
                        after_redrive = Some(intent);
                    }
                }
                // The root is parked and no redrive ran since: a child the
                // park already records adds nothing to it, and the store
                // adds the handle of one it does not.
                None if child
                    .as_ref()
                    .is_some_and(|child| park.children.contains(child)) =>
                {
                    return Ok(EngineParkRecorded::AttachedToExisting(park.park_id));
                }
                None => {}
            },
            // A listing read before an operator resumed the child is stale:
            // parking from it would park a running root.
            None if child.is_some() && !still_stopped().await? => {
                return Ok(EngineParkRecorded::Redriven);
            }
            None => {}
        }
        let at_ms = self.clock.timestamp_ms();
        let write = match child {
            Some(child) => crate::store::TurnParkWrite::reconcile_child(
                session.clone(),
                root.clone(),
                reason,
                at_ms,
                child,
                after_redrive,
            ),
            None => crate::store::TurnParkWrite::reconcile(
                session.clone(),
                root.clone(),
                reason,
                at_ms,
                engine,
                after_redrive,
            ),
        };
        let held = held.map(|park| park.park_id);
        match record_root_park(store, &write).await {
            Ok(park) if park.resume_intent.is_some() => Ok(EngineParkRecorded::Redriven),
            Ok(park) if held == Some(park.park_id) || !records(&park, &write) => {
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
            Err(StoreError::RootInputWithdrawn { .. }) => Ok(EngineParkRecorded::TargetGone),
            Err(StoreError::SessionDeleted { .. }) => Ok(EngineParkRecorded::TargetGone),
            Err(error) => Err(error),
        }
    }
}

/// Whether `park`, as the store answered `write`, records that write's
/// refusal: the store opened or re-parked it. A park another writer recorded
/// between this writer's read and its write is left as it was, and carries
/// that writer's reason and instant.
fn records(park: &crate::store::TurnPark, write: &crate::store::TurnParkWrite) -> bool {
    park.last_refused_ms == write.at_ms && park.reason == write.reason
}

impl StoreParkRecovery<'_> {
    /// Park a session's stopped drive (ADR 0109 §3): a drive the engine
    /// stopped retrying in its admission is never resumed blindly. It waits
    /// on the session's park, and the park's operator verb resumes it.
    ///
    /// A session already parked keeps its park: the drive stopped behind it,
    /// and the verb that resolves it resumes the drive too. A redrive still
    /// open owns the session. A drive whose every attempt was refused only
    /// because that redrive had not settled (D15) waited on the redrive, not
    /// on an operator: once the redrive settles — its park still held, or
    /// already cleared by its root's commit — the engine resumes the drive.
    /// Any other drive still stopped after a settled redrive re-parks its
    /// root, so the operator can act again. Otherwise the root the session's
    /// next admission names is parked, with no engine handle: the engine
    /// finds the stopped drive by its session. A drive whose next work names
    /// no root is released: its session's ingress obligations ask again.
    async fn record_drive_park(
        &self,
        session: &crate::SessionId,
        reason: crate::store::ParkReason,
        execution: &dyn crate::engine::StalledExecution,
    ) -> Result<crate::engine::EngineParkRecorded, StoreError> {
        use crate::engine::EngineParkRecorded;
        if !super::session_is_live(self.sessions, session).await? {
            return Ok(EngineParkRecorded::TargetGone);
        }
        let store = self.sessions;
        let still_stopped = || async {
            execution
                .still_stopped()
                .await
                .map_err(|refusal| StoreError::Backend(refusal.to_string()))
        };
        let behind_redrive = reason.stopped_behind_unsettled_redrive();
        let (root, after_redrive) = match store.load_turn_park(session).await? {
            Some(park) => match park.resume_intent {
                None => return Ok(EngineParkRecorded::AttachedToExisting(park.park_id)),
                Some(intent) => {
                    let open = self
                        .sessions
                        .load_intent(intent)
                        .await?
                        .is_some_and(|intent| intent.engine_half_owed());
                    if open || !still_stopped().await? {
                        return Ok(EngineParkRecorded::Redriven);
                    }
                    if behind_redrive {
                        return Ok(EngineParkRecorded::ResumeDrive);
                    }
                    (park.turn_id, Some(intent))
                }
            },
            // The redrive the drive stopped behind settled, and its root's
            // commit already cleared the park.
            None if behind_redrive => {
                return Ok(if still_stopped().await? {
                    EngineParkRecorded::ResumeDrive
                } else {
                    EngineParkRecorded::Redriven
                });
            }
            None => {
                let Some(root) = next_admission_root(store, session).await? else {
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
        let write = crate::store::TurnParkWrite::reconcile(
            session.clone(),
            root.clone(),
            reason,
            self.clock.timestamp_ms(),
            None,
            after_redrive,
        );
        match record_root_park(store, &write).await {
            Ok(park) if park.resume_intent.is_some() => Ok(EngineParkRecorded::Redriven),
            Ok(park) if !records(&park, &write) => {
                Ok(EngineParkRecorded::AttachedToExisting(park.park_id))
            }
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
            Err(StoreError::RootInputWithdrawn { .. }) => Ok(EngineParkRecorded::NothingToPark),
            Err(StoreError::SessionDeleted { .. }) => Ok(EngineParkRecorded::TargetGone),
            Err(error) => Err(error),
        }
    }
}

/// The root the session's next admission names, when its next work carries
/// one: an owed follow-on's recovery, then the unfinished root, then the head
/// turn input — unless an open session command or earlier queued work comes
/// first, whose root is named by the admission that mints it. Mirrors the
/// drive's own admission order (ADR 0101 §4, §5), so a park written here is
/// cleared by the commit of the root the resumed drive runs.
async fn next_admission_root(
    store: &dyn crate::store::RuntimeStore,
    session: &crate::SessionId,
) -> Result<Option<crate::TurnId>, StoreError> {
    if let Some(owed) = store.load_pending_follow_on(session).await? {
        return Ok(Some(owed.recovery_root()));
    }
    if let Some(unfinished) = store.unfinished_root(session).await? {
        return Ok(Some(unfinished.root));
    }
    if store
        .pending_session_work_ordering(session)
        .await?
        .session_command_precedes_turn_input()
    {
        return Ok(None);
    }
    let open = store.list_pending_turn_inputs(session).await?;
    let queued = store.list_open_queued_work(session).await?;
    let Some(TurnLaneHead::Input(head)) = turn_lane_head(&open, &queued) else {
        return Ok(None);
    };
    let bound = store.root_binding(session, &head.input.input_id).await?;
    Ok(Some(head_input_root(head, bound)))
}

/// What the session's turn lane admits next once no session command is open
/// (ADR 0101 §5): the host input and
/// the queued work pending in the two admission tables take one per-session
/// `enqueue_seq`, and the earlier of the head next-turn input and the
/// earliest pending queued turn work goes first. There is no kind priority.
#[derive(Clone, Copy, Debug)]
pub enum TurnLaneHead<'a> {
    /// The head of the accepted next-turn input.
    Input(&'a crate::PendingTurnInputRead),
    /// The earliest pending queued turn work, which heads a queued root.
    Queued(&'a crate::QueuedWorkBatch),
}

/// The turn lane's next item among the session's `open` inputs and pending
/// `queued` batches (see [`TurnLaneHead`]); `None` when both are empty. Only
/// turn work heads the lane: session commands drain before it (ADR 0101 §4).
#[must_use]
pub fn turn_lane_head<'a>(
    open: &'a [crate::PendingTurnInputRead],
    queued: &'a [crate::QueuedWorkBatch],
) -> Option<TurnLaneHead<'a>> {
    let earliest_queued = queued
        .iter()
        .filter(|batch| batch.work_class() == crate::store::QueuedWorkClass::TurnWork)
        .min_by_key(|batch| batch.enqueue_seq);
    match (head_input(open), earliest_queued) {
        (Some(head), Some(queued)) if queued.enqueue_seq < head.input.enqueue_seq => {
            Some(TurnLaneHead::Queued(queued))
        }
        (Some(head), _) => Some(TurnLaneHead::Input(head)),
        (None, Some(queued)) => Some(TurnLaneHead::Queued(queued)),
        (None, None) => None,
    }
}

/// The head of the session's accepted next-turn input, among its `open`
/// inputs at idle: the oldest one. With no turn running, every open input is
/// next-turn input, whatever turn its submitted delivery addresses (ADR 0101
/// §5.1).
#[must_use]
pub fn head_input(open: &[crate::PendingTurnInputRead]) -> Option<&crate::PendingTurnInputRead> {
    open.iter()
        .filter(|read| read.input.state.is_next_turn_input(None))
        .min_by_key(|read| read.input.enqueue_seq)
}

/// The root the head input `head` runs under, given the root its store
/// binding names (`bound`): that root (the root whose admission took it, or the
/// new root a fork bound it to, FIG-3600 S7), else [`input_root`].
#[must_use]
pub fn head_input_root(
    head: &crate::PendingTurnInputRead,
    bound: Option<crate::TurnId>,
) -> crate::TurnId {
    bound.unwrap_or_else(|| input_root(&head.input))
}

/// The root of a drive that starts with `input`: the host's id for it (its
/// source key) when it has one, else its input id (FIG-3600, ruling Q4).
#[must_use]
pub fn input_root(input: &crate::PendingTurnInput) -> crate::TurnId {
    input
        .source_key
        .as_deref()
        .and_then(|source_key| crate::TurnId::parse(source_key).ok())
        .unwrap_or_else(|| crate::TurnId::from(&input.input_id))
}
