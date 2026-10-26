//! Acknowledgement of the emission sequence an activation owes a turn.
//! The terminal can become readable before the actor hands its closer to
//! the sink. A local output follower waits on this sequence's channel;
//! no durable read depends on a remote node staying alive.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_protocol_types::sync::MutexExt as _;
use tokio::sync::watch;
use {lash_kernel_value::identity::SessionId, lash_vocabulary::identity::TurnId};

type Turn = (SessionId, TurnId);

#[derive(Default)]
pub(super) struct TurnEmissions {
    pending: Mutex<HashMap<Turn, Vec<watch::Receiver<()>>>>,
}

impl TurnEmissions {
    pub(super) fn hold(self: &Arc<Self>, session: &SessionId, run: &TurnId) -> Sequence {
        let turn = (session.clone(), run.clone());
        let (done, receipt) = watch::channel(());
        self.pending
            .lock_recover()
            .entry(turn.clone())
            .or_default()
            .push(receipt.clone());
        Sequence {
            emissions: self.clone(),
            turn,
            receipt,
            _done: done,
        }
    }

    pub(super) async fn handed_off(&self, session: &SessionId, run: &TurnId) {
        // Snapshot only the sequences already claimed before the terminal
        // was read. The channel itself is their identity, including when an
        // old owner is being dropped while another activation takes over.
        let pending = self
            .pending
            .lock_recover()
            .get(&(session.clone(), run.clone()))
            .cloned()
            .unwrap_or_default();
        for mut receipt in pending {
            // The sender closes on acknowledgement or owner loss. Neither
            // path leaves a follower depending on a task that no longer runs.
            let _ = receipt.changed().await;
        }
    }
}

pub(super) struct Sequence {
    emissions: Arc<TurnEmissions>,
    turn: Turn,
    receipt: watch::Receiver<()>,
    _done: watch::Sender<()>,
}

impl Drop for Sequence {
    fn drop(&mut self) {
        let mut pending = self.emissions.pending.lock_recover();
        if let Some(sequences) = pending.get_mut(&self.turn) {
            sequences.retain(|receipt| !receipt.same_channel(&self.receipt));
            if sequences.is_empty() {
                pending.remove(&self.turn);
            }
        }
        // Dropping the sender acknowledges this exact sequence after its
        // removal. Sink callbacks have returned, or the owner was dropped.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt as _;

    /// FIG-7297: cancellation acknowledges only its own sequence, including
    /// two overlapping activations of the same turn during owner loss.
    #[tokio::test]
    async fn dropping_an_emission_owner_releases_only_its_own_sequence() {
        let emissions = Arc::new(TurnEmissions::default());
        let session = SessionId::fixture("emission-owner");
        let run = TurnId::fixture("run");
        let old = emissions.hold(&session, &run);
        let next = emissions.hold(&session, &run);
        {
            let waiting = emissions.handed_off(&session, &run);
            tokio::pin!(waiting);
            assert!(waiting.as_mut().now_or_never().is_none());
            drop(old);
            assert!(waiting.as_mut().now_or_never().is_none());
            // A dropped observer owns no part of the activation's obligation.
        }
        let waiting = emissions.handed_off(&session, &run);
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        drop(next);
        waiting.await;
        assert!(emissions.pending.lock_recover().is_empty());
    }
}
