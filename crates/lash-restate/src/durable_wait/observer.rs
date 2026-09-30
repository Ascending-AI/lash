//! Observer-only attaches to durable waits from outside any handler
//! (FIG-4345): callers that read a wait's resolution and never resolve it
//! peek before they register, and share one server-side waiter per wait.

use lash_core::{AwaitEventKey, AwaitEventWaitIdentity, Resolution};

use super::{RestateDurableWaitAddress, RestateDurableWaitAwaitRequest};
use crate::ingress::{RestateHttpError, RestateIngressClient};

/// A family of observer-only attaches to durable waits from outside any
/// handler: callers that read a wait's resolution and never resolve it.
/// Every attach of one family to one wait shares one server-side waiter
/// (FIG-3672 P9, FIG-4345).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitObserver {
    /// A watch of one turn's cancellation gate or escalation: each model
    /// call watches it for its own lifetime and drops its call when it ends.
    TurnCancel,
    /// An attach to one turn's terminal publication: a follower's wait, held
    /// and dropped once per probe window.
    TurnTerminal,
    /// A recorded step body's live watch of its effect-group child's cancel
    /// fact, which the journal never sees.
    GroupChildCancel,
}

impl WaitObserver {
    /// The observer family `key`'s wait identity names, when it names one.
    pub(crate) fn of_key(key: &AwaitEventKey) -> Option<Self> {
        match key.wait {
            AwaitEventWaitIdentity::TurnCancelGate
            | AwaitEventWaitIdentity::TurnCancelEscalation => Some(Self::TurnCancel),
            AwaitEventWaitIdentity::TurnTerminal => Some(Self::TurnTerminal),
            AwaitEventWaitIdentity::ToolCompletion { .. }
            | AwaitEventWaitIdentity::ProcessSignal { .. }
            | AwaitEventWaitIdentity::Custom { .. } => None,
        }
    }

    /// The idempotency key every attach of this family to `key` carries.
    fn attachment(self, key: &AwaitEventKey) -> String {
        let family = match self {
            Self::TurnCancel => "lash-turn-cancel-watch",
            Self::TurnTerminal => "lash-turn-terminal-watch",
            Self::GroupChildCancel => "lash-group-child-cancel-watch",
        };
        format!("{family}:{}", key.key_id)
    }
}

/// An `observer`'s attach to `request.key`'s durable wait through the
/// ingress, from outside any handler (FIG-4345).
///
/// It peeks the wait's promise through the workflow's shared `peek` first: a
/// resolved wait answers from its promise and never reaches the scope's
/// exclusive wait index. An unresolved wait is attached under the family's
/// idempotency key, so every attach of the family joins one
/// `await_resolution` invocation, the waiter its first attach opened (a
/// dropped attach does not cancel it), and reads that invocation's retained
/// result once it has one. However often observers attach and drop, the
/// index sees one `register` and one `settle` for the wait.
#[allow(
    clippy::result_large_err,
    reason = "the attach answers the ingress client's own RestateHttpError"
)]
pub(crate) async fn observe_durable_wait(
    ingress: &RestateIngressClient,
    durable_wait_workflow: &str,
    observer: WaitObserver,
    request: &RestateDurableWaitAwaitRequest,
) -> Result<Resolution, RestateHttpError> {
    let workflow_key = RestateDurableWaitAddress::for_key(&request.key).workflow_key;
    if let Some(resolution) = ingress
        .call_lash_workflow::<_, Option<Resolution>>(
            durable_wait_workflow,
            &workflow_key,
            "peek",
            &(),
        )
        .await?
    {
        return Ok(resolution);
    }
    ingress
        .call_lash_workflow_idempotent::<_, Resolution>(
            durable_wait_workflow,
            &workflow_key,
            "await_resolution",
            request,
            &observer.attachment(&request.key),
        )
        .await
}
