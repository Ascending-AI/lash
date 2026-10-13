//! The durable store of one execution's saved states (ADR 0132 §8).
//!
//! A run is saved when it parks: [`kernel::store`](crate::kernel::store)
//! commits its state, the ledger that matches it and the admission of every
//! effect it requested, in one transaction, as the execution's root graph.
//! This module is what that transaction stands on: the execution's root row
//! and its revision, the admissions its activation knows, the identity an
//! admitted execution takes, and the members that run the admitted bodies
//! on this activation.
//!
//! A member's identity is minted at admission and stored in its admission.
//! It is never a journal position.

use std::sync::{Arc, Mutex};

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::claim_obligations::ExecutionDebts;
use lash_core_execution::runtime::actor::round::PolicyView;
use lash_core_execution::runtime::actor::round::lifecycle::MemberBodies;
use lash_core_store::checkpoint_graph::{DecodeLimits, NodeId};
use lash_durable::CommitLabel;
use lash_durable::domain::{AdmittedId, ExecKey, ExecRootRow, Ordinal, RunSeq, SnapshotRev};

use crate::kernel::graph::AdmissionBook;
use crate::members::Members;

/// One admitted member's identity: the run of its operation's admission
/// and its ordinal there, under its execution's owner. Minted at admission,
/// stored in the snapshot, never a journal position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

/// The root an execution's row holds: its revision and node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldRoot {
    pub rev: SnapshotRev,
    pub root: NodeId,
}

/// The durable snapshot store of one execution, over an actor's context:
/// its root row and graph, its admissions and its run facts, committed
/// under the actor's epoch. It holds the execution's admitted members on
/// this activation and runs their bodies from the host's [`MemberBodies`].
pub struct DurableSnapshotStore {
    pub(crate) cx: ActorContext,
    pub(crate) exec: ExecKey,
    /// The root it last read or wrote: `Some(None)` when it knows the
    /// execution has none.
    held: Mutex<Option<Option<HeldRoot>>>,
    /// The admissions the execution's latest root names, as this activation
    /// knows them.
    pub(crate) book: Mutex<AdmissionBook>,
    /// The latest root as last read, until a commit replaces it.
    pub(crate) latest: Mutex<Option<std::sync::Arc<crate::kernel::store::RootRead>>>,
    /// Nodes the execution's latest root reaches, as this activation read
    /// or committed them: stored until a commit replaces that root, so a
    /// park stages only the others. Emptied when a commit fails.
    pub(crate) stored: Mutex<std::collections::BTreeSet<NodeId>>,
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
    pub(crate) fn read_limits(&self) -> DecodeLimits {
        self.cx
            .backend()
            .config()
            .settings()
            .graph_reads
            .execution()
    }
    pub fn new(cx: &ActorContext, exec: ExecKey) -> Self {
        Self {
            members: tokio::sync::Mutex::new(Members::new(cx, exec.owner())),
            cx: cx.clone(),
            exec,
            held: Mutex::new(None),
            book: Mutex::new(AdmissionBook::default()),
            latest: Mutex::new(None),
            stored: Mutex::default(),
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

    pub(crate) fn held(&self) -> std::sync::MutexGuard<'_, Option<Option<HeldRoot>>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The root a write replaces: the one last read or written, or the
    /// stored one.
    pub(crate) async fn revision(&self) -> Result<Option<HeldRoot>, QuietPointRefusal> {
        if let Some(held) = *self.held() {
            return Ok(held);
        }
        let held = self.read().await?.map(|row| HeldRoot {
            rev: row.rev,
            root: row.root,
        });
        *self.held() = Some(held);
        Ok(held)
    }

    pub(crate) async fn read(&self) -> Result<Option<ExecRootRow>, QuietPointRefusal> {
        self.cx
            .durable_reads()
            .map_err(refused)?
            .exec_root(&self.exec)
            .await
            .map_err(refused)
    }

    /// The nodes the execution's latest root reaches, as this activation
    /// knows them. They remain protected by that root until its consumer
    /// publishes the state it carries and retires the root in one commit.
    #[must_use]
    pub fn stored_nodes(&self) -> std::collections::BTreeSet<NodeId> {
        self.stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Commit `tx` under `label`, which publishes the execution's root
    /// owing `owing`. A refused or unacknowledged commit forgets the
    /// revision, the records the members read and the nodes it knew stored:
    /// the next write reads them back.
    pub(crate) async fn commit(
        &self,
        tx: lash_durable::ActorTx,
        label: CommitLabel,
        owing: &ExecutionDebts,
        members: &mut Members,
    ) -> Result<lash_durable::ActorCommit, QuietPointRefusal> {
        members.forget();
        *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        match self.cx.commit_owing(tx, label, owing).await {
            Ok(committed) => Ok(committed),
            Err(error) => {
                *self.held() = None;
                self.stored
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
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
