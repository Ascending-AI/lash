//! The store-maintenance outcome contract (ADR 0067 §4 and §5).
//!
//! Every reclamation lever — [`StoreMaintenance::vacuum`],
//! [`StoreMaintenance::gc_unreachable`], and the attachment sweep
//! [`reclaim_unreferenced_attachments`](crate::attachments::reclaim_unreferenced_attachments),
//! plus trigger occurrence reclamation
//! [`TriggerStore::reclaim_trigger_occurrences`](crate::TriggerStore::reclaim_trigger_occurrences)
//! — answers in one vocabulary with five arms, and every backend reaches all of
//! the arms it can reach:
//!
//! * **Swept** — `Ok(report)` that reclaimed at least one item and recorded no
//!   per-item failures.
//! * **Incomplete** — `Ok(report)` that completed its scope but recorded failed
//!   destructive steps in report-specific failure channels.
//! * **Nothing to do** — `Ok(report)` that reclaimed nothing and recorded no
//!   failures or deferrals. Emptiness here is *witnessed*: the pass enumerated
//!   its whole scope and there was nothing to reclaim.
//! * **Refused** — `Err(`[`MaintenanceFailure`]`)` carrying
//!   [`MaintenanceStop::Refused`]. A destructive step's precondition could not
//!   be proven, so the pass declined to take it. Nothing was destroyed after
//!   the refusal; a later pass can retry.
//! * **Failed** — `Err(`[`MaintenanceFailure`]`)` carrying
//!   [`MaintenanceStop::Failed`]. The backend errored.
//!
//! Both stop arms carry the report accumulated **before** the stop, so work
//! already done is reported instead of being discarded. That is the half of the
//! contract that makes a failure impossible to launder: a backend that catches
//! its own error must hand back the partial report with the error, and can no
//! longer return a clean zero report that reads as a healthy no-op.
//!
//! [`StoreMaintenance::vacuum`]: crate::store::StoreMaintenance::vacuum
//! [`StoreMaintenance::gc_unreachable`]: crate::store::StoreMaintenance::gc_unreachable

use crate::store::StoreError;

/// Counters from one [`StoreMaintenance::gc_unreachable`](crate::store::StoreMaintenance::gc_unreachable)
/// pass over a store's content-addressed blobs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub root_count: usize,
    pub retained_blob_count: usize,
    pub deleted_blob_count: usize,
}

/// Counters from one [`StoreMaintenance::vacuum`](crate::store::StoreMaintenance::vacuum) pass.
///
/// `removed_node_count` counts the tombstoned graph-node rows that were
/// physically deleted from the store. `removed_pending_turn_input_tombstone_count`
/// counts terminal pending-input evidence rows pruned by host-scheduled
/// retention. Returned so hosts can emit metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VacuumReport {
    pub removed_node_count: usize,
    pub removed_pending_turn_input_tombstone_count: usize,
}

/// Blob outcomes from one session-owner delete cascade.
///
/// `enumerated_blob_count` is the witnessed candidate set reached from the
/// session edges being severed. `retained_blob_count` counts candidates that
/// still have an exact edge from a surviving head, anchor, artifact ref, or
/// checkpoint manifest. The remainder was deleted in the owning transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionBlobReclaimReport {
    pub enumerated_blob_count: usize,
    pub retained_blob_count: usize,
    pub deleted_blob_count: usize,
}

/// Which arm of the success side a completed maintenance pass landed on.
///
/// [`MaintenanceSweep::Swept`] means the pass reclaimed at least one item with
/// zero per-item failures. [`MaintenanceSweep::Incomplete`] means the pass
/// completed its scope but some destructive steps failed; those failures remain
/// visible in report-specific channels such as failed ids or deferred
/// condemnations. [`MaintenanceSweep::NothingToDo`] is witnessed emptiness:
/// zero reclaimed, zero failed, and zero deferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceSweep {
    /// The pass enumerated its scope and reclaimed at least one row or blob,
    /// with no per-item failures.
    Swept,
    /// The pass completed its scope, but one or more destructive steps failed
    /// or were deferred. The report carries their identities or counts.
    Incomplete,
    /// The pass enumerated its *whole* scope and found nothing reclaimable.
    ///
    /// This is the witnessed-emptiness arm (ADR 0067 §5), reachable only from
    /// `Ok`: a pass that could not finish enumerating reports a
    /// [`MaintenanceStop`] instead, so "there was nothing" is never spelled the
    /// same way as "I got nothing back".
    NothingToDo,
}

/// The aggregate half of the outcome contract: counters a completed pass
/// produced, and the success arm they name.
///
/// Implemented by every maintenance report so callers and the conformance laws
/// can ask one question — did this pass reclaim anything? — without knowing
/// which lever produced the report.
pub trait MaintenanceReport {
    /// Rows, blobs, or digests this pass physically reclaimed.
    fn reclaimed_count(&self) -> usize;

    /// The success arm these counters name.
    ///
    /// This default is valid only for reports with no failure channels: a
    /// positive reclaimed count is [`MaintenanceSweep::Swept`] and zero is
    /// [`MaintenanceSweep::NothingToDo`]. Reports that can record per-item
    /// failures or deferrals must override this method so those channels
    /// classify the pass as [`MaintenanceSweep::Incomplete`].
    fn sweep(&self) -> MaintenanceSweep {
        if self.reclaimed_count() == 0 {
            MaintenanceSweep::NothingToDo
        } else {
            MaintenanceSweep::Swept
        }
    }
}

impl MaintenanceReport for GcReport {
    fn reclaimed_count(&self) -> usize {
        self.deleted_blob_count
    }
}

impl MaintenanceReport for VacuumReport {
    fn reclaimed_count(&self) -> usize {
        self.removed_node_count + self.removed_pending_turn_input_tombstone_count
    }
}

impl MaintenanceReport for SessionBlobReclaimReport {
    fn reclaimed_count(&self) -> usize {
        self.deleted_blob_count
    }
}

/// A destructive precondition a maintenance pass could not prove.
///
/// A refusal is not an error in the backend: the store is healthy, the scope is
/// intact, and nothing was destroyed. It is the pass declining to take a
/// destructive step on evidence it does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaintenanceRefusal {
    /// The live root set enumerated empty and the host did not authorize an
    /// empty root set to mean "delete every unreferenced blob"
    /// ([`EmptyRootSetPolicy::Refuse`](crate::attachments::EmptyRootSetPolicy::Refuse)).
    EmptyRootSetUnauthorized,
    /// The scope could not be enumerated completely, so its emptiness is
    /// unwitnessed and no deletion may be authorized by it (ADR 0067 §5).
    /// `scope` names the source that could not be proven.
    UnwitnessedScope { scope: &'static str },
}

impl std::fmt::Display for MaintenanceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRootSetUnauthorized => f.write_str(
                "the live root set is empty and the host did not authorize deleting every \
                 unreferenced blob",
            ),
            Self::UnwitnessedScope { scope } => {
                write!(f, "`{scope}` could not be enumerated completely")
            }
        }
    }
}

/// Why a maintenance pass stopped short of completing its scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaintenanceStop<E = StoreError> {
    /// The pass declined a destructive step (see [`MaintenanceRefusal`]).
    Refused(MaintenanceRefusal),
    /// The backend failed. Retrying the pass is always safe.
    Failed(E),
}

impl<E: std::fmt::Display> std::fmt::Display for MaintenanceStop<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "refused: {refusal}"),
            Self::Failed(error) => write!(f, "failed: {error}"),
        }
    }
}

/// A maintenance pass that did not run to completion, plus the work it had
/// already done when it stopped.
///
/// The partial report is the point: a caller is told how far the pass got
/// instead of having to guess, and a backend cannot present its own caught
/// error as a clean empty sweep (ADR 0067 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceFailure<R, E = StoreError> {
    /// Why the pass stopped.
    pub stop: MaintenanceStop<E>,
    /// Work completed before the stop. Zeroed counters here mean the pass
    /// stopped before reclaiming anything, not that there was nothing to do.
    pub partial: R,
}

impl<R, E> MaintenanceFailure<R, E> {
    /// A backend failure, carrying the report accumulated before it.
    pub fn failed(error: E, partial: R) -> Self {
        Self {
            stop: MaintenanceStop::Failed(error),
            partial,
        }
    }

    /// A refused destructive step, carrying the report accumulated before it.
    pub fn refused(refusal: MaintenanceRefusal, partial: R) -> Self {
        Self {
            stop: MaintenanceStop::Refused(refusal),
            partial,
        }
    }

    /// The refusal, when this pass refused rather than failed.
    pub fn refusal(&self) -> Option<&MaintenanceRefusal> {
        match &self.stop {
            MaintenanceStop::Refused(refusal) => Some(refusal),
            MaintenanceStop::Failed(_) => None,
        }
    }
}

impl<R, E> MaintenanceFailure<R, E>
where
    R: Default,
{
    /// A backend failure that stopped the pass before it reclaimed anything.
    pub fn failed_before_any_work(error: E) -> Self {
        Self::failed(error, R::default())
    }
}

impl<R, E: std::fmt::Display> std::fmt::Display for MaintenanceFailure<R, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store maintenance {}", self.stop)
    }
}

impl<R, E> std::error::Error for MaintenanceFailure<R, E>
where
    R: std::fmt::Debug,
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.stop {
            MaintenanceStop::Failed(error) => Some(error),
            MaintenanceStop::Refused(_) => None,
        }
    }
}

/// The result of one store-maintenance lever.
pub type MaintenanceResult<R> = Result<R, MaintenanceFailure<R>>;
