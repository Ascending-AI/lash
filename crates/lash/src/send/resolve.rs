//! Outcome derivation (FIG-3600 S5b, D1 §1.5): an input's root, then that
//! root's terminal or its park, read from the store alone.
//!
//! Resolution makes no engine call. It is engine-neutral, so the interim SQL
//! engine and Restate share it, and it answers the same after a restart.

use std::time::Duration;

use lash_core::drive::{physical_turn_of, root_of_physical_turn};
use lash_core::facade_support::{TurnAddress, TurnOutcome, TurnTerminal, TurnWorkDriver};
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::{InputId, SessionId, TurnId};

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

/// The root an input starts when no turn applied it yet: the host's id, or
/// the input's own id (the admission rule).
pub(super) fn input_root(receipt: &TurnInputAcceptanceReceipt) -> TurnId {
    TurnId::from(
        receipt
            .source_key
            .clone()
            .unwrap_or_else(|| receipt.input_id.to_string()),
    )
}

/// The session's input applications in commit order, or `None` when the
/// store keeps no application records (it cannot say which turn applied an
/// input, so an input's own root is the only one a handle can name).
pub(super) async fn applications(
    parts: &SendParts,
) -> Result<Option<Vec<lash_core::TurnInputApplication>>> {
    match parts
        .store
        .list_turn_input_applications(&parts.session_id)
        .await
    {
        Ok(applications) => Ok(Some(applications)),
        Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => Ok(None),
        Err(error) => Err(store_error(error)),
    }
}

/// The physical turn that applied `input`, when one committed.
fn applying_turn(
    applications: &[lash_core::TurnInputApplication],
    input: &InputId,
) -> Option<TurnId> {
    applications
        .iter()
        .find(|application| application.input_id == *input)
        .map(|application| application.turn_id.clone())
}

/// Resolve an accepted input.
pub(super) async fn resolve_input(
    parts: &SendParts,
    receipt: &TurnInputAcceptanceReceipt,
) -> Result<Resolution> {
    let recorded = applications(parts).await?;
    if let Some(turn) = recorded
        .as_deref()
        .and_then(|applications| applying_turn(applications, &receipt.input_id))
    {
        return resolve_from_turn(parts, &turn).await;
    }
    let own_root = input_root(receipt);
    if recorded.is_none() {
        let own = resolve_from_turn(parts, &own_root).await?;
        if !matches!(own, Resolution::Undecided { .. }) {
            return Ok(own);
        }
    }
    if let Some(parked) = park_of(parts, &own_root).await? {
        return Ok(Resolution::Parked(parked));
    }
    let open = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .map_err(store_error)?
        .iter()
        .any(|read| read.input.input_id == receipt.input_id);
    if open {
        return Ok(Resolution::Undecided { root: None });
    }
    // Gone from the queue: either a commit that applied it raced the first
    // read (the commit writes the application and settles the row in one
    // transaction), or it was withdrawn.
    match applications(parts).await? {
        Some(applications) => match applying_turn(&applications, &receipt.input_id) {
            Some(turn) => resolve_from_turn(parts, &turn).await,
            None => Ok(Resolution::Withdrawn),
        },
        // Without application records, an input that left the queue was
        // applied by its own root once that root committed a turn; a store
        // that cannot say whether it did leaves the input undecided.
        None => {
            let own = resolve_from_turn(parts, &own_root).await?;
            if !matches!(own, Resolution::Undecided { .. }) {
                return Ok(own);
            }
            match parts
                .store
                .turn_is_committed(&TurnAddress::new(
                    parts.session_id.clone(),
                    own_root.clone(),
                ))
                .await
            {
                Ok(false) => Ok(Resolution::Withdrawn),
                Ok(true) | Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => {
                    Ok(Resolution::Undecided {
                        root: Some(own_root),
                    })
                }
                Err(error) => Err(store_error(error)),
            }
        }
    }
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
        .unwrap_or_default()
        .into_iter()
        .filter(|application| root_of_physical_turn(&application.turn_id).0 == *root)
        .map(|application| application.input_id)
        .collect())
}

/// D2 Q6: the settled input whose host id is `id`, when root `id` already
/// has terminal evidence. A send under that id then commits nothing.
pub(super) async fn settled_by_id(
    parts: &SendParts,
    id: &TurnId,
) -> Result<Option<(TurnInputAcceptanceReceipt, TurnOutcome)>> {
    let Some(application) = applications(parts)
        .await?
        .unwrap_or_default()
        .into_iter()
        .find(|application| application.source_key.as_deref() == Some(id.as_str()))
    else {
        return Ok(None);
    };
    match resolve_from_turn(parts, id).await? {
        Resolution::Settled { outcome, .. } => Ok(Some((
            TurnInputAcceptanceReceipt {
                input_id: application.input_id,
                session_id: SessionId::from(parts.session_id.to_string()),
                source_key: application.source_key,
                ingress: lash_core::runtime::TurnInputIngress::NextTurn,
            },
            outcome,
        ))),
        _ => Ok(None),
    }
}
