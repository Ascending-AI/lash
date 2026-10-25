//! An admission draft owned by the turn until its first recorded boundary.
use super::*;
use lash_protocol_types::sync::MutexExt as _;

pub(in crate::runtime::actor) struct FirstCommit(std::sync::Mutex<Option<Pending>>);
struct Pending {
    tx: ActorTx,
    committed: Box<dyn FnOnce() + Send>,
}

impl FirstCommit {
    pub(super) fn stage(&self, tx: &mut ActorTx) -> Result<(), DurableError> {
        if let Some(pending) = self.0.lock_recover().as_ref() {
            tx.append(pending.tx.clone())?;
            if let Some(ack) = pending.tx.ack() {
                tx.ack_through(ack);
            }
            if let Some(formats) = pending.tx.formats() {
                tx.stamp_formats(formats.clone());
            }
        }
        Ok(())
    }

    pub(super) fn committed(&self) {
        let pending = self.0.lock_recover().take();
        if let Some(pending) = pending {
            (pending.committed)();
        }
    }
}

impl ActorContext {
    /// Carry an unsubmitted admission into the first owner transaction opened
    /// through this context. All scoped clones share the draft and its one
    /// acknowledgement. Dropping them before a commit discards the admission;
    /// the independently accepted input remains available to the next owner.
    /// The original epoch and revision are checked on every composed draft.
    #[doc(hidden)]
    #[must_use]
    pub fn with_first_commit(
        &self,
        tx: ActorTx,
        committed: impl FnOnce() + Send + 'static,
    ) -> Self {
        let mut context = self.clone();
        context.first_commit = Some(Arc::new(FirstCommit(std::sync::Mutex::new(Some(
            Pending {
                tx,
                committed: Box::new(committed),
            },
        )))));
        context
    }
}
