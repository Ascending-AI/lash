//! A logical root's park (FIG-3600 S7, D2 §1.3): the record an aborting root
//! writes when its abort is a refusal no redrive of the same build can get
//! past.
//!
//! A park names the **logical root**, never a physical turn: a frame switch's
//! follow-on, an S4 follow-on and a redrive all run under the root, and the
//! operator verbs (redrive, cancel, fork) act on it. Parked is durable and
//! non-terminal: the root writes no terminal evidence, keeps its claims and
//! keeps `Turn(root)` open, and admission answers `Parked` while the row
//! exists.
//!
//! A root that already has terminal evidence never parks (P2). The store
//! refuses the write with `StoreError::RootAlreadyTerminal` inside its own
//! transaction, which fences a zombie execution that resumed after an
//! operator's cancel or fork: its abort leaves nothing behind.

use crate::runtime::LashRuntime;
use crate::{RuntimeError, StoreError, TurnId};

impl LashRuntime {
    /// Record `root`'s park when its abort is a refusal that parks it
    /// (FIG-3586, FIG-3600).
    ///
    /// A parked root keeps every claim it holds, exactly as any live-fault
    /// abort does, so each redrive under the same build refuses again with
    /// nothing dispatched; the park is the typed, queryable record of why it
    /// stopped, which `drain_status` counts. Best effort, like every
    /// abort-path repair: the root is already aborting and this must not
    /// replace its error. A park the store cannot write leaves the root
    /// exactly as the abort left it — its held claims still keep the
    /// deployment from reporting drained.
    pub(in crate::runtime) async fn record_turn_park_after_abort(
        &self,
        err: &RuntimeError,
        root: &TurnId,
    ) {
        let Some(reason) = crate::store::ParkReason::of_error(err) else {
            return;
        };
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        let write = crate::store::TurnParkWrite::refusal(
            self.state.session_id.clone(),
            root.clone(),
            reason,
            self.host.core.clock.timestamp_ms(),
        );
        let reason_code = write.reason.code().as_str();
        let effect_kind = write.reason.effect_kind();
        match store.record_turn_park(&write).await {
            Ok(park) => {
                crate::operational_metrics::record_work_parked("turn", reason_code);
                tracing::warn!(
                    session_id = %self.state.session_id,
                    root = %root,
                    code = %err.code,
                    reason_code,
                    effect_kind,
                    park_id = %park.park_id,
                    attempts = park.attempts,
                    event = "turn.parked",
                    "root parked on a replay refusal; redrive it under the build that wrote its \
                     journal, cancel it, or fork from before it"
                );
            }
            // P2: an operator already ended the root. This execution is a
            // zombie of it, and its abort leaves nothing behind.
            Err(StoreError::RootAlreadyTerminal { by, .. }) => tracing::info!(
                session_id = %self.state.session_id,
                root = %root,
                code = %err.code,
                ended_by = ?by,
                event = "turn.park_refused_terminal",
                "a root with terminal evidence refused its park; the aborting execution is \
                 released"
            ),
            Err(error) => tracing::warn!(
                session_id = %self.state.session_id,
                root = %root,
                error = %error,
                event = "turn.park_record_failed",
                "failed to record the root's park; its held claims still keep the deployment \
                 from draining"
            ),
        }
    }
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
    ) -> Result<crate::engine::EngineParkRecorded, StoreError> {
        use crate::engine::{EngineParkRecorded, ParkTarget};
        let ParkTarget::Root { session, root } = target else {
            return Err(StoreError::UnsupportedStoreOperation {
                operation: "record_engine_park: a process parks through its registry",
            });
        };
        if self.sessions.root_terminal(session, root).await?.is_some() {
            return Ok(EngineParkRecorded::TargetTerminal);
        }
        let Some(store) = self.sessions.open_existing_store_by_id(session).await? else {
            return Ok(EngineParkRecorded::TargetGone);
        };
        let held = store
            .load_turn_park(session)
            .await?
            .filter(|park| park.turn_id == *root)
            .map(|park| park.park_id);
        let write = crate::store::TurnParkWrite {
            engine: Some(engine),
            ..crate::store::TurnParkWrite::refusal(
                session.clone(),
                root.clone(),
                reason,
                self.clock.timestamp_ms(),
            )
        };
        match store.record_turn_park(&write).await {
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
