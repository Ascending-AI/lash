//! How a commit that may publish the session's execution root is made
//! (FIG-6418): the heap pages the session's code executor staged under the
//! root travel as the commit's candidate, owned by the one write that names
//! the root (a turn's working state, or a head commit's checkpoint), with
//! the publication identity check, lock order and spool settlement a cell's
//! candidate gets (S6). Every such commit goes through [`commit`]:
//!
//! - `phases`: `model.start`, `model.done` (a held response committed on
//!   its own, and a refused round), and `turn.commit`;
//! - `tool_round`: the round admission's `model.done`;
//! - `turn_cancel`: `turn.cancel`;
//! - `head_commit`: a session command's, a context-pressure frame's and a
//!   session carry's head commit.

use std::sync::Arc;

use lash_core_store::checkpoint_graph::{CommitId, NodeId};
use lash_durable::domain::pages::{CandidateOwner, CandidatePublication};
use lash_durable::domain::{SessionCommitWrite, TurnWrite};
use lash_durable::{ActorCommit, ActorTx, CommitLabel, DomainWrite, DurableError, Release};

use crate::ActorContext;
use crate::plugin::CodeExecutorPlugin;

/// The write of `tx` that publishes `session`'s execution root.
#[derive(Clone, Copy)]
enum Publishes {
    /// The turn's working state, at the root.
    Working(NodeId),
    /// A head commit over `expected_head`, whose checkpoint names the root.
    Head { root: NodeId, expected_head: u64 },
}

impl Publishes {
    fn root(self) -> NodeId {
        match self {
            Self::Working(root) | Self::Head { root, .. } => root,
        }
    }

    /// Whether the write landed: the turn's working row names the root,
    /// or the head moved past the revision the commit expected. The
    /// session actor's fence makes either this commit's.
    async fn landed(
        self,
        cx: &ActorContext,
        session: &crate::SessionId,
    ) -> Result<bool, DurableError> {
        match self {
            Self::Working(root) => Ok(cx
                .durable_reads()?
                .turn(session)
                .await?
                .is_some_and(|row| row.working == Some(root))),
            Self::Head { expected_head, .. } => {
                let factory = cx.backend().session_store_factory();
                let head =
                    crate::SessionCommitStore::load_session_head_meta(factory.as_ref(), session)
                        .await
                        .map_err(|error| {
                            DurableError::Store(lash_durable::StoreFailure {
                                kind: lash_durable::StoreFailureKind::Unavailable,
                                message: format!("the session's head: {error}"),
                            })
                        })?;
                Ok(head.is_some_and(|head| head.head_revision > expected_head))
            }
        }
    }
}

/// The write of `tx` that publishes `session`'s execution root, if any.
fn published(tx: &ActorTx, session: &crate::SessionId) -> Option<Publishes> {
    tx.domain().iter().find_map(|write| match write {
        DomainWrite::Turn(TurnWrite::Working {
            session: wrote,
            working: Some(root),
            ..
        }) if wrote == session => Some(Publishes::Working(*root)),
        DomainWrite::SessionCommit(SessionCommitWrite {
            session: wrote,
            execution: Some(root),
            expected_head,
            ..
        }) if wrote == session => Some(Publishes::Head {
            root: *root,
            expected_head: *expected_head,
        }),
        _ => None,
    })
}

/// Commits `tx` under `label` on `cx`, the session actor's context, with
/// the claimed actor given up as `release` when one is: with the candidate
/// `executor` builds for the execution root `tx` publishes for `session`,
/// owned by the session, when the executor stages that root's pages; as a
/// plain commit otherwise.
///
/// An acknowledged publication is acknowledged to the executor. One whose
/// acknowledgement was lost is resolved against the row its owner write
/// names, as a cell's is against its execution row, and the error is
/// returned unchanged: the pass ends, and the next resumes from the rows,
/// as a cell's activation does.
/// Any other refusal withdraws the capture's candidate, its pages kept
/// staged for the next capture.
///
/// # Errors
///
/// The store's refusal, or the executor's failure to build the candidate;
/// nothing was written unless the error is [`DurableError::AckLost`].
pub(crate) async fn commit(
    cx: &ActorContext,
    executor: Option<&Arc<dyn CodeExecutorPlugin>>,
    session: &crate::SessionId,
    mut tx: ActorTx,
    label: CommitLabel,
    release: Option<Release>,
) -> Result<ActorCommit, DurableError> {
    let staged = match (executor, published(&tx, session)) {
        (Some(executor), Some(publishes)) => executor
            .execution_candidate(publishes.root(), CommitId::mint(), &mut tx)
            .await
            .map_err(|error| {
                DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: format!("the session's execution candidate: {error}"),
                })
            })?
            .map(|candidate| (executor, publishes, candidate)),
        _ => None,
    };
    let Some((executor, publishes, candidate)) = staged else {
        return match release {
            Some(release) => cx.commit_draining(tx, label, release).await,
            None => cx.commit(tx, label).await,
        };
    };
    let publication = CandidatePublication {
        owner: CandidateOwner::Session(session.clone()),
        manifest: candidate.manifest,
    };
    let committed = match release {
        Some(release) => {
            cx.commit_candidate_draining(tx, publication, label, release)
                .await
        }
        None => {
            cx.commit_candidate_owing(tx, publication, label, None)
                .await
        }
    };
    match &committed {
        Ok(_) => executor.acknowledge_execution_state_capture().await,
        Err(DurableError::AckLost { .. }) => {
            let landed = publishes.landed(cx, session).await?;
            executor.resolve_execution_state_capture(landed).await;
        }
        Err(_) => executor.resolve_execution_state_capture(false).await,
    }
    committed
}
