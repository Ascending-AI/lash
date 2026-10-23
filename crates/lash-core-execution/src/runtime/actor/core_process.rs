//! Process projection access through the claimed actor coordinator.
use super::*;

impl ActorContext {
    /// A terminal resolution retained by this execution needs no writer or
    /// discovery. Absence is always read from the durable resolution frontier.
    #[doc(hidden)]
    pub async fn retained_resolution(
        &self,
        key: &lash_core_store::resolutions::ResolutionKey,
    ) -> Result<Option<lash_core_store::resolutions::ResolutionRecord>, DurableError> {
        if let Some(owner) = self.inner.process_projection.lock().await?.as_ref()
            && let Some(record) = owner.state()?.resolutions.get(key)
        {
            return Ok(Some(record.clone()));
        }
        let record = self
            .backend()
            .stores()
            .execution_resolutions()
            .load(key)
            .await?;
        if let Some(record) = &record {
            let mut held = self.inner.process_projection.lock().await?;
            if let Some(owner) = held.as_mut() {
                let revision = owner.revision()?;
                let mut next = owner.state()?.clone();
                next.resolutions.insert(key.clone(), record.clone());
                *owner = super::super::owner_state::OwnerState::loaded(
                    self.actor().clone(),
                    self.epoch(),
                    revision,
                    next,
                );
            }
        }
        Ok(record)
    }

    /// Commit first-use discovery through the claim's existing adoption seam.
    #[doc(hidden)]
    pub async fn commit_resolution(
        &self,
        candidate: lash_core_store::resolutions::ResolutionRecord,
    ) -> Result<lash_core_store::resolutions::ResolutionRecord, DurableError> {
        let _serial = self.inner.fresh_updates.lock().await;
        if let Some(record) = self.retained_resolution(&candidate.key).await? {
            return Ok(record);
        }
        let tx = self.begin().await?;
        let store = self.backend().stores().execution_resolutions();
        let winner = std::sync::Mutex::new(None);
        let returned = &winner;
        self.with_obligations(
            tx,
            CommitLabel::RESOLUTION_COMMIT,
            None,
            Some(&winner),
            |tx| async move {
                let owner = lash_durable::resolutions::ResolutionOwner {
                    process_view: tx.process_preimage().cloned(),
                    actor: tx.actor().clone(),
                    epoch: tx.epoch(),
                    expected_revision: tx.revision(),
                };
                let acknowledged = store.commit_acknowledged(&owner, candidate).await?;
                *returned
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(match acknowledged.result {
                        lash_durable::resolutions::CommitResolution::Inserted(record)
                        | lash_durable::resolutions::CommitResolution::Existing(record) => record,
                    });
                Ok(acknowledged.actor)
            },
        )
        .await?;
        winner
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ok_or_else(|| DurableError::ProcessPostimage {
                actor: self.actor().clone(),
                message: "resolution receipt has no winner".into(),
            })
    }

    /// The current acknowledged process view for composition preparation.
    #[doc(hidden)]
    pub async fn acknowledged_process_view(
        &self,
    ) -> Result<lash_durable::domain::ProcessView, DurableError> {
        self.inner
            .process_projection
            .lock()
            .await?
            .as_ref()
            .ok_or_else(|| DurableError::ProcessPostimage {
                actor: self.actor().clone(),
                message: "missing process projection".into(),
            })?
            .state()?
            .view
            .clone()
            .ok_or_else(|| DurableError::ProcessPostimage {
                actor: self.actor().clone(),
                message: "missing process view".into(),
            })
    }

    pub(crate) async fn invalidate_process_projection(&self) {
        if let Ok(mut projection) = self.inner.process_projection.lock().await {
            *projection = None;
        }
    }

    pub(crate) async fn process_revision(
        &self,
    ) -> Result<lash_durable::StateRevision, DurableError> {
        self.inner
            .process_projection
            .lock()
            .await?
            .as_ref()
            .ok_or_else(|| DurableError::ProcessPostimage {
                actor: self.actor().clone(),
                message: "missing process projection".into(),
            })?
            .revision()
    }

    pub(crate) async fn process_continuation(
        &self,
    ) -> Result<super::super::process::continuation::Continuation, DurableError> {
        self.inner
            .process_projection
            .lock()
            .await?
            .as_ref()
            .ok_or_else(|| DurableError::ProcessPostimage {
                actor: self.actor().clone(),
                message: "missing process projection".into(),
            })?
            .state()
            .cloned()
    }

    pub(crate) async fn enrich_process(
        &self,
        revision: lash_durable::StateRevision,
        derived: super::super::process::continuation::Continuation,
    ) -> Result<(), DurableError> {
        let mut held = self.inner.process_projection.lock().await?;
        let Some(owner) = held.as_mut() else {
            return Ok(());
        };
        if owner.revision()? != revision {
            return Ok(());
        }
        let mut next = owner.state()?.clone();
        next.enrich(derived);
        *owner = super::super::owner_state::OwnerState::loaded(
            self.actor().clone(),
            self.epoch(),
            revision,
            next,
        );
        Ok(())
    }

    pub(crate) async fn open_process_projection(
        &self,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
        let mut held = self.inner.process_projection.lock().await?;
        let expected = held
            .as_ref()
            .map(super::super::owner_state::OwnerState::revision)
            .transpose()?;
        let (mut tx, view) = if let Some(expected) = expected {
            self.probe().owner_read(
                self.actor(),
                self.epoch(),
                lash_durable::OwnerRead::ExternalFrontierReads,
                1,
            );
            let (tx, frontier) = self
                .durable()?
                .begin_process_retained(
                    self.actor(),
                    self.epoch(),
                    expected,
                    self.backend()
                        .config()
                        .settings()
                        .graph_reads
                        .owner()
                        .total_bytes,
                )
                .await?;
            let mut next = held
                .as_ref()
                .ok_or_else(|| DurableError::ProcessPostimage {
                    actor: self.actor().clone(),
                    message: "process projection disappeared".into(),
                })?
                .state()?
                .clone();
            next.refresh_frontier(frontier)?;
            let view = next
                .view
                .clone()
                .ok_or_else(|| DurableError::ProcessPostimage {
                    actor: self.actor().clone(),
                    message: "process projection has no view".into(),
                })?;
            *held = Some(super::super::owner_state::OwnerState::loaded(
                self.actor().clone(),
                self.epoch(),
                expected,
                next,
            ));
            (tx, view)
        } else {
            self.probe().owner_read(
                self.actor(),
                self.epoch(),
                lash_durable::OwnerRead::RecoveryLoads,
                1,
            );
            let (tx, view) = self
                .durable()?
                .open_process(self.actor(), self.epoch())
                .await?;
            let next = super::super::process::continuation::Continuation::loaded(view.clone());
            *held = Some(super::super::owner_state::OwnerState::loaded(
                self.actor().clone(),
                self.epoch(),
                tx.revision(),
                next,
            ));
            (tx, view)
        };
        tx.retain_process(view.clone());
        Ok((tx, view))
    }
}
