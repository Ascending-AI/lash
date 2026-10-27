//! Reopen observations displaced by this claim's own committed writes.
//! Prepared transactions and immutable record readers keep their original basis.

use crate::domain::pages::{LeaseFence, PageStore, RootLease, RootLeaseRequest};
use crate::domain::{ExecKey, RootOwner};
use crate::{ActorKey, ActorKind, DurableError, Epoch, StateRevision};
use lash_core_store::checkpoint_graph::GraphRefusal;

/// Whether a retained observation was displaced by this claim's adopted revision.
/// The caller reads `acknowledged` under the same lock that adopts owner commits.
/// This authorizes a new observation, never a revision change to a prepared draft.
///
/// # Errors
/// A failure reading the acknowledged projection after a matching conflict.
pub async fn retained_displaced(
    actor: &ActorKey,
    epoch: Epoch,
    sampled: Option<StateRevision>,
    error: &DurableError,
    acknowledged: impl std::future::Future<Output = Result<Option<StateRevision>, DurableError>> + Send,
) -> Result<bool, DurableError> {
    let DurableError::RevisionConflict {
        actor: writer,
        epoch: written_epoch,
        expected,
        actual,
    } = error
    else {
        return Ok(false);
    };
    if !same_claim(actor, epoch, writer.kind(), writer.id(), *written_epoch)
        || sampled != Some(*expected)
    {
        return Ok(false);
    }
    let acknowledged = acknowledged.await?;
    let reopen = acknowledged.is_some_and(|revision| revision > *expected && revision >= *actual);
    tracing::debug!(%actor, %epoch, ?expected, ?actual, ?acknowledged, reopen,
        "retained owner read revision conflict");
    Ok(reopen)
}

/// Lease a live process's current root under its unchanged claim fence.
/// A sibling outcome can advance the root between lookup and acquisition.
/// Only a replacement written by this actor at this epoch permits reopening;
/// fixed records, readers and roots belonging to other actors remain exact.
///
/// # Errors
/// The original refusal for a foreign displacement, a retired root, lost
/// ownership, or a store failure. Each new acquisition rechecks ownership.
pub async fn acquire_lease(
    store: &dyn PageStore,
    mut request: RootLeaseRequest,
) -> Result<RootLease, DurableError> {
    loop {
        match store.acquire_root_lease(request.clone()).await {
            Err(error @ DurableError::Graph(GraphRefusal::RevisionConflict)) => {
                let (
                    RootOwner::Exec(exec @ ExecKey::Process(process)),
                    LeaseFence::Actor { actor, epoch },
                ) = (&request.owner, &request.fence)
                else {
                    return Err(error);
                };
                if actor.kind() != ActorKind::Process || actor.id() != process.as_str() {
                    return Err(error);
                }
                let row = store
                    .exec_root(exec)
                    .await?
                    .ok_or(DurableError::Graph(GraphRefusal::RootRetired))?;
                let reopen = same_claim(
                    actor,
                    *epoch,
                    ActorKind::Process,
                    process.as_str(),
                    row.written_epoch,
                ) && row.exec == *exec;
                tracing::debug!(
                    %actor, %epoch, expected = %request.expected, actual = %row.root,
                    written_epoch = %row.written_epoch, reopen,
                    "checkpoint lease revision conflict"
                );
                if !reopen {
                    return Err(error);
                }
                request.expected = row.root;
            }
            result => return result,
        }
    }
}

fn same_claim(
    actor: &ActorKey,
    epoch: Epoch,
    writer_kind: ActorKind,
    writer_id: &str,
    written_epoch: Epoch,
) -> bool {
    actor.kind() == writer_kind && actor.id() == writer_id && epoch == written_epoch
}
