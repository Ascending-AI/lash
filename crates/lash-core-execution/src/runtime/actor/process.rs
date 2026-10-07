//! The process's half of the context (ADR 0132 §10, §11). Owned by L6
//! (FIG-5175).
//!
//! The process activation, `advance` driving and the terminal transaction
//! live here; the engine trait is `runtime::process::engine::ProcessEngine`.

mod activation;
mod driver;
mod session_turn;
mod terminal;

pub use activation::ProcessActivation;
pub use session_turn::{SessionTurnCancel, SessionTurnMail, SessionTurns};
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

/// The registry's scope for `scope`: what its `Until` children name and
/// what its closure fact is keyed by. A code cell owns no `Until`
/// processes.
#[must_use]
pub fn scope_id(scope: &ScopeKey) -> Option<crate::ScopeId> {
    Some(match scope {
        ScopeKey::Turn(session_id, turn_id) => crate::ScopeId::Opener(crate::EffectOpener::Turn {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        }),
        ScopeKey::Session(session_id) => crate::ScopeId::Session(session_id.clone()),
        ScopeKey::Process(process_id) => crate::ScopeId::Opener(crate::EffectOpener::Process {
            process_id: process_id.clone(),
        }),
        ScopeKey::Cell(..) => return None,
    })
}

/// The registry's index columns for `scope`, `(lifetime_scope_kind,
/// lifetime_scope_id)`: the projection its `Until` children are found by.
#[must_use]
pub fn scope_index(scope: &ScopeKey) -> Option<(&'static str, String)> {
    scope_id(scope).map(|scope| (scope.storage_kind(), scope.storage_id()))
}

/// The roots of `scope`'s `Until` subtree, as the live-subtree read binds
/// them: the scope's own index columns and, for a session, the index-id
/// prefixes of the turn and session-operation scopes inside it, whose
/// processes live in the session's tree too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeRoots {
    /// The scope's `lifetime_scope_kind`.
    pub kind: &'static str,
    /// The scope's `lifetime_scope_id`.
    pub id: String,
    /// For a session, the prefix of its turn scopes' ids.
    pub turns: Option<String>,
    /// For a session, the prefix of its session-operation scopes' ids.
    pub operations: Option<String>,
}

/// The roots of `scope`'s `Until` subtree; `None` for a code cell.
#[must_use]
pub fn subtree_roots(scope: &ScopeKey) -> Option<SubtreeRoots> {
    let (kind, id) = scope_index(scope)?;
    let (turns, operations) = match scope {
        // Each range's lower bound is the prefix every encoding in it
        // starts with.
        ScopeKey::Session(session_id) => (
            Some(crate::EffectOpener::session_turn_encoding_range(session_id).0),
            Some(crate::EffectOpener::session_operation_encoding_range(session_id).0),
        ),
        ScopeKey::Turn(..) | ScopeKey::Process(_) | ScopeKey::Cell(..) => (None, None),
    };
    Some(SubtreeRoots {
        kind,
        id,
        turns,
        operations,
    })
}

/// Close `scope` on `tx`, the transaction that ended it: its closure fact
/// refuses every later registration under it (ADR 0132 §11). Write it after
/// the scope's first cascade batch, so the transaction records a turn
/// scope as ending exactly when a child is left unmarked.
pub fn close_scope(tx: &mut ActorTx, scope: &ScopeKey) {
    tx.write(DomainWrite::Process(ProcessWrite::ScopeClosed {
        scope: scope.clone(),
    }));
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
        match envelope.command {
            // The registry operations commit their actor rows in their own
            // transactions: a start creates the actor ready, a cancel
            // records the first request with its mail, an await reads the
            // process-terminal wait. A process command runs on its own
            // executor, never through `execute`, which refuses it.
            crate::RuntimeEffectCommand::Process { command } => {
                let result = if matches!(
                    command.as_ref(),
                    crate::ProcessCommand::PublishDefinition { .. }
                        | crate::ProcessCommand::GetDefinition { .. }
                ) {
                    local.into_definition_execution()?.execute(*command).await?
                } else {
                    let receiver = envelope.invocation.execution_scope().clone();
                    // Boxed: the start and await state machines are large.
                    Box::pin(local.into_process()?.execute(&receiver, *command)).await?
                };
                Ok(crate::RuntimeEffectOutcome::Process { result })
            }
            command @ crate::RuntimeEffectCommand::LoadExecutionEnv { .. } => {
                local
                    .execute(crate::RuntimeEffectEnvelope {
                        invocation: envelope.invocation,
                        command,
                    })
                    .await
            }
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
