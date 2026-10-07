//! The process's half of the context (ADR 0132 §10, §11). Owned by L6
//! (FIG-5175).
//!
//! The process activation, `advance` driving and the terminal transaction
//! live here; the engine trait is `runtime::process::engine::ProcessEngine`.

mod activation;
mod driver;
mod terminal;

pub use activation::ProcessActivation;
pub use terminal::{ProcessParkReason, cancelled, record_park, record_terminal};

use lash_durable::domain::{ProcessWrite, ScopeKey};
use lash_durable::{ActorTx, DomainWrite, DurableError, DurableReads};

pub use lash_durable::domain::{CancelAnswer, CancelRequest, ProcessActorRow};

/// How far one [`end_scope`] batch got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CascadeProgress {
    /// Every `Until` child of the scope is marked for cancel.
    Done,
    /// A batch was marked; the rest continue from the durable cursor.
    More {
        /// How many children this batch marked.
        marked: usize,
    },
}

/// The registry's index columns for `scope`, `(lifetime_scope_kind,
/// lifetime_scope_id)`: the projection its `Until` children are found by.
/// A code cell owns no `Until` processes.
#[must_use]
pub fn scope_index(scope: &ScopeKey) -> Option<(&'static str, String)> {
    let scope = match scope {
        ScopeKey::Turn(session_id, turn_id) => crate::ScopeId::Opener(crate::EffectOpener::Turn {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        }),
        ScopeKey::Session(session_id) => crate::ScopeId::Session(session_id.clone()),
        ScopeKey::Process(process_id) => crate::ScopeId::Opener(crate::EffectOpener::Process {
            process_id: process_id.clone(),
        }),
        ScopeKey::Cell(..) => return None,
    };
    Some((scope.storage_kind(), scope.storage_id()))
}

/// Mark the next `batch` of `scope`'s live `Until` children for cancel on
/// `tx` with `origin`, after the cursor `after`: each records its first
/// cancel request, gets its cancel mail and a control wake when `tx`
/// commits. The children are read through `reads` first; a child marked
/// by an earlier batch, or cancelled by anyone else, is not read again, so
/// a crash between batches loses and repeats nothing. When `scope` is the
/// committing process its cursor moves with the batch. A turn's commit, a
/// session close and a process terminal call it.
///
/// # Errors
///
/// The read's failure.
pub async fn end_scope(
    reads: &dyn DurableReads,
    tx: &mut ActorTx,
    scope: &ScopeKey,
    after: Option<&crate::ProcessId>,
    batch: usize,
    origin: crate::CancelOrigin,
    requester: &str,
) -> Result<CascadeProgress, DurableError> {
    let batch = batch.max(1);
    let mut children = reads.until_children(scope, after, batch + 1).await?;
    let more = children.len() > batch;
    children.truncate(batch);
    let marked = children.len();
    let cursor = more
        .then(|| children.last().map(|child| child.as_str().to_owned()))
        .flatten();
    tx.write(DomainWrite::Process(ProcessWrite::CascadeBatch {
        scope: scope.clone(),
        children,
        origin,
        requester: requester.to_owned(),
        cursor,
    }));
    Ok(if more {
        CascadeProgress::More { marked }
    } else {
        CascadeProgress::Done
    })
}

use super::ActorContext;

/// The process methods of the context.
impl ActorContext {
    /// The process effects: `Process` (start, list, transfer, await,
    /// attach, cancel, signal, emit) and `LoadExecutionEnv`. A start is a
    /// store-local effect of its call's outcome; an await is a
    /// `process_terminal` wait; a cancel is mail. Any other command is
    /// refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn process_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        match &envelope.command {
            // The registry operations commit their actor rows in their own
            // transactions: a start creates the actor ready, a cancel
            // records the first request with its mail, an await reads the
            // process-terminal wait.
            crate::RuntimeEffectCommand::Process { .. }
            | crate::RuntimeEffectCommand::LoadExecutionEnv { .. } => local.execute(envelope).await,
            other => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EngineControlUnsupported,
                format!("`{:?}` is not a process effect", other.kind()),
            )),
        }
    }

    /// Whether the process this context runs has a committed cancellation.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn observe_process_cancel(
        &self,
        lent_stop: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::RuntimeEffectControllerError> {
        if lent_stop.is_cancelled() {
            return Ok(true);
        }
        let Some(process) = self.admitted_process() else {
            return Ok(false);
        };
        let record = self
            .backend()
            .process_registry()
            .get_process(process)
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ProcessRegistryUnavailable,
                    error.to_string(),
                )
            })?;
        Ok(record.is_some_and(|record| record.cancel_request.is_some()))
    }

    /// Run one registry step of a process drive.
    ///
    /// # Errors
    ///
    /// The step's refusal.
    pub async fn record_process_drive_step(
        &self,
        _name: String,
        step: crate::ProcessDriveStep<'_>,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        // Nothing is replayed (ADR 0132): the step runs, and its registry
        // write is its own record.
        step.await
            .map_err(crate::RuntimeEffectControllerError::from)
    }
}
