//! The session actor's head commits outside `turn.commit` (ADR 0132 §3,
//! §14): a session command's (FIG-5230), a context-pressure frame's open
//! (FIG-5355) and the carry of an idle session an earlier build wrote
//! (FIG-5787).
//!
//! The caller builds the head commit. [`commit`] writes it as the session's
//! own head commit ([`DomainWrite::SessionCommit`], the shape `turn.commit`
//! writes), the heap pages of the execution root it names as its candidate
//! (FIG-6418), on the session actor's fenced transaction, under the caller's
//! label (`session.command`, `pressure.frame`): an owner whose epoch is
//! stale commits nothing, the store takes it as the session's own head
//! write whatever run owns the head, and the crash matrix cuts the commit
//! like every other owner commit.

use lash_durable::domain::{DomainRefusal, SessionCommitWrite};
use lash_durable::{CommitLabel, DomainWrite, DurableError};

use std::sync::Arc;

use crate::plugin::CodeExecutorPlugin;
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
            Self::Owner(DurableError::RevisionConflict {
                actor,
                epoch,
                expected,
                actual,
            }) => RuntimeError::new(
                RuntimeErrorCode::OwnerRevisionConflict,
                format!("owner {actor} revision moved from {expected:?} to {actual:?}"),
            )
            .with_cause(crate::RuntimeErrorCause::OwnerRevisionConflict(Box::new(
                lash_core_store::runtime_error::OwnerRevisionConflict {
                    actor: actor.as_str().to_owned(),
                    epoch: epoch.0,
                    expected: expected.0,
                    actual: actual.0,
                },
            ))),
            Self::Owner(error @ DurableError::OwnershipLost(_)) => RuntimeError::new(
                RuntimeErrorCode::SessionExecutionLeaseLost,
                error.to_string(),
            ),
            Self::Owner(error) => RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!("the session's head commit: {error}"),
            ),
        }
    }
}

/// Writes `commit` in `tx` as the session's head commit over
/// `expected_head`: its execution's descriptor staged beside it when the
/// runtime holds it, and its execution root required otherwise, so the
/// commit refuses with nothing written when the root is in neither the
/// store nor the commit. The heap pages under the root are the commit's
/// candidate ([`super::session_publication`]).
pub(crate) fn write_session_commit(
    tx: &mut lash_durable::ActorTx,
    session: &crate::SessionId,
    expected_head: u64,
    commit: crate::store::EncodedSessionCommit,
) {
    tx.stage(commit.namespace_nodes.into_values());
    match commit.descriptor {
        Some(descriptor) => {
            tx.stage([descriptor]);
        }
        None => {
            if let Some(root) = commit.execution {
                tx.require(root);
            }
        }
    }
    tx.write(DomainWrite::SessionCommit(SessionCommitWrite {
        session: session.clone(),
        expected_head,
        commit_json: commit.json,
        execution: commit.execution,
    }));
}

/// Commit `commit`, a head commit of the session actor `owner`, on its
/// fenced transaction under `label`, after admitting it against its commit
/// budget: the pages `executor`, the session's code executor, staged under
/// the execution root it names published with it.
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
    tx: lash_durable::ActorTx,
    executor: Option<&Arc<dyn CodeExecutorPlugin>>,
    commit: RuntimeCommit,
    label: CommitLabel,
    metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
) -> Result<(), HeadCommitError> {
    commit_in(owner, tx, executor, commit, label, metrics, None, None).await
}

/// [`commit`], recording in the same transaction that the session's state
/// is written in `formats` from it on (the commit that carries a session an
/// earlier build wrote to this build's formats, FIG-5787), and ending the
/// operation scope `ending` in it (ADR 0132 §11): the commit that settles a
/// plugin task's command closes its operation run's scope and marks the
/// first batch of its `Until` children (FIG-5345).
///
/// # Errors
///
/// As [`commit`]; a failed read of `ending`'s children is
/// [`HeadCommitError::Owner`].
#[expect(
    clippy::too_many_arguments,
    reason = "the cutover preserves the supplied transaction and existing commit options"
)]
pub(crate) async fn commit_in(
    owner: &ActorContext,
    tx: lash_durable::ActorTx,
    executor: Option<&Arc<dyn CodeExecutorPlugin>>,
    commit: RuntimeCommit,
    label: CommitLabel,
    metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
    formats: Option<lash_durable::FormatSet>,
    ending: Option<&lash_durable::domain::ScopeKey>,
) -> Result<(), HeadCommitError> {
    crate::store::admit_runtime_commit_budget(&commit, metrics).map_err(HeadCommitError::Store)?;
    let encoded = crate::store::encode_session_commit(&commit).map_err(HeadCommitError::Store)?;
    let mut tx = tx;
    if commit.frame_transition.is_some() || commit.applied_commands.is_some() || formats.is_some() {
        tx.write(DomainWrite::Prompt(
            lash_durable::domain::PromptWrite::ResetPrefix {
                session: commit.session_id.clone(),
            },
        ));
    }
    write_session_commit(
        &mut tx,
        &commit.session_id,
        commit.expected_head_revision,
        encoded,
    );
    if let Some(scope) = ending {
        super::turn_scope::end_operation_scope(owner, &mut tx, scope)
            .await
            .map_err(HeadCommitError::Owner)?;
    }
    if let Some(formats) = formats {
        tx.stamp_formats(formats);
    }
    let committed =
        super::session_publication::commit(owner, executor, &commit.session_id, tx, label, None)
            .await;
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

impl crate::LashRuntime {
    /// Open before loading the mutable head. A validating read cannot grant
    /// a newer revision to a draft assembled from an older one.
    pub(in crate::runtime) async fn open_head_attempt(
        &mut self,
        owner: &ActorContext,
    ) -> Result<lash_durable::ActorTx, RuntimeError> {
        let tx = owner
            .begin()
            .await
            .map_err(|error| HeadCommitError::Owner(error).into_runtime_error())?;
        self.invalidate_resident_session_state();
        self.reload_invalidated_resident_session_state().await?;
        let checked = owner
            .begin()
            .await
            .map_err(|error| HeadCommitError::Owner(error).into_runtime_error())?;
        if tx.revision() != checked.revision() {
            self.invalidate_resident_session_state();
            return Err(HeadCommitError::Owner(DurableError::RevisionConflict {
                actor: tx.actor().clone(),
                epoch: tx.epoch(),
                expected: tx.revision(),
                actual: checked.revision(),
            })
            .into_runtime_error());
        }
        Ok(tx)
    }
}
