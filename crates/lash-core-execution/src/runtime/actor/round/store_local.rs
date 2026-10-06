//! Store-local effects: a built-in tool's lash store write, committed in the
//! transaction that records its outcome, so the effect happens exactly once
//! (ADR 0132 §5).

use lash_durable::domain::ProcessWrite;
use lash_durable::{ActorTx, DomainWrite};

use super::{AdmittedExecution, SettleRefusal, StoreLocalEffect};

/// Write `effect`, the store half of `admitted`'s completion, on `tx`.
///
/// # Errors
///
/// [`SettleRefusal::ForeignEffect`] for an effect this store cannot commit
/// with an outcome.
pub(super) fn write(
    tx: &mut ActorTx,
    admitted: &AdmittedExecution,
    effect: StoreLocalEffect,
) -> Result<(), SettleRefusal> {
    match effect {
        StoreLocalEffect::ProcessStart(rows) => {
            tx.write(DomainWrite::Process(ProcessWrite::Register(rows)));
            Ok(())
        }
        StoreLocalEffect::TriggerCreate(_)
        | StoreLocalEffect::TriggerDelete(_)
        | StoreLocalEffect::SignalSend(_)
        | StoreLocalEffect::ChildSessionSpawn(_)
        | StoreLocalEffect::RealizationStore(_) => {
            Err(SettleRefusal::ForeignEffect(admitted.call().clone()))
        }
    }
}
