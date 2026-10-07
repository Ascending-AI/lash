//! Store-local effects: a built-in tool's lash store write, committed in the
//! transaction that records its outcome, so the effect happens exactly once
//! (ADR 0132 §5).

use lash_durable::domain::ProcessWrite;
use lash_durable::{ActorTx, CommitLabel, DomainWrite};

use super::super::ActorContext;
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
    write_effect(tx, effect).map_err(|_| SettleRefusal::ForeignEffect(admitted.call().clone()))
}

/// Write `effect` on `tx`; an effect no store commits yet comes back.
fn write_effect(tx: &mut ActorTx, effect: StoreLocalEffect) -> Result<(), StoreLocalEffect> {
    match effect {
        StoreLocalEffect::ProcessStart(rows) => {
            tx.write(DomainWrite::Process(ProcessWrite::Register(rows)));
            Ok(())
        }
        StoreLocalEffect::SignalSend(rows) => {
            tx.write(DomainWrite::Process(ProcessWrite::Signal {
                process: rows.process,
                signal_json: rows.signal_json,
            }));
            Ok(())
        }
        effect @ (StoreLocalEffect::TriggerCreate(_)
        | StoreLocalEffect::TriggerDelete(_)
        | StoreLocalEffect::ChildSessionSpawn(_)) => Err(effect),
    }
}

impl ActorContext {
    /// Commit `effects`, the store-local effects of a call that no round
    /// member runs, in one fenced transaction of their own (`tool.effect`):
    /// a code cell's call ends in memory, and nothing else records its
    /// outcome until FIG-5225 admits it as a round member.
    ///
    /// # Errors
    ///
    /// An effect no store commits yet, ownership lost, or the store's
    /// refusal; nothing is written.
    pub async fn commit_store_local(
        &self,
        effects: Vec<StoreLocalEffect>,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let mut tx = self.begin().await.map_err(store_error)?;
        for effect in effects {
            if let Err(effect) = write_effect(&mut tx, effect) {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                    format!("no store commits the store-local effect {effect:?}"),
                ));
            }
        }
        self.commit(tx, CommitLabel::TOOL_EFFECT)
            .await
            .map(drop)
            .map_err(store_error)
    }
}

/// A commit that did not land: a redrivable failure of the call, whose
/// outcome its owner's next activation recovers.
fn store_error(error: lash_durable::DurableError) -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::StoreCommitFailed,
        error.to_string(),
    )
}
