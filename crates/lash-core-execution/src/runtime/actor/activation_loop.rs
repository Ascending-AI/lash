//! The activation-loop budget as a claim reads it, the same for a process
//! and a session (ADR 0132 §3; FIG-6096): the store counts the claims in a
//! row that found no commit since the previous claim, an activation that
//! panicked or gave up a read among them, and an activation whose claim
//! reached the budget parks its actor instead of running it again.

use lash_durable::ActorSnapshot;
use lash_durable::runner::{Exit, Owned};

/// The actor `owned` holds, as of its claim: read again at the claim-poll
/// pace while the read fails and the node holds the actor. `Err` is the
/// activation's exit: [`Exit::Released`] when the row is gone, and
/// [`Exit::Abandoned`] when a draining node gives the read up.
///
/// # Errors
///
/// The activation's [`Exit`] when it ends without the row.
pub async fn claimed_row(owned: &Owned) -> Result<ActorSnapshot, Exit> {
    loop {
        match owned.store().actor(owned.actor()).await {
            Ok(Some(snapshot)) => return Ok(snapshot),
            Ok(None) => return Err(Exit::Released),
            Err(_) if owned.draining() => return Err(Exit::Abandoned),
            Err(error) => {
                tracing::debug!(%error, actor = %owned.actor(), "actor read failed; reading again");
                owned.wait_for_mail().await;
            }
        }
    }
}

/// Whether `failed_activations` claims in a row with nothing committed
/// reached `budget`: the claim parks its actor instead of running it.
#[must_use]
pub const fn reached(failed_activations: u32, budget: u32) -> bool {
    failed_activations >= budget
}
