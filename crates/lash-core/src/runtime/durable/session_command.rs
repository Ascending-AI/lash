//! A session command's commit (ADR 0132 §3, §14; FIG-5230).
//!
//! The command lane's handler builds the head commit that applies a command
//! and settles its rows. [`commit`] writes it as the session's own head
//! commit ([`DomainWrite::SessionCommit`], the shape `turn.commit` writes) on
//! the session actor's fenced transaction, under `session.command`: an owner
//! whose epoch is stale commits nothing, and the crash matrix cuts the
//! commit like every other owner commit.

use lash_durable::domain::{DomainRefusal, SessionCommitWrite};
use lash_durable::{CommitLabel, DomainWrite, DurableError};

use crate::store::RuntimeCommit;
use crate::{ActorContext, RuntimeError, RuntimeErrorCode, StoreError};

/// Why a session command's commit is not known to have landed.
#[derive(Debug)]
pub(crate) enum CommandCommitError {
    /// The session store refused it, with its own typed refusal; nothing was
    /// written.
    Store(StoreError),
    /// The owner's transaction failed: ownership lost (nothing was written),
    /// the store failed, or the acknowledgement was lost.
    Owner(DurableError),
}

impl CommandCommitError {
    pub(crate) fn into_runtime_error(self) -> RuntimeError {
        match self {
            Self::Store(error) => crate::runtime::runtime_error_from_store_commit(error),
            Self::Owner(error) => RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!("the session command's commit: {error}"),
            ),
        }
    }
}

/// Commit `commit`, a session command's head commit, on `owner`'s fenced
/// transaction under `session.command`, after admitting it against its
/// commit budget.
///
/// # Errors
///
/// [`CommandCommitError::Store`] with the store's refusal (a withdrawn
/// command, a moved head, a stale append's ancestor, the budget);
/// [`CommandCommitError::Owner`] when the transaction failed.
pub(crate) async fn commit(
    owner: &ActorContext,
    commit: RuntimeCommit,
    metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
) -> Result<(), CommandCommitError> {
    crate::store::admit_runtime_commit_budget(&commit, metrics)
        .map_err(CommandCommitError::Store)?;
    let write = SessionCommitWrite {
        session: commit.session_id.clone(),
        expected_head: commit.expected_head_revision,
        commit_json: crate::store::encode_session_commit(&commit)
            .map_err(CommandCommitError::Store)?,
    };
    let mut tx = owner.begin().await.map_err(CommandCommitError::Owner)?;
    tx.write(DomainWrite::SessionCommit(write));
    match owner.commit(tx, CommitLabel::SESSION_COMMAND).await {
        Ok(_) => Ok(()),
        Err(DurableError::Domain(DomainRefusal::HeadMoved {
            expected, found, ..
        })) => Err(CommandCommitError::Store(
            StoreError::HeadRevisionConflict {
                expected,
                actual: found.unwrap_or_default(),
            },
        )),
        Err(DurableError::Domain(DomainRefusal::SessionCommandWithdrawn { session, batch })) => {
            Err(CommandCommitError::Store(
                StoreError::SessionCommandWithdrawn {
                    session_id: session,
                    batch_id: batch,
                },
            ))
        }
        Err(DurableError::Domain(DomainRefusal::AppendAncestorNotActive { required, .. })) => Err(
            CommandCommitError::Store(StoreError::AppendAncestorNotActive {
                required_node_id: required,
            }),
        ),
        Err(error) => Err(CommandCommitError::Owner(error)),
    }
}
