//! The session actor's head commits outside `turn.commit` (ADR 0132 §3,
//! §14): a session command's (FIG-5230), a context-pressure frame's open
//! (FIG-5355) and the carry of an idle session an earlier build wrote
//! (FIG-5787).
//!
//! The caller builds the head commit. [`commit`] writes it as the session's
//! own head commit ([`DomainWrite::SessionCommit`], the shape `turn.commit`
//! writes) on the session actor's fenced transaction, under the caller's
//! label (`session.command`, `pressure.frame`): an owner whose epoch is
//! stale commits nothing, the store takes it as the session's own head
//! write whatever run owns the head, and the crash matrix cuts the commit
//! like every other owner commit.

use lash_durable::domain::{DomainRefusal, SessionCommitWrite};
use lash_durable::{CommitLabel, DomainWrite, DurableError};

use crate::store::RuntimeCommit;
use crate::{ActorContext, RuntimeError, RuntimeErrorCode, StoreError};

/// Why a head commit is not known to have landed.
#[derive(Debug)]
pub(crate) enum HeadCommitError {
    /// The session store refused it, with its own typed refusal; nothing was
    /// written.
    Store(StoreError),
    /// The session store refused its content for one of its own rules,
    /// under the code and cause it carries the refusal with; nothing was
    /// written.
    Refused(RuntimeError),
    /// The owner's transaction failed: ownership lost (nothing was written),
    /// the store failed or refused it for the deployment or the state it
    /// holds (FIG-5398), or the acknowledgement was lost.
    Owner(DurableError),
}

impl HeadCommitError {
    pub(crate) fn into_runtime_error(self) -> RuntimeError {
        match self {
            Self::Store(error) => crate::runtime::runtime_error_from_store_commit(error),
            Self::Refused(error) => error,
            Self::Owner(error) => RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!("the session's head commit: {error}"),
            ),
        }
    }
}

/// Commit `commit`, a head commit of the session actor `owner`, on its
/// fenced transaction under `label`, after admitting it against its commit
/// budget.
///
/// # Errors
///
/// [`HeadCommitError::Store`] with the store's refusal (a withdrawn
/// command, a moved head, a stale append's ancestor, the budget);
/// [`HeadCommitError::Refused`] when the store refused its content for
/// another of its rules;
/// [`HeadCommitError::Owner`] when the transaction failed.
pub(crate) async fn commit(
    owner: &ActorContext,
    commit: RuntimeCommit,
    label: CommitLabel,
    metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
) -> Result<(), HeadCommitError> {
    commit_in(owner, commit, label, metrics, None).await
}

/// [`commit`], recording in the same transaction that the session's state
/// is written in `formats` from it on: the commit that carries a session an
/// earlier build wrote to this build's formats (FIG-5787).
///
/// # Errors
///
/// As [`commit`].
pub(crate) async fn commit_in(
    owner: &ActorContext,
    commit: RuntimeCommit,
    label: CommitLabel,
    metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
    formats: Option<lash_durable::FormatSet>,
) -> Result<(), HeadCommitError> {
    crate::store::admit_runtime_commit_budget(&commit, metrics).map_err(HeadCommitError::Store)?;
    let write = SessionCommitWrite {
        session: commit.session_id.clone(),
        expected_head: commit.expected_head_revision,
        commit_json: crate::store::encode_session_commit(&commit)
            .map_err(HeadCommitError::Store)?,
    };
    let mut tx = owner.begin().await.map_err(HeadCommitError::Owner)?;
    tx.write(DomainWrite::SessionCommit(write));
    if let Some(formats) = formats {
        tx.stamp_formats(formats);
    }
    let committed = owner.commit(tx, label).await;
    if let Err(DurableError::Domain(refusal)) = &committed
        && let Some(error) = refusal.session_commit_refusal()
    {
        return Err(HeadCommitError::Refused(error));
    }
    match committed {
        Ok(_) => Ok(()),
        Err(DurableError::Domain(DomainRefusal::HeadMoved {
            expected, found, ..
        })) => Err(HeadCommitError::Store(StoreError::HeadRevisionConflict {
            expected,
            actual: found.unwrap_or_default(),
        })),
        Err(DurableError::Domain(DomainRefusal::SessionCommandWithdrawn { session, batch })) => {
            Err(HeadCommitError::Store(
                StoreError::SessionCommandWithdrawn {
                    session_id: session,
                    batch_id: batch,
                },
            ))
        }
        Err(DurableError::Domain(DomainRefusal::AppendAncestorNotActive { required, .. })) => Err(
            HeadCommitError::Store(StoreError::AppendAncestorNotActive {
                required_node_id: required,
            }),
        ),
        Err(error) => Err(HeadCommitError::Owner(error)),
    }
}
