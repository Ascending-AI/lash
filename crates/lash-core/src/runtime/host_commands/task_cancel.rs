//! A host's cancel of a plugin task the session actor runs (FIG-4391,
//! FIG-4453; ADR 0132 §12).
//!
//! A task's command binds nothing: its row stays open until the commit that
//! applies it settles it, predicated on the row still being open. A host's
//! cancel is the command's withdrawal, which reaches it at any point before
//! that commit. While the task's code runs, the session actor watches the
//! row at every wake and poll, and fires the task's cancellation token once
//! the row is withdrawn; the task's settling commit then finds the row gone
//! and applies nothing of it.

use super::*;
use crate::ActorContext;
use tokio_util::sync::CancellationToken;

/// Fire `stop` once a host withdrew the command `batch_id` names. It ends
/// then, or when a read fails; the caller drops it when the task's code
/// returns first.
pub(super) async fn watch_withdrawal(
    controller: &ActorContext,
    store: &crate::store::SessionStore,
    batch_id: &crate::BatchId,
    stop: &CancellationToken,
) {
    loop {
        controller.wait_for_mail().await;
        match store.list_queued_work().await {
            Ok(open) if open.iter().any(|batch| &batch.batch_id == batch_id) => {}
            Ok(_) => {
                stop.cancel();
                return;
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    batch = %batch_id,
                    "a plugin task's withdrawal watch failed; the task runs to its own end"
                );
                return;
            }
        }
    }
}
