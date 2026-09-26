//! Cancel, withdraw (FIG-3600 S5b, D1 §1.6): an input still queued is
//! withdrawn; an input whose root runs has the root cancelled cooperatively
//! through its cancellation gate (ADR 0039).

use lash_core::drive::{physical_turn_of, root_of_physical_turn};
use lash_core::facade_support::{
    TurnAddress, TurnCancelDisposition, TurnCancelMode, TurnCancelRequest, TurnWorkDriver,
};
use lash_core::runtime::{
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
};
use lash_core::{InputId, TurnId};

use super::resolve::{self, Resolution};
use super::{CancelReceipt, CancelTarget, SendParts};
use crate::error::{EmbedError, Result};

/// The request fields a cancel carries.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelRequestSpec {
    pub(crate) request_id: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) mode: TurnCancelMode,
    pub(crate) undelivered: TurnCancelDisposition,
}

pub(super) async fn apply(
    parts: &SendParts,
    target: &CancelTarget,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    match target {
        CancelTarget::Input(input) => cancel_input(parts, input, request).await,
        CancelTarget::Root(root) => {
            let request_id = request
                .request_id
                .clone()
                .unwrap_or_else(|| format!("cancel:root:{root}"));
            cancel_root(parts, root, request_id, request).await
        }
    }
}

async fn cancel_input(
    parts: &SendParts,
    input: &InputId,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    let outcome = parts
        .ops
        .cancel_pending_turn_input(&parts.store, input.as_str())
        .await?;
    let request_id = request
        .request_id
        .clone()
        .unwrap_or_else(|| format!("cancel:input:{input}"));
    match outcome {
        outcome @ (PendingTurnInputCancelOutcome::Cancelled(_)
        | PendingTurnInputCancelOutcome::AlreadyCancelled(_)) => Ok(CancelReceipt::Withdrawn(
            Box::new(PendingTurnInputCancelReceipt {
                target: PendingTurnInputCancelTarget::input_id(input.to_string()),
                outcome,
            }),
        )),
        PendingTurnInputCancelOutcome::NotFound => Ok(CancelReceipt::NotFound),
        // A completed input was applied by a committed turn, which is not
        // the root's end: a root that switched frames runs on in its
        // follow-on turns, so its root answers whether it settled.
        PendingTurnInputCancelOutcome::AlreadyCompleted(_)
        | PendingTurnInputCancelOutcome::AlreadyClaimed { .. }
        | PendingTurnInputCancelOutcome::TurnBound { .. } => {
            let root = root_of_input(parts, input).await?;
            cancel_root(parts, &root, request_id, request).await
        }
    }
}

/// The root that took `input`, from its durable binding: the claim binds it
/// before any turn commits, so a cancel reaches the consuming root even
/// before the input has application evidence.
async fn root_of_input(parts: &SendParts, input: &InputId) -> Result<TurnId> {
    if let Some(root) = parts.store.root_of_input(&parts.session_id, input).await? {
        return Ok(root);
    }
    resolve::applications(parts)
        .await?
        .into_iter()
        .find(|application| application.input_id == *input)
        .map(|application| root_of_physical_turn(&application.turn_id).0)
        .ok_or_else(|| {
            EmbedError::from(crate::SendError::Unresolved {
                input_id: input.clone(),
            })
        })
}

async fn cancel_root(
    parts: &SendParts,
    root: &TurnId,
    request_id: String,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    if let Resolution::Settled { .. } = resolve::resolve_root(parts, root).await? {
        return Ok(CancelReceipt::AlreadySettled { root: root.clone() });
    }
    // The cancel addresses the root's running physical turn: the first of its
    // turns that has not committed.
    let mut ordinal = 0_u64;
    let turn = loop {
        let turn = physical_turn_of(root, ordinal);
        let committed = parts
            .store
            .turn_is_committed(&TurnAddress::new(parts.session_id.clone(), turn.clone()))
            .await
            .unwrap_or(false);
        if !committed {
            break turn;
        }
        ordinal = ordinal.saturating_add(1);
    };
    let mut cancel = TurnCancelRequest::new(
        TurnAddress::new(parts.session_id.clone(), turn),
        request_id,
        request.origin,
    )
    .undelivered(request.undelivered)
    .mode(request.mode);
    cancel.reason = request.reason;
    let receipt = TurnWorkDriver::for_session(
        std::sync::Arc::clone(&parts.effect_host),
        parts.session_id.to_string(),
        std::sync::Arc::clone(&parts.store),
    )
    .request_cancel(cancel)
    .await
    .map_err(EmbedError::Runtime)?;
    Ok(CancelReceipt::Requested {
        root: root.clone(),
        receipt: Box::new(receipt),
    })
}
