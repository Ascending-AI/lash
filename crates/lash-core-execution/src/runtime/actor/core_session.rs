//! Session projections are loaded and recovered as one fenced basis.
use super::*;
use crate::runtime::actor::owner_state;

impl ActorContext {
    /// The acknowledged session projection. A poisoned projection is never
    /// exposed as an absent cache entry: recovery must explicitly replace it.
    #[doc(hidden)]
    pub async fn session_projection(
        &self,
    ) -> Result<Option<owner_state::SessionProjection>, DurableError> {
        lash_core_ids::profile_history!(OwnerRetention, {
            if !self.inner.claim_live.load(Ordering::Acquire) {
                return Err(DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: "session claim has ended".into(),
                }));
            }
            self.inner
                .session_projection
                .lock()
                .await?
                .as_ref()
                .map(|owner| owner.state().cloned())
                .transpose()
        })
    }

    /// The compatible plugin assembly belongs to the acknowledged claim,
    /// so releasing or poisoning its projection drops the assembly as well.
    #[doc(hidden)]
    pub async fn retain_session_composition(
        &self,
        composition: Arc<crate::plugin::PluginHost>,
    ) -> Result<(), DurableError> {
        lash_core_ids::profile_history!(OwnerRetention, {
            let mut projection = self.inner.session_projection.lock().await?;
            let owner = projection.as_mut().ok_or_else(|| {
                DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: "session claim has no projection".into(),
                })
            })?;
            let revision = owner.revision()?;
            let mut next = owner.state()?.clone();
            next.composition = Some(composition);
            // This is an immutable assembly, not a durable state transition.
            *owner =
                owner_state::OwnerState::loaded(self.actor().clone(), self.epoch(), revision, next);
            Ok(())
        })
    }

    /// Install a coherent cold load at its unchanged transaction basis.
    #[doc(hidden)]
    pub async fn load_session_projection(
        &self,
        tx: &ActorTx,
        projection: owner_state::SessionProjection,
    ) -> Result<(), DurableError> {
        lash_core_ids::profile_history!(OwnerRetention, {
            if tx.actor() != self.actor() || tx.epoch() != self.epoch() {
                return Err(DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Corrupt,
                    message: "session hydration names another claim".into(),
                }));
            }
            if let Some(obligations) = &self.inner.obligations {
                obligations
                    .hydrated(
                        &projection,
                        self.durable_reads()?,
                        self.backend().config().settings().graph_reads.execution(),
                        self.backend().config().settings().graph_reads.owner(),
                    )
                    .await?;
            }
            // Include obligation hydration in the same revision bracket as the head.
            let checked = self
                .durable()?
                .begin_retained(self.actor(), self.epoch(), tx.revision(), 0)
                .await?;
            if checked.revision() != tx.revision() {
                return Err(DurableError::RevisionConflict {
                    actor: self.actor().clone(),
                    epoch: self.epoch(),
                    expected: tx.revision(),
                    actual: checked.revision(),
                });
            }
            *self.inner.session_projection.lock().await? = Some(owner_state::OwnerState::loaded(
                self.actor().clone(),
                self.epoch(),
                tx.revision(),
                projection,
            ));
            Ok(())
        })
    }

    /// Recovery discards head, turn and every revision-derived draft together.
    #[doc(hidden)]
    pub async fn invalidate_session_projection(&self) {
        lash_core_ids::profile_history!(OwnerRetention, {
            if let Ok(mut projection) = self.inner.session_projection.lock().await {
                *projection = None;
            }
            if let Some(obligations) = &self.inner.obligations {
                obligations.invalidate().await;
            }
        })
    }

    /// Re-establish the complete acknowledged basis under the projection
    /// lock. A moved revision reloads head and turn together; no draft is
    /// ever granted the new revision of unrelated facts.
    pub(super) async fn recovered_session(
        &self,
        mut acknowledged: owner_state::SessionProjection,
        basis: lash_durable::StateRevision,
    ) -> Result<(lash_durable::StateRevision, owner_state::SessionProjection), DurableError> {
        use lash_core_ids::operational_metrics::sql::{OwnerReadClass, classify_owner_reads};
        use lash_core_store::store::{
            RuntimeStore, SessionStore, WindowSelector, load_session_window_state,
        };
        let tx = self.durable()?.begin(self.actor(), self.epoch()).await?;
        if tx.revision() != basis {
            acknowledged = classify_owner_reads(OwnerReadClass::Recovery, async {
                let id = acknowledged.head.session_id.clone();
                let failed = |error: lash_core_store::store::StoreError| {
                    DurableError::session_commit(id.clone(), &error)
                };
                let runtime: Arc<dyn RuntimeStore> = self.backend().session_store_factory();
                let store = SessionStore::new(runtime, id.clone()).map_err(failed)?;
                let mut loaded = load_session_window_state(&store, WindowSelector::Current)
                    .await
                    .map_err(failed)?
                    .ok_or_else(|| {
                        failed(lash_core_store::store::StoreError::SessionNotFound {
                            session_id: id.clone(),
                        })
                    })?;
                loaded.state.session_graph.detach_owner_prompt_rendering();
                let turn = self.durable_reads()?.turn(&id).await?;
                Ok::<_, DurableError>(owner_state::SessionProjection {
                    prompt: Default::default(),
                    composition: None,
                    phase: None,
                    head: Arc::new(loaded.state),
                    fleet: store.fleet_format(),
                    turn,
                })
            })
            .await?;
        } else if let Some(turn) = &mut acknowledged.turn {
            turn.cancel = tx.turn_cancel().cloned();
        }
        if let Some(obligations) = &self.inner.obligations {
            let limits = self.backend().config().settings().graph_reads;
            obligations
                .hydrated(
                    &acknowledged,
                    self.durable_reads()?,
                    limits.execution(),
                    limits.owner(),
                )
                .await?;
        }
        let checked = self
            .durable()?
            .begin_retained(self.actor(), self.epoch(), tx.revision(), 0)
            .await?;
        if checked.revision() != tx.revision() {
            return Err(DurableError::RevisionConflict {
                actor: self.actor().clone(),
                epoch: self.epoch(),
                expected: tx.revision(),
                actual: checked.revision(),
            });
        }
        Ok((tx.revision(), acknowledged))
    }
}
