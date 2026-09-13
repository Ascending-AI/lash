use super::*;

impl PostgresSessionStoreFactory {
    pub(super) fn turn_cancel_closure_owner_binding(
        &self,
    ) -> Option<lash_core::TurnCancelClosureOwnerBinding> {
        use sha2::Digest as _;
        let owner = self
            .turn_cancel_closure_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        let catalog_digest = sha2::Sha256::digest(self.await_event_signing_secret.as_ref());
        Some(lash_core::TurnCancelClosureOwnerBinding::new(
            format!("postgres-catalog:{catalog_digest:x}"),
            owner,
        ))
    }

    pub(super) fn store_for(&self, session_id: SessionId) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            await_event_signing_secret: Arc::clone(&self.await_event_signing_secret),
            clock: Arc::clone(&self.clock),
            session_id,
            turn_cancel_closure_owner: self.turn_cancel_closure_owner_binding(),
            #[cfg(any(test, feature = "testing"))]
            lease_clock_for_testing: self.lease_clock_for_testing.clone(),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl PostgresSessionStoreFactory {
    /// Concrete constructor behind [`SessionStoreFactory::create_store`]; the
    /// gated conformance factory shares it.
    pub(crate) async fn create_session_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<PostgresSessionStore>, StoreError> {
        lash_core::store::validate_session_id(&request.session_id)?;
        let store = self.store_for(request.session_id.clone());
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
            pending_observer_intents: request.pending_observer_intents.clone(),
        };
        let created_at_ms = self.clock.timestamp_ms();
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &request.session_id)
            .await?;
        let deleted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
             )",
        )
        .bind(request.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if deleted {
            return Err(StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Insert,
            created_at_ms,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Arc::new(store))
    }

    /// Concrete reopen behind [`SessionStoreFactory::open_existing_store`];
    /// the gated conformance factory shares it.
    pub(crate) async fn open_existing_session_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<PostgresSessionStore>>, String> {
        let store = self.store_for(request.session_id.clone());
        if store
            .load_session_meta()
            .await
            .map_err(|err| err.to_string())?
            .is_some()
        {
            Ok(Some(Arc::new(store)))
        } else {
            Ok(None)
        }
    }
}
