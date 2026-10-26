//! A host's cancel of a plugin task the session actor runs (FIG-4391,
//! FIG-4453, FIG-5343; ADR 0132 §12).
//!
//! A task's command binds nothing: its row stays open until the commit that
//! applies it settles it, predicated on the row still being open. Before
//! the task's code runs, the session actor enters the task's operation run
//! ([`enter`]) in a fenced commit of its own, predicated on the row being
//! open. A host's cancel is one mail transaction against the row
//! (`MailDomainWrite::RequestOperationCancel`): before the entry it
//! withdraws the command, which never runs; after it, it records the
//! cancel request and control-wakes the session. While the task's code
//! runs, the session actor watches the row at every wake and poll
//! ([`watch_cancel`]), and fires the task's cancellation token once the
//! request is recorded: the task then settles `Cancelled`. A row the
//! session's close took stops the task too, and nothing of it settles.

use crate::ActorContext;
use lash_durable::domain::{DomainRefusal, SessionMailWrite};
use lash_durable::{CommitLabel, DomainWrite, DurableError};
use tokio_util::sync::CancellationToken;

/// Enter the operation run of the task command `batch` names before its
/// code runs: `false` when the command is no longer open (a host withdrew
/// it, or it settled). Entering an entered run keeps its first entry, so a
/// redrive enters it again.
///
/// # Errors
///
/// The owner's transaction failed.
pub(super) async fn enter(
    controller: &ActorContext,
    session: &lash_kernel_value::identity::SessionId,
    batch: &lash_vocabulary::identity::BatchId,
) -> Result<bool, crate::RuntimeError> {
    let entered = async {
        let mut tx = controller.begin().await?;
        tx.write(DomainWrite::SessionMail(SessionMailWrite::EnterOperation {
            session: session.clone(),
            batch: batch.clone(),
        }));
        controller.commit(tx, CommitLabel::TURN_ADMIT).await
    }
    .await;
    match entered {
        Ok(_) => Ok(true),
        Err(DurableError::Domain(DomainRefusal::SessionCommandWithdrawn { .. })) => Ok(false),
        Err(error) => Err(crate::RuntimeError::new(
            crate::RuntimeErrorCode::StoreCommitFailed,
            format!("the entry of plugin task {batch}: {error}"),
        )),
    }
}

/// Fire `stop` once a host requested the cancel of the run of the task
/// command `batch_id` names, or the open command is gone (the session's
/// close settled it). It reads before it first waits, so a request a crash
/// left is honoured at once; a command already settled when it first reads
/// is a settled task's replay, which runs to its own end. It ends then, or
/// when a read fails; the caller drops it when the task's code returns
/// first.
pub(super) async fn watch_cancel(
    controller: &ActorContext,
    session: &lash_kernel_value::identity::SessionId,
    batch_id: &lash_vocabulary::identity::BatchId,
    stop: &CancellationToken,
) {
    let mut open = false;
    loop {
        match controller
            .backend()
            .durable()
            .session_mailbox(session)
            .await
        {
            Ok(mailbox) => match mailbox
                .batches
                .iter()
                .find(|batch| batch.batch == *batch_id)
            {
                Some(batch) if batch.cancel_requested => {
                    stop.cancel();
                    return;
                }
                Some(_) => open = true,
                None if open => {
                    stop.cancel();
                    return;
                }
                None => return,
            },
            Err(error) => {
                tracing::warn!(
                    %error,
                    batch = %batch_id,
                    "a plugin task's cancel watch failed; the task runs to its own end"
                );
                return;
            }
        }
        controller.wait_for_mail().await;
    }
}
