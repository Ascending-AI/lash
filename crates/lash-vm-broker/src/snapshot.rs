//! The durable store of one execution's saved states (ADR 0132 §8).
//!
//! A run is saved when it parks: [`kernel::store`](crate::kernel::store)
//! commits its state, the ledger that matches it and the admission of every
//! effect it requested, in one transaction. This module is what that
//! transaction stands on: the execution's snapshot row and its revision,
//! the identity an admitted execution takes, and the members that run the
//! admitted bodies on this activation.
//!
//! A member's identity is minted at admission and stored in the saved
//! ledger. It is never a journal position.

use std::sync::{Arc, Mutex};

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::PolicyView;
use lash_core_execution::runtime::actor::round::lifecycle::MemberBodies;
use lash_durable::CommitLabel;
use lash_durable::domain::{AdmittedId, ExecKey, Ordinal, RunSeq, SnapshotRev};
use serde::{Deserialize, Serialize};

use crate::members::Members;

/// One admitted member's identity: the run of its operation's admission
/// and its ordinal there, under its execution's owner. Minted at admission,
/// stored in the snapshot, never a journal position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationId {
    /// The run it was admitted in.
    pub run: u64,
    /// Its admission's ordinal.
    pub ordinal: u64,
}

impl OperationId {
    /// The admitted execution this member is under `exec`.
    #[must_use]
    pub fn admitted(&self, exec: &ExecKey) -> AdmittedId {
        AdmittedId {
            owner: exec.owner(),
            run: RunSeq(self.run),
            ordinal: Ordinal(self.ordinal),
        }
    }

    /// The identity of an admitted execution.
    #[must_use]
    pub fn of(admitted: &AdmittedId) -> Self {
        Self {
            run: admitted.run.0,
            ordinal: admitted.ordinal.0,
        }
    }
}

/// The durable snapshot store of one execution, over an actor's context:
/// rows in `lash_exec_snapshots` and `lash_run_records`, committed under the
/// actor's epoch. It holds the execution's admitted members on this
/// activation and runs their bodies from the host's [`MemberBodies`].
pub struct DurableSnapshotStore {
    pub(crate) cx: ActorContext,
    pub(crate) exec: ExecKey,
    /// The revision it last read or wrote.
    rev: Mutex<Option<Option<SnapshotRev>>>,
    pub(crate) members: tokio::sync::Mutex<Members>,
}

impl std::fmt::Debug for DurableSnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableSnapshotStore")
            .field("exec", &self.exec)
            .finish_non_exhaustive()
    }
}

impl DurableSnapshotStore {
    /// The snapshot store of `exec`, owned by `cx`'s actor.
    #[must_use]
    pub fn new(cx: &ActorContext, exec: ExecKey) -> Self {
        Self {
            members: tokio::sync::Mutex::new(Members::new(cx, exec.owner())),
            cx: cx.clone(),
            exec,
            rev: Mutex::new(None),
        }
    }

    /// Run its members' bodies from `bodies`, vetoing a stored `Repeatable`
    /// rerun against the policies `current` declares. Bound before the
    /// execution runs.
    pub async fn bind_members(&self, bodies: Arc<dyn MemberBodies>, current: PolicyView) {
        self.members.lock().await.with_bodies(bodies, current);
    }

    /// The context it commits through.
    #[must_use]
    pub fn context(&self) -> &ActorContext {
        &self.cx
    }

    /// The execution.
    #[must_use]
    pub fn exec(&self) -> &ExecKey {
        &self.exec
    }

    pub(crate) fn held_rev(&self) -> std::sync::MutexGuard<'_, Option<Option<SnapshotRev>>> {
        self.rev
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The revision a write replaces: the one last read or written, or the
    /// stored one.
    pub(crate) async fn revision(&self) -> Result<Option<SnapshotRev>, QuietPointRefusal> {
        if let Some(rev) = *self.held_rev() {
            return Ok(rev);
        }
        let rev = self.read().await?.map(|row| row.rev);
        *self.held_rev() = Some(rev);
        Ok(rev)
    }

    pub(crate) async fn read(
        &self,
    ) -> Result<Option<lash_durable::domain::SnapshotRow>, QuietPointRefusal> {
        self.cx
            .durable_reads()
            .map_err(refused)?
            .snapshot(&self.exec)
            .await
            .map_err(refused)
    }

    /// Commit `tx` under `label`. A refused or unacknowledged commit forgets
    /// the revision and the records the members read: the next write reads
    /// them back.
    pub(crate) async fn commit(
        &self,
        tx: lash_durable::ActorTx,
        label: CommitLabel,
        members: &mut Members,
    ) -> Result<(), QuietPointRefusal> {
        members.forget();
        match self.cx.commit(tx, label).await {
            Ok(_) => Ok(()),
            Err(error) => {
                *self.held_rev() = None;
                Err(refused(error))
            }
        }
    }
}

/// Why a store refused a save.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the save was not committed: {0}")]
pub struct QuietPointRefusal(pub String);

pub(crate) fn refused(error: impl std::fmt::Display) -> QuietPointRefusal {
    QuietPointRefusal(error.to_string())
}
