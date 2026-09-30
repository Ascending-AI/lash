//! A host's cancel of a plugin task a drive already admitted (FIG-4391).
//!
//! Withdrawal reaches only a command no drive has read (FIG-4202). Once a
//! drive admitted a plugin task, a host's cancel goes through the task's
//! cancel gate: a first-writer-wins keyed promise under the command's own
//! queue-drain scope. The host resolves it cancelled; the drive seals it the
//! moment the task's code returns, before anything of the task is folded
//! into resident state. Whichever wrote first decides how the command
//! settles: a cancel that won settles it
//! [`Cancelled`](crate::PluginOperationCommandOutcome::Cancelled) with
//! nothing of the task committed, and a seal that won keeps the task's own
//! outcome. The drive commits that decision as the command's settlement,
//! which is the recorded outcome every replay and every submitter reads back
//! (ADR 0105 §1). A redrive before that commit meets the same winner, since a
//! resolved gate never changes.
//!
//! While the task runs, the drive watches the gate and fires the task's
//! cancellation token when the cancel lands. The token only stops the
//! task's code; what the command settles as is the gate's winner. A watch
//! that fails is not a cancel: the task runs to its own end, and the seal
//! still finds a cancel that won.

use super::*;
use tokio_util::sync::CancellationToken;

/// What a host's cancel of an admitted plugin task did (FIG-4391).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginTaskCancelRequest {
    /// The cancel won the task's cancel gate, in this request or an earlier
    /// one: the command settles `Cancelled`, with nothing of the task
    /// committed.
    Requested,
    /// The task's drive sealed the gate first: the task's code already
    /// returned, and the command settles with the task's own outcome.
    TaskSettled,
    /// No gate carries the cancel: the effect host keeps no durable
    /// await-event keys, or the session's waits are revoked. The command
    /// settles as its drive applies it.
    Unavailable,
}

/// The resolution a task's drive seals its cancel gate with once the task's
/// code returned.
fn task_returned_seal() -> crate::Resolution {
    crate::Resolution::Ok(serde_json::Value::String("task_returned".to_string()))
}

/// Whether `error` says the effect host holds no gate for the task: it mints
/// no durable keys, or the session's waits are revoked.
fn gate_unavailable(error: &RuntimeError) -> bool {
    matches!(
        error.code,
        RuntimeErrorCode::AwaitEventUnsupported | RuntimeErrorCode::AwaitEventUnknownOrRevoked
    )
}

/// One admitted plugin task's cancel gate, over the deployment's effect host.
pub(super) struct PluginTaskCancelGate {
    host: Arc<dyn crate::EffectHost>,
    key: crate::AwaitEventKey,
}

impl PluginTaskCancelGate {
    /// The cancel gate of the plugin task command `batch_id` names, over
    /// `host`. `None` when `host` holds no gate for it.
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
                crate::AwaitEventWaitIdentity::SessionCommandCancelGate,
            )
            .await;
        match minted {
            Ok(key) => Ok(Some(Self { host, key })),
            Err(error) if gate_unavailable(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether a host's cancel already won the gate. A drive that admits the
    /// task after the cancel landed runs none of its code.
    pub(super) async fn cancel_won(&self) -> Result<bool, RuntimeError> {
        match self
            .host
            .await_event_resolver()
            .peek_await_event(&self.key)
            .await
        {
            Ok(resolution) => Ok(matches!(resolution, Some(crate::Resolution::Cancelled))),
            Err(error) if gate_unavailable(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Execution-side only: fire `stop` when a host's cancel resolves the
    /// gate. It ends once the gate resolved or the watch failed; the drive
    /// drops it when the task's code returns first.
    pub(super) async fn watch(&self, stop: &CancellationToken) {
        // Never a fired token: firing the waiter's token would resolve the
        // gate itself cancelled.
        let watched = self
            .host
            .await_event_resolver()
            .await_await_event(&self.key, CancellationToken::new(), None)
            .await;
        match watched {
            Ok(crate::Resolution::Cancelled) => stop.cancel(),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                %error,
                key = %self.key.key_id,
                "a plugin task's cancel watch failed; the task runs to its own end"
            ),
        }
    }

    /// Seal the gate once the task's code returned: `true` when a host's
    /// cancel won it first, and the command settles `Cancelled`.
    pub(super) async fn seal(&self) -> Result<bool, RuntimeError> {
        let sealed = self
            .host
            .await_event_resolver()
            .resolve_await_event(&self.key, task_returned_seal())
            .await?;
        Ok(match sealed {
            crate::ResolveOutcome::Accepted | crate::ResolveOutcome::UnknownOrRevoked => false,
            crate::ResolveOutcome::AlreadyResolved { terminal } => {
                matches!(terminal, crate::Resolution::Cancelled)
            }
        })
    }

    /// Resolve the gate cancelled, for a host.
    async fn request_cancel(&self) -> Result<PluginTaskCancelRequest, RuntimeError> {
        let requested = self
            .host
            .await_event_resolver()
            .resolve_await_event(&self.key, crate::Resolution::Cancelled)
            .await?;
        Ok(match requested {
            crate::ResolveOutcome::Accepted
            | crate::ResolveOutcome::AlreadyResolved {
                terminal: crate::Resolution::Cancelled,
            } => PluginTaskCancelRequest::Requested,
            crate::ResolveOutcome::AlreadyResolved { .. } => PluginTaskCancelRequest::TaskSettled,
            crate::ResolveOutcome::UnknownOrRevoked => PluginTaskCancelRequest::Unavailable,
        })
    }
}

/// Cancel the plugin task `receipt` names after a drive admitted it
/// (FIG-4391), over the deployment's `effect_host`: a host's cancel once
/// withdrawing the command
/// ([`cancel_queued_work_batch`](crate::RuntimeHandle::cancel_queued_work_batch))
/// no longer reaches it. It takes no runtime writer, which the drive applying
/// the task may hold.
///
/// The cancel resolves the task's cancel gate. When it wins the gate, the
/// drive stops the task's code through its cancellation token and settles
/// the command `Cancelled`, with nothing of the task committed; the
/// settlement is read back by the command's receipt as any other. When the
/// task's code already returned, the command settles with the task's own
/// outcome.
pub async fn request_plugin_task_cancel(
    effect_host: &Arc<dyn crate::EffectHost>,
    receipt: &crate::SessionCommandReceipt,
) -> Result<PluginTaskCancelRequest, RuntimeError> {
    let Some(gate) = PluginTaskCancelGate::open(
        Arc::clone(effect_host),
        &receipt.session_id,
        receipt.batch_id.as_str(),
    )
    .await?
    else {
        return Ok(PluginTaskCancelRequest::Unavailable);
    };
    gate.request_cancel().await
}
