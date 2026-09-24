//! The SQL store driver's answer to the served-only contract (FIG-3719).

use super::*;

/// Refuses a served-only effect whose outcome the journal does not hold.
///
/// The driver asks its row store for a settled row at the effect's key
/// before it claims anything: a served-only effect runs only from its
/// recorded outcome, so one with none refuses with the effect's drift and the
/// driver writes nothing — no claim, no failure row.
pub(super) async fn refuse<P: EffectReplayRowStore, A: AwaitEventBackend>(
    driver: &StoreEffectReplayDriver<P, A>,
    scope: &ExecutionScope,
    envelope: &RuntimeEffectEnvelope,
    served_only: Option<crate::ServedOnly>,
) -> Result<(), RuntimeEffectControllerError> {
    let Some(served_only) = served_only else {
        return Ok(());
    };
    let key = envelope.invocation.replay_key();
    let identity = scope
        .journal_identity()
        .map_err(RuntimeEffectControllerError::from)?;
    if driver
        .row_store
        .replay_row_settled(identity.key(), key)
        .await?
    {
        return Ok(());
    }
    Err(served_only.refuse())
}
