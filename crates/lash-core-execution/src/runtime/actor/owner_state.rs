//! A claim's acknowledged projection and its single-flight adoption boundary.
//!
//! Taking the projection before calling the writer is the cancellation guard:
//! no future, panic or failed adoption can leave the old basis usable.

use std::future::Future;

use lash_durable::{
    ActorCommit, ActorKey, ActorState, ActorTx, DurableError, Epoch, StateRevision, StoreFailure,
    StoreFailureKind,
};

/// The sole acknowledged projection for one actor claim.
pub struct OwnerState<S> {
    actor: ActorKey,
    epoch: Epoch,
    revision: StateRevision,
    state: Option<S>,
}

/// A privately prepared replacement, tied to the authority that prepared it.
pub struct OwnerDraft<S> {
    tx: ActorTx,
    next: S,
    observed: Option<(
        lash_durable::CommitLabel,
        std::sync::Arc<dyn lash_durable::DurableProbe>,
    )>,
}

impl<S> OwnerDraft<S> {
    /// Attribute the adoption cuts to the transition being acknowledged.
    #[must_use]
    pub fn observed(
        mut self,
        label: lash_durable::CommitLabel,
        probe: std::sync::Arc<dyn lash_durable::DurableProbe>,
    ) -> Self {
        self.observed = Some((label, probe));
        self
    }
}

impl<S> OwnerState<S> {
    /// Install a coherently loaded projection under its original authority.
    #[must_use]
    pub fn loaded(actor: ActorKey, epoch: Epoch, revision: StateRevision, state: S) -> Self {
        Self {
            actor,
            epoch,
            revision,
            state: Some(state),
        }
    }

    /// The acknowledged projection, or a refusal requiring coherent reload.
    ///
    /// # Errors
    /// The projection was poisoned or released.
    pub fn state(&self) -> Result<&S, DurableError> {
        self.state
            .as_ref()
            .ok_or_else(|| invalid("owner projection requires coherent reload"))
    }

    /// The revision of the usable projection.
    ///
    /// # Errors
    /// The projection was poisoned or released.
    pub fn revision(&self) -> Result<StateRevision, DurableError> {
        self.state()?;
        Ok(self.revision)
    }

    fn check(&self, tx: &ActorTx) -> Result<(), DurableError> {
        self.state()?;
        if tx.actor() != &self.actor {
            return Err(invalid("owner draft names another actor"));
        }
        if tx.epoch() != self.epoch {
            return Err(DurableError::OwnershipLost(lash_durable::Fenced {
                actor: self.actor.clone(),
                held: tx.epoch(),
                current: Some(self.epoch),
            }));
        }
        if tx.revision() != self.revision {
            return Err(DurableError::RevisionConflict {
                actor: self.actor.clone(),
                epoch: self.epoch,
                expected: tx.revision(),
                actual: self.revision,
            });
        }
        Ok(())
    }

    /// Bind a prepared candidate to its unchanged transaction basis.
    ///
    /// # Errors
    /// The projection is unusable, or the supplied transaction has another basis.
    pub fn draft(&self, tx: ActorTx, next: S) -> Result<OwnerDraft<S>, DurableError> {
        self.check(&tx)?;
        Ok(OwnerDraft {
            tx,
            next,
            observed: None,
        })
    }

    /// Commit and adopt one replacement. A failed, cancelled or panicking
    /// operation invalidates the entire projection before control escapes.
    ///
    /// # Errors
    /// A stale draft, failed write, invalid acknowledgement or failed adoption.
    pub async fn commit<F, Fut, A>(
        &mut self,
        draft: OwnerDraft<S>,
        write: F,
        adopt: A,
    ) -> Result<ActorCommit, DurableError>
    where
        F: FnOnce(ActorTx) -> Fut,
        Fut: Future<Output = Result<ActorCommit, DurableError>>,
        A: FnOnce(S, &ActorCommit) -> Result<S, DurableError>,
    {
        let checked = self.check(&draft.tx);
        // Invalidate before the first await *and* before any user callback.
        self.poison();
        checked?;
        let expected = self
            .revision
            .0
            .checked_add(1)
            .ok_or_else(|| invalid("owner revision overflow"))?;
        let receipt = write(draft.tx).await?;
        if receipt.revision != StateRevision(expected) {
            return Err(invalid("owner acknowledgement has an unexpected revision"));
        }
        if let Some((label, probe)) = &draft.observed {
            probe.owner_cut(
                &self.actor,
                self.epoch,
                *label,
                lash_durable::OwnerCut::Acknowledged,
            )?;
        }
        let next = adopt(draft.next, &receipt)?;
        if let Some((label, probe)) = &draft.observed {
            probe.owner_cut(
                &self.actor,
                self.epoch,
                *label,
                lash_durable::OwnerCut::Adopted,
            )?;
        }
        self.revision = receipt.revision;
        if receipt.state == ActorState::Owned {
            self.state = Some(next);
        }
        Ok(receipt)
    }

    /// Drop every retained fact. Only a new coherent load can restore use.
    pub fn poison(&mut self) {
        self.state = None;
    }

    /// A released claim never retains a usable projection.
    pub fn release(&mut self) {
        self.poison();
    }
}

fn invalid(message: &str) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: message.to_owned(),
    })
}

/// A session's acknowledged head and unfinished turn. Immutable head nodes
/// are shared with readers and private drafts; only this projection advances.
#[derive(Clone)]
pub struct SessionProjection {
    pub prompt: lash_core_store::prompt_sections::OwnerPromptState,

    /// The compatible immutable plugin assembly for this claim.
    pub composition: Option<std::sync::Arc<crate::plugin::PluginHost>>,
    pub fleet: lash_core_store::store::FleetFormat,
    pub head: std::sync::Arc<lash_core_store::session_state::RuntimeSessionState>,
    pub turn: Option<lash_durable::domain::TurnRow>,
    /// The acknowledged phase's trace surface; absence requires cold recovery.
    pub phase_surface: Option<(
        lash_core_store::checkpoint_graph::NodeId,
        Option<std::sync::Arc<lash_core_store::tool_surface::RecordedSurface>>,
    )>,
}

impl SessionProjection {
    pub(super) fn prepare(&mut self, tx: &ActorTx) -> Result<(), DurableError> {
        use lash_durable::DomainWrite;
        use lash_durable::domain::{ModelCall, TurnRow, TurnWrite, UnfinishedPhase};
        let session = &self.head.session_id;
        let mut phase_surface = tx.prepared_phase_surface();
        for write in tx.domain() {
            let DomainWrite::Turn(write) = write else {
                continue;
            };
            match write {
                TurnWrite::Admit {
                    session: target,
                    run,
                    admission,
                    turn_deadline,
                } if target == session => {
                    self.phase_surface = None;
                    self.turn = Some(TurnRow {
                        session: target.clone(),
                        run: run.clone(),
                        admission: admission.clone(),
                        phase: UnfinishedPhase::Admitted,
                        working: None,
                        iteration: 0,
                        model_calls: 0,
                        tool_calls: 0,
                        turn_deadline: *turn_deadline,
                        written_epoch: tx.epoch(),
                        cancel: None,
                    });
                }
                TurnWrite::Terminal {
                    session: target,
                    run,
                    ..
                } if target == session => {
                    if self.turn.as_ref().is_none_or(|turn| turn.run != *run) {
                        return Err(invalid("terminal postimage has no matching open turn"));
                    }
                    self.turn = None;
                    self.phase_surface = None;
                }
                TurnWrite::Advance {
                    session: target,
                    run,
                    phase,
                    iteration,
                } if target == session => {
                    let turn = self
                        .turn
                        .as_mut()
                        .filter(|turn| turn.run == *run)
                        .ok_or_else(|| invalid("phase postimage has no matching open turn"))?;
                    turn.phase = phase.clone().into();
                    turn.iteration = *iteration;
                    if let Some(pin) = phase.model() {
                        turn.model_calls = pin.call;
                    }
                    turn.written_epoch = tx.epoch();
                    // A held model.done can advance the phase and then end
                    // the turn in this same transaction. Adopt its surface
                    // at the root it names, before later writes consume it.
                    self.phase_surface = phase_surface
                        .filter(|(root, _)| turn.phase.checkpoint() == Some(*root))
                        .map(|(root, surface)| {
                            phase_surface = None;
                            (*root, surface.clone())
                        });
                }
                TurnWrite::Answer {
                    session: target,
                    run,
                    answer,
                } if target == session => {
                    let turn = self
                        .turn
                        .as_mut()
                        .filter(|turn| turn.run == *run)
                        .ok_or_else(|| invalid("answer postimage has no matching open turn"))?;
                    let UnfinishedPhase::Model { call, .. } = &mut turn.phase else {
                        return Err(invalid("answer postimage has no model phase"));
                    };
                    *call = ModelCall::Answered { receipt: *answer };
                    turn.written_epoch = tx.epoch();
                }
                TurnWrite::Working {
                    session: target,
                    run,
                    working,
                    ..
                } if target == session => {
                    let turn = self
                        .turn
                        .as_mut()
                        .filter(|turn| turn.run == *run)
                        .ok_or_else(|| invalid("working postimage has no matching open turn"))?;
                    turn.working = *working;
                    turn.written_epoch = tx.epoch();
                }
                TurnWrite::AddToolCalls {
                    session: target,
                    run,
                    calls,
                } if target == session => {
                    if let Some(turn) = self.turn.as_mut().filter(|turn| turn.run == *run) {
                        turn.tool_calls = turn
                            .tool_calls
                            .checked_add(*calls)
                            .ok_or_else(|| invalid("tool admission count overflow"))?;
                    }
                }
                _ => {}
            }
        }
        if let Some((root, surface)) = phase_surface {
            if self.turn.as_ref().and_then(|turn| turn.phase.checkpoint()) != Some(*root) {
                return Err(invalid(
                    "phase surface postimage has no matching phase root",
                ));
            }
            self.phase_surface = Some((*root, surface.clone()));
        }
        Ok(())
    }

    pub(super) fn acknowledge(
        mut self,
        postimage: Option<lash_core_store::session_state::RuntimeSessionState>,
        receipt: &ActorCommit,
        prompt: Option<lash_core_store::prompt_sections::OwnerPromptState>,
    ) -> Result<Self, DurableError> {
        if !receipt.process_views.is_empty() {
            return Err(invalid("session acknowledgement contains a process view"));
        }
        let heads: Vec<_> = receipt
            .session_heads
            .iter()
            .filter(|head| head.session == self.head.session_id)
            .collect();
        let mut frame_changed = false;
        match (postimage, heads.as_slice()) {
            (Some(mut state), [committed]) => {
                state
                    .adopt_retained_commit_result(&committed.receipt)
                    .map_err(|error| invalid(&error.to_string()))?;
                frame_changed = state.current_frame_node_id != self.head.current_frame_node_id;
                if frame_changed {
                    self.prompt.clear();
                }
                self.head = std::sync::Arc::new(state);
            }
            (None, []) => {}
            _ => {
                return Err(invalid(
                    "session acknowledgement has no matching complete postimage",
                ));
            }
        }
        if let Some(turn) = &mut self.turn {
            turn.cancel = receipt.turn_cancel.clone();
        }
        if let Some(prompt) = prompt.filter(|_| !frame_changed) {
            self.prompt = prompt;
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt as _;

    fn tx(revision: i64) -> ActorTx {
        ActorTx::opened(lash_durable::OpenedActor {
            actor: ActorKey::session("postimage-law").unwrap(),
            epoch: Epoch(1),
            revision: StateRevision(revision),
            acked: lash_durable::MailSeq(0),
            seen: lash_durable::MailSeq(0),
            mail: Vec::new(),
            at: lash_durable::DurableInstant(0),
            turn_cancel: None,
        })
    }
    fn owner() -> OwnerState<u32> {
        OwnerState::loaded(tx(0).actor().clone(), Epoch(1), StateRevision(0), 0)
    }
    fn receipt(revision: i64, state: ActorState) -> ActorCommit {
        ActorCommit {
            process_views: Vec::new(),
            revision: StateRevision(revision),
            state,
            session_heads: Vec::new(),
            root_leases: Vec::new(),
            turn_cancel: None,
        }
    }

    /// LO-POSTIMAGE: no interrupted or refused commit can expose its old
    /// acknowledged basis, and two drafts prepared at one revision cannot win.
    #[tokio::test]
    async fn lo_postimage_invalidates_every_interrupted_adoption() {
        let mut owner = owner();
        let draft = owner.draft(tx(0), 1).unwrap();
        let stale = owner.draft(tx(0), 2).unwrap();
        owner
            .commit(
                draft,
                |_| async { Ok(receipt(1, ActorState::Owned)) },
                |next, _| Ok(next),
            )
            .await
            .unwrap();
        assert_eq!(*owner.state().unwrap(), 1);
        assert!(matches!(
            owner
                .commit(
                    stale,
                    |_| async { panic!("stale draft reached writer") },
                    |next, _| Ok(next)
                )
                .await,
            Err(DurableError::RevisionConflict {
                expected: StateRevision(0),
                actual: StateRevision(1),
                ..
            })
        ));
        assert!(owner.state().is_err());

        let mut cancelled = super::tests::owner();
        let draft = cancelled.draft(tx(0), 1).unwrap();
        let mut commit =
            Box::pin(cancelled.commit(draft, |_| std::future::pending(), |next, _| Ok(next)));
        assert!(futures_util::poll!(commit.as_mut()).is_pending());
        drop(commit);
        assert!(cancelled.state().is_err());

        for cut in 0..4 {
            let mut current = super::tests::owner();
            let draft = current.draft(tx(0), 1).unwrap();
            let result = std::panic::AssertUnwindSafe(current.commit(
                draft,
                |_| async move {
                    if cut == 0 {
                        return Err(invalid("writer failed"));
                    }
                    Ok(receipt(if cut == 1 { 2 } else { 1 }, ActorState::Owned))
                },
                |next, _| {
                    if cut == 2 {
                        return Err(invalid("adoption failed"));
                    }
                    if cut == 3 {
                        panic!("adoption panicked");
                    }
                    Ok(next)
                },
            ))
            .catch_unwind()
            .await;
            assert!(result.is_err() || result.unwrap().is_err());
            assert!(current.state().is_err(), "cut {cut} retained a basis");
        }
        let mut released = super::tests::owner();
        let draft = released.draft(tx(0), 1).unwrap();
        released
            .commit(
                draft,
                |_| async { Ok(receipt(1, ActorState::Idle)) },
                |next, _| Ok(next),
            )
            .await
            .unwrap();
        assert!(released.state().is_err());
    }
}
