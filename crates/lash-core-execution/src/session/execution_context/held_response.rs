//! The model response a turn holds for its cell's first save (FIG-6000).

use std::sync::{Arc, Mutex};

use lash_durable::domain::TurnCancelRequest;
use lash_durable::{ActorCommit, ActorTx, CommitLabel, DurableError};
use lash_sansio::sync::MutexExt as _;
use tokio_util::sync::CancellationToken;

/// The model response that asked for a cell, held uncommitted by its turn
/// (FIG-6000): the writes `model.done` would commit (the turn's `Tools`
/// checkpoint that re-delivers the cell, its run changes and the model
/// call's settlement), handed to the cell's executor so its first save
/// commits them in the same transaction, as
/// `model.done+cell.snapshot+admit`.
///
/// The hold is bounded: it lasts while a worker is ready and the machine
/// runs its first slice. A cell that must wait for a worker, or whose first
/// stretch runs past one slice, commits the writes on their own as
/// `model.done` at that point and runs on. A cell that answers without
/// saving leaves them to the commit that consumes its answer.
///
/// Nothing of the cell reaches its host's stream or trace before the
/// response commits: what the cell publishes is kept here
/// ([`after_commit`](Self::after_commit)) and published, in order, once a
/// commit holds the response. A node lost before that loses the response:
/// its successor sends the model call again, which may answer with another
/// cell, and no observer saw the first.
///
/// Clones share the hold.
#[derive(Clone)]
pub struct HeldResponse {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    /// Whether the open read of a save says the turn stops at once.
    stops_at: fn(&ActorTx) -> bool,
    /// Whether a cancel a commit read under its fence stops the turn at once.
    stops_in: fn(&TurnCancelRequest) -> bool,
    /// Fired once the turn's cancel stopped the cell at its first save.
    stopped: CancellationToken,
}

struct State {
    /// The writes, while no commit holds them.
    writes: Option<ActorTx>,
    /// How the response was committed, once it was.
    committed: Option<CommitLabel>,
    /// What the cell published before the response committed, in order.
    deferred: Vec<Box<dyn FnOnce() + Send>>,
}

impl std::fmt::Debug for HeldResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.state.lock_recover();
        f.debug_struct("HeldResponse")
            .field("held", &state.writes.is_some())
            .field("committed", &state.committed)
            .field("deferred", &state.deferred.len())
            .finish_non_exhaustive()
    }
}

impl HeldResponse {
    /// The response `writes` hold, uncommitted. `stops_at` answers whether
    /// a save's open read stops the turn at once; `stops_in` whether a
    /// cancel its commit read does.
    #[must_use]
    pub fn holding(
        writes: ActorTx,
        stops_at: fn(&ActorTx) -> bool,
        stops_in: fn(&TurnCancelRequest) -> bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    writes: Some(writes),
                    committed: None,
                    deferred: Vec::new(),
                }),
                stops_at,
                stops_in,
                stopped: CancellationToken::new(),
            }),
        }
    }

    /// The writes a save carries, while no commit holds them.
    #[must_use]
    pub fn writes(&self) -> Option<ActorTx> {
        self.inner.state.lock_recover().writes.clone()
    }

    /// Whether the response is still held: no commit holds it yet.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.inner.state.lock_recover().writes.is_some()
    }

    /// The label of the commit that holds the response, once one does.
    #[must_use]
    pub fn committed_as(&self) -> Option<CommitLabel> {
        self.inner.state.lock_recover().committed
    }

    /// Whether `tx`, a save's open read, says the turn stops at once: the
    /// save admits nothing and commits nothing.
    #[must_use]
    pub fn stops_at(&self, tx: &ActorTx) -> bool {
        (self.inner.stops_at)(tx)
    }

    /// Whether the cancel `committed` read under its fence stops the turn
    /// at once: the save committed, and nothing it admitted may start.
    #[must_use]
    pub fn stops_in(&self, committed: &ActorCommit) -> bool {
        committed
            .turn_cancel
            .as_ref()
            .is_some_and(|request| (self.inner.stops_in)(request))
    }

    /// A commit labelled `label` that holds the response was acknowledged:
    /// what the cell published since is published now, in order.
    pub fn committed(&self, label: CommitLabel) {
        let deferred = {
            let mut state = self.inner.state.lock_recover();
            if state.committed.is_some() {
                return;
            }
            state.writes = None;
            state.committed = Some(label);
            std::mem::take(&mut state.deferred)
        };
        for publish in deferred {
            publish();
        }
    }

    /// Commit the response on its own as `model.done` on `cx`, the turn's
    /// actor: the cell is about to wait, or to run past its first slice.
    /// Nothing is committed when no commit is owed.
    ///
    /// # Errors
    ///
    /// The store's refusal of the commit; the response stays held.
    pub async fn commit_alone(&self, cx: &crate::ActorContext) -> Result<(), DurableError> {
        let Some(writes) = self.writes() else {
            return Ok(());
        };
        cx.commit(writes, CommitLabel::MODEL_DONE).await?;
        self.committed(CommitLabel::MODEL_DONE);
        Ok(())
    }

    /// The turn's cancel stopped the cell at its first save: the turn stops
    /// where it would have before the cell started.
    pub fn stop(&self) {
        self.inner.stopped.cancel();
    }

    /// Resolves once the turn's cancel stopped the cell at its first save.
    pub async fn stopped(&self) {
        self.inner.stopped.cancelled().await;
    }

    /// Publish what `publish` publishes once a commit holds the response:
    /// now when one does already.
    pub fn after_commit(&self, publish: Box<dyn FnOnce() + Send>) {
        {
            let mut state = self.inner.state.lock_recover();
            if state.committed.is_none() {
                state.deferred.push(publish);
                return;
            }
        }
        publish();
    }
}
