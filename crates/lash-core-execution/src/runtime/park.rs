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
        let ParkTarget::Root { session, root } = target else {
            return Err(StoreError::UnsupportedStoreOperation {
                operation: "record_engine_park: a process parks through its registry",
            });
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
