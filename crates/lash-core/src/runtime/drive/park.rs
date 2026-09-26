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
    /// The logical root a park of the running turn names, and so the root
    /// whose park the turn's commit clears (D2 §1.3): the admitted root's,
    /// when a drive runs one — the root a follow-on recovery ends, not the
    /// recovery's admission name — else `scope_root`, the root the turn's
    /// controller runs under, else the physical `turn` itself.
    pub(in crate::runtime) fn park_root(
        &self,
        scope_root: Option<TurnId>,
        turn: &TurnId,
    ) -> TurnId {
        self.drive_root
            .as_ref()
            .map(|run| run.root().clone())
            .or(scope_root)
            .unwrap_or_else(|| turn.clone())
    }

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
        // Under a drive the park names the admitted root's logical root,
        // whatever name the aborting site knew it by.
        let root = &self.park_root(Some(root.clone()), root);
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
        match lash_core_execution::runtime::record_root_park(store.as_ref(), &write).await {
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

pub use lash_core_execution::runtime::StoreParkRecovery;
