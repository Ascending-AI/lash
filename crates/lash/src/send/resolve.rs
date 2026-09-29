//! Outcome derivation (FIG-3600 S5b, D1 §1.5): an input's root, then that
//! root's terminal or its park, read from the store alone.
//!
//! Resolution makes no engine call. It is engine-neutral, so the interim SQL
//! engine and Restate share it, and it answers the same after a restart.

use std::time::Duration;

use lash_core::drive::{physical_turn_of, root_of_physical_turn};
use lash_core::facade_support::{TurnAddress, TurnOutcome, TurnTerminal, TurnWorkDriver};
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::{InputId, TurnId};

use super::{ParkedTurn, SendParts, StalledDelivery};
use crate::error::Result;

/// How long one read waits for a committed turn's terminal publication.
const TERMINAL_READ: Duration = Duration::from_millis(250);

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

/// Resolve a logical root.
pub(super) async fn resolve_root(parts: &SendParts, root: &TurnId) -> Result<Resolution> {
    resolve_from_turn(parts, root).await
}

/// Follow `turn`'s root from `turn` to its final physical turn: a frame
/// switch continues the root in its next physical turn.
async fn resolve_from_turn(parts: &SendParts, turn: &TurnId) -> Result<Resolution> {
    let (root, mut ordinal) = root_of_physical_turn(turn);
    loop {
        let physical = physical_turn_of(&root, ordinal);
        match terminal_of(parts, &physical).await? {
            Some(TurnTerminal::Committed {
                outcome: TurnOutcome::AgentFrameSwitch { .. },
                ..
            }) => {
                ordinal = ordinal.saturating_add(1);
            }
            Some(TurnTerminal::Committed { outcome, .. }) => {
                return Ok(Resolution::Settled { root, outcome });
            }
            Some(TurnTerminal::Failed { .. }) | None => {
                if let Some(parked) = park_of(parts, &root).await? {
                    return Ok(Resolution::Parked(parked));
                }
                if let Some(refusal) = refusal_of(parts, &root).await? {
                    return Ok(Resolution::Refused { root, refusal });
                }
                return Ok(Resolution::Undecided { root: Some(root) });
            }
        }
    }
}

/// The published terminal of one physical turn, once its commit is durable.
async fn terminal_of(parts: &SendParts, turn: &TurnId) -> Result<Option<TurnTerminal>> {
    let address = TurnAddress::new(parts.session_id.clone(), turn.clone());
    match parts.store.turn_is_committed(&address).await {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        // A store that cannot answer the commit read is asked for the
        // terminal directly.
        Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => {}
        Err(error) => return Err(store_error(error)),
    }
    let driver = TurnWorkDriver::for_session(
        std::sync::Arc::clone(&parts.effect_host),
        parts.session_id.to_string(),
        std::sync::Arc::clone(parts.store.store()),
    );
    match driver
        .await_terminal_with_timeout(&address, TERMINAL_READ)
        .await
    {
        Ok(terminal) => Ok(Some(terminal)),
        Err(error) => {
            tracing::debug!(
                session_id = %parts.session_id,
                turn_id = %turn,
                error = %error,
                "committed turn's terminal is not readable yet"
            );
            Ok(None)
        }
    }
}

/// The refusal `root`'s run ended with, when its terminal evidence is one.
async fn refusal_of(parts: &SendParts, root: &TurnId) -> Result<Option<lash_core::RuntimeError>> {
    let terminal = match parts.store.root_terminal(root).await {
        Ok(terminal) => terminal,
        Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => None,
        Err(error) => return Err(store_error(error)),
    };
    Ok(terminal.and_then(|terminal| match terminal.cause {
        lash_core::store::RootTerminalCause::Refused {
            code,
            message,
            refusal_cause,
        } => {
            // The structured cause is the refusal's type: a session-retirement
            // refusal must answer as one, not as its bare code.
            let mut refusal = lash_core::RuntimeError::new(code, message);
            refusal.cause = refusal_cause;
            Some(refusal)
        }
        _ => None,
    }))
}

/// The session's park, when it holds `root`.
async fn park_of(parts: &SendParts, root: &TurnId) -> Result<Option<ParkedTurn>> {
    let park = match parts.store.load_turn_park().await {
        Ok(park) => park,
        Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => None,
        Err(error) => return Err(store_error(error)),
    };
    Ok(park
        .filter(|park| park.turn_id == *root || root_of_physical_turn(&park.turn_id).0 == *root)
        .map(|park| ParkedTurn {
            session_id: park.session_id,
            root: root.clone(),
            park_id: park.park_id,
            reason: park.reason,
            since_ms: park.since_ms,
            attempts: park.attempts,
        }))
}
