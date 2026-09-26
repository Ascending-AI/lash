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

use super::{ParkedTurn, SendParts};
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
}

fn store_error(error: lash_core::StoreError) -> crate::EmbedError {
    crate::EmbedError::Store(error)
}

/// Committed input applications, in commit order.
///
/// Only an input no claim bound needs them: a checkpoint delivery acquires
/// application evidence at its turn's commit. A store that keeps no
/// application records (a test double) has none to show; every durable
/// backend keeps them.
pub(super) async fn applications(
    parts: &SendParts,
) -> Result<Vec<lash_core::TurnInputApplication>> {
    match parts
        .store
        .list_turn_input_applications(&parts.session_id)
        .await
    {
        Ok(applications) => Ok(applications),
        Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => Ok(Vec::new()),
        Err(error) => Err(store_error(error)),
    }
}

/// Resolve an accepted input through the durable binding made by its claim.
pub(super) async fn resolve_input(
    parts: &SendParts,
    receipt: &TurnInputAcceptanceReceipt,
) -> Result<Resolution> {
    if let Some(root) = parts
        .store
        .root_of_input(&parts.session_id, &receipt.input_id)
        .await
        .map_err(store_error)?
    {
        return resolve_root(parts, &root).await;
    }
    let open = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .map_err(store_error)?
        .iter()
        .any(|read| read.input.input_id == receipt.input_id);
    // Claim and settlement can race the pending read. Re-read the binding
    // before interpreting a missing row as a withdrawal.
    if let Some(root) = parts
        .store
        .root_of_input(&parts.session_id, &receipt.input_id)
        .await
        .map_err(store_error)?
    {
        return resolve_root(parts, &root).await;
    }
    // Checkpoint inputs in the interim ingress acquire application evidence
    // at commit. Never infer their root from their host id.
    if let Some(application) = applications(parts)
        .await?
        .into_iter()
        .find(|application| application.input_id == receipt.input_id)
    {
        return resolve_from_turn(parts, &application.turn_id).await;
    }
    Ok(if open {
        Resolution::Undecided { root: None }
    } else {
        Resolution::Withdrawn
    })
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
        std::sync::Arc::clone(&parts.store),
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

/// The session's park, when it holds `root`.
async fn park_of(parts: &SendParts, root: &TurnId) -> Result<Option<ParkedTurn>> {
    let park = match parts.store.load_turn_park(&parts.session_id).await {
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

/// The inputs a root's physical turns applied, in commit order.
pub(super) async fn inputs_of_root(parts: &SendParts, root: &TurnId) -> Result<Vec<InputId>> {
    Ok(applications(parts)
        .await?
        .into_iter()
        .filter(|application| root_of_physical_turn(&application.turn_id).0 == *root)
        .map(|application| application.input_id)
        .collect())
}
