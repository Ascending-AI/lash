//! A host's cancel of a plugin task a drive already admitted (FIG-4391,
//! FIG-4453).
//!
//! Withdrawal reaches only a command no drive has read (FIG-4202). Once a
//! drive admitted a plugin task, a host's cancel resolves the task's cancel
//! signal: a keyed promise under the command's own queue-drain scope, which
//! only a host's cancel ever resolves. The signal is a durable request, not a
//! decision. The one record of how the command ended is its settlement,
//! written by the commit that settles it, which every replay and every
//! submitter reads back (ADR 0105 §1).
//!
//! The drive peeks the signal before the task runs, and a task whose cancel
//! was already requested runs none of its code. While the task runs, the
//! drive watches the signal and fires the task's cancellation token when a
//! cancel lands. Once the task's code returned, the drive peeks the signal
//! again: a requested cancel settles the command
//! [`Cancelled`](crate::PluginOperationCommandOutcome::Cancelled) with nothing
//! of the task committed, and otherwise the command settles with the task's
//! own outcome. That decision becomes durable only with the settling commit
//! (FIG-4453): a drive that dies before its commit leaves nothing decided,
//! and its redrive runs the task's code again under the same live signal, so
//! a host's cancel still reaches the task.
//!
//! A cancel that lands after the drive's last peek and before its commit is
//! kept on the signal but reaches nothing: the command settles with the
//! task's own outcome, and its settlement says so. The token only stops the
//! task's code. A watch that fails is not a cancel: the task runs to its own
//! end, and the drive's peek after it still finds a requested cancel.

use super::*;
use tokio_util::sync::CancellationToken;

/// What a host's cancel of an admitted plugin task did (FIG-4391,
/// FIG-4453). The command's settlement is the answer to how the task
/// ended; this says only where the cancel landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginTaskCancelRequest {
    /// The command was unsettled, and the task's cancel signal holds the
    /// cancel, from this request or an earlier one. Its drive stops the
    /// task's code and settles the command `Cancelled`, with nothing of the
    /// task committed, unless the drive already found the task's code
    /// returned with no cancel requested and settles it with its own
    /// outcome.
    Requested,
    /// The command had already settled: its settlement, read back by its
    /// receipt, says how it ended.
    AlreadySettled,
    /// No signal carries the cancel: the effect host keeps no durable
    /// await-event keys, or the session's waits are revoked. The command
    /// settles as its drive applies it.
    Unavailable,
}

/// Whether `error` says the effect host holds no signal for the task: it
/// mints no durable keys, or the session's waits are revoked.
fn signal_unavailable(error: &RuntimeError) -> bool {
    matches!(
        error.code,
        RuntimeErrorCode::AwaitEventUnsupported | RuntimeErrorCode::AwaitEventUnknownOrRevoked
    )
}

/// The refusal for a cancel signal holding a resolution no host's cancel
/// writes.
fn foreign_resolution(key: &crate::AwaitEventKey, resolution: &crate::Resolution) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::SessionCommandRun,
        format!(
            "the plugin task cancel signal `{}` holds {resolution:?}, which no host's cancel writes",
            key.key_id
        ),
    )
}

/// One admitted plugin task's cancel signal, over the deployment's effect
/// host.
pub(super) struct PluginTaskCancelSignal {
    host: Arc<dyn crate::EffectHost>,
    key: crate::AwaitEventKey,
}

impl PluginTaskCancelSignal {
    /// The cancel signal of the plugin task command `batch_id` names, over
    /// `host`. `None` when `host` holds no signal for it.
    pub(super) async fn open(
        host: Arc<dyn crate::EffectHost>,
        session_id: &crate::SessionId,
        batch_id: &str,
    ) -> Result<Option<Self>, RuntimeError> {
        let scope = crate::ExecutionScope::queue_drain(session_id.clone(), batch_id);
        let minted = host
            .await_event_resolver()
            .await_event_key(
                &scope,
                crate::AwaitEventWaitIdentity::SessionCommandCancelSignal,
            )
            .await;
        match minted {
            Ok(key) => Ok(Some(Self { host, key })),
            Err(error) if signal_unavailable(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether a host's cancel of the task was requested. The drive asks
    /// before the task runs, so a task cancelled before it ran runs none of
    /// its code, and again once the task's code returned, to decide how the
    /// command settles.
    pub(super) async fn cancel_requested(&self) -> Result<bool, RuntimeError> {
        match self
            .host
            .await_event_resolver()
            .peek_await_event(&self.key)
            .await
        {
            Ok(None) => Ok(false),
            Ok(Some(crate::Resolution::Cancelled)) => Ok(true),
            Ok(Some(resolution)) => Err(foreign_resolution(&self.key, &resolution)),
            Err(error) if signal_unavailable(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Execution-side only: fire `stop` when a host's cancel resolves the
    /// signal. It ends once the signal resolved or the watch failed; the
    /// drive drops it when the task's code returns first.
    pub(super) async fn watch(&self, stop: &CancellationToken) {
        // Never a fired token: firing the waiter's token would resolve the
        // signal itself cancelled.
        let watched = self
            .host
            .await_event_resolver()
            .await_await_event(&self.key, CancellationToken::new(), None)
            .await;
        match watched {
            Ok(crate::Resolution::Cancelled) => stop.cancel(),
            Ok(resolution) => tracing::warn!(
                key = %self.key.key_id,
                ?resolution,
                "a plugin task's cancel signal holds a resolution no host's cancel writes; \
                 the task runs to its own end"
            ),
            Err(error) => tracing::warn!(
                %error,
                key = %self.key.key_id,
                "a plugin task's cancel watch failed; the task runs to its own end"
            ),
        }
    }

    /// Resolve the signal cancelled, for a host.
    async fn request_cancel(&self) -> Result<PluginTaskCancelRequest, RuntimeError> {
        let requested = self
            .host
            .await_event_resolver()
            .resolve_await_event(&self.key, crate::Resolution::Cancelled)
            .await?;
        match requested {
            crate::ResolveOutcome::Accepted
            | crate::ResolveOutcome::AlreadyResolved {
                terminal: crate::Resolution::Cancelled,
            } => Ok(PluginTaskCancelRequest::Requested),
            crate::ResolveOutcome::AlreadyResolved { terminal } => {
                Err(foreign_resolution(&self.key, &terminal))
            }
            crate::ResolveOutcome::UnknownOrRevoked => Ok(PluginTaskCancelRequest::Unavailable),
        }
    }
}

/// Cancel the plugin task `receipt` names after a drive admitted it
/// (FIG-4391), over the session's `store` and the deployment's
/// `effect_host`: a host's cancel once withdrawing the command
/// ([`cancel_queued_work_batch`](crate::RuntimeHandle::cancel_queued_work_batch))
/// no longer reaches it. It takes no runtime writer, which the drive applying
/// the task may hold.
///
/// A command that already settled answers
/// [`AlreadySettled`](PluginTaskCancelRequest::AlreadySettled). Otherwise the
/// cancel resolves the task's cancel signal: the drive stops the task's code
/// through its cancellation token and settles the command `Cancelled`, with
/// nothing of the task committed, unless it already found the task's code
/// returned with no cancel requested (FIG-4453). Either way the command's
/// settlement, read back by its receipt, is how it ended.
pub async fn request_plugin_task_cancel(
    store: &dyn crate::store::RuntimeStore,
    effect_host: &Arc<dyn crate::EffectHost>,
    receipt: &crate::SessionCommandReceipt,
) -> Result<PluginTaskCancelRequest, RuntimeError> {
    let settled = store
        .queued_work_batch_completion(&receipt.session_id, receipt.batch_id.as_str())
        .await
        .map_err(|error| RuntimeError::new(RuntimeErrorCode::RuntimeStore, error.to_string()))?;
    if settled.is_some() {
        return Ok(PluginTaskCancelRequest::AlreadySettled);
    }
    let Some(signal) = PluginTaskCancelSignal::open(
        Arc::clone(effect_host),
        &receipt.session_id,
        receipt.batch_id.as_str(),
    )
    .await?
    else {
        return Ok(PluginTaskCancelRequest::Unavailable);
    };
    signal.request_cancel().await
}
