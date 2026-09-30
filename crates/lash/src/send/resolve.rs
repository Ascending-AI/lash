//! Outcome derivation (FIG-3600 S5b, D1 §1.5): an input's root, then that
//! root's terminal or its park, read from the store alone.
//!
//! Resolution makes no engine call. It is engine-neutral, so the interim SQL
//! engine and Restate share it, and it answers the same after a restart. A
//! committed root answers from its terminal evidence, which the head commit
//! of its final physical turn writes with the outcome it committed: one
//! read, however deep the engine's queues are (FIG-4345).

use lash_core::facade_support::TurnOutcome;
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::store::{PhysicalTurn, RootTerminalCause};
use lash_core::{InputId, TurnId};

use super::{ParkedTurn, SendParts, StalledDelivery};
use crate::error::Result;

/// What the store says about an input or a root, right now.
#[derive(Debug)]
pub(super) enum Resolution {
    /// Not settled yet. `root` is known once a turn applied the input.
    Undecided { root: Option<TurnId> },
    /// The input left the queue without any turn applying it.
    Withdrawn,
    /// The root's final physical turn committed with `outcome`.
    Settled { root: TurnId, outcome: TurnOutcome },
    /// The root is parked (ADR 0104 O3): durable, and not terminal.
    Parked(ParkedTurn),
    /// The root's run ended with `refusal`, a typed refusal no retry could
    /// change, and no turn of it committed (FIG-4018).
    Refused {
        root: TurnId,
        refusal: lash_core::RuntimeError,
    },
    /// The input is open and its delivery to the engine stalled (ADR 0109
    /// §3): no drive will take it until its obligation is re-armed.
    Stalled(StalledDelivery),
}

fn store_error(error: lash_core::StoreError) -> crate::EmbedError {
    crate::EmbedError::Store(error)
}

/// Resolve an accepted input through the durable binding made by its claim.
pub(super) async fn resolve_input(
    parts: &SendParts,
    receipt: &TurnInputAcceptanceReceipt,
) -> Result<Resolution> {
    if let Some(root) = parts
        .store
        .root_of_input(&receipt.input_id)
        .await
        .map_err(store_error)?
    {
        return resolve_root(parts, &root).await;
    }
    let open = parts
        .store
        .pending_turn_input(&receipt.input_id)
        .await
        .map_err(store_error)?
        .is_some();
    // Claim and settlement can race the pending read. Re-read the binding
    // before interpreting a missing row as a withdrawal.
    if let Some(root) = parts
        .store
        .root_of_input(&receipt.input_id)
        .await
        .map_err(store_error)?
    {
        return resolve_root(parts, &root).await;
    }
    if !open {
        return Ok(Resolution::Withdrawn);
    }
    Ok(match stalled_delivery(parts, &receipt.input_id).await? {
        Some(stalled) => Resolution::Stalled(stalled),
        None => Resolution::Undecided { root: None },
    })
}

/// The open input's delivery, when its ingress obligation stalled. A store
/// that keeps no obligations has none stalled.
async fn stalled_delivery(parts: &SendParts, input: &InputId) -> Result<Option<StalledDelivery>> {
    let stalled = match parts.ops.stalled_ingress(input.as_str()).await {
        Ok(stalled) => stalled,
        Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => None,
        Err(error) => return Err(store_error(error)),
    };
    Ok(stalled.map(|stalled| StalledDelivery {
        session_id: parts.session_id.clone(),
        input_id: input.clone(),
        reason: stalled.reason,
        attempts: stalled.attempts,
        last_error: stalled.last_error,
        stalled_at_ms: stalled.stalled_at_ms,
    }))
}

/// Resolve a logical root from its durable record: its terminal evidence,
/// then its park.
///
/// A root the head commit of its final physical turn ended is settled with
/// the outcome that commit wrote. A parked root is parked. A root whose run
/// ended with a typed refusal is refused. Any other root is undecided.
pub(super) async fn resolve_root(parts: &SendParts, root: &TurnId) -> Result<Resolution> {
    let cause = parts
        .store
        .root_terminal(root)
        .await
        .map_err(store_error)?
        .map(|terminal| terminal.cause);
    let refusal = match cause {
        Some(RootTerminalCause::Committed { outcome, .. }) => {
            return Ok(Resolution::Settled {
                root: root.clone(),
                outcome: TurnOutcome::from(outcome),
            });
        }
        Some(RootTerminalCause::Refused {
            code,
            message,
            refusal_cause,
        }) => {
            // The structured cause is the refusal's type: a session-retirement
            // refusal must answer as one, not as its bare code.
            let mut refusal = lash_core::RuntimeError::new(code, message);
            refusal.cause = refusal_cause;
            Some(refusal)
        }
        // An operator's end, the session's deletion or a lost run carries no
        // answer of its own.
        Some(
            RootTerminalCause::OperatorCancelled { .. }
            | RootTerminalCause::Forked { .. }
            | RootTerminalCause::SessionDeleted { .. }
            | RootTerminalCause::SubstrateLost { .. },
        )
        | None => None,
    };
    if let Some(parked) = park_of(parts, root).await? {
        return Ok(Resolution::Parked(parked));
    }
    if let Some(refusal) = refusal {
        return Ok(Resolution::Refused {
            root: root.clone(),
            refusal,
        });
    }
    Ok(Resolution::Undecided {
        root: Some(root.clone()),
    })
}

/// The session's park, when it holds `root`.
async fn park_of(parts: &SendParts, root: &TurnId) -> Result<Option<ParkedTurn>> {
    let park = match parts.store.load_turn_park().await {
        Ok(park) => park,
        Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => None,
        Err(error) => return Err(store_error(error)),
    };
    Ok(park
        .filter(|park| PhysicalTurn::physical_ordinal_of(root, &park.turn_id).is_some())
        .map(|park| ParkedTurn {
            session_id: park.session_id,
            root: root.clone(),
            park_id: park.park_id,
            reason: park.reason,
            since_ms: park.since_ms,
            attempts: park.attempts,
        }))
}
