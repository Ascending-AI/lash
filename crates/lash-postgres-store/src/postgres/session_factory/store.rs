use super::*;

impl PostgresStore {
    pub(crate) fn turn_cancel_closure_owner_binding(
        &self,
    ) -> Option<lash_core_execution::TurnCancelClosureOwnerBinding> {
        let owner = self
            .turn_cancel_closure_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        Some(lash_core_execution::TurnCancelClosureOwnerBinding::new(
            format!("postgres-catalog:{}", self.catalog_id),
            owner,
        ))
    }
}

impl PostgresStore {
    pub(crate) async fn admit_session_inner(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<lash_core_execution::SessionAdmission, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        let meta = SessionMeta {
            owning_process_id: request.owning_process_id.clone(),
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
            pending_observer_intents: request.pending_observer_intents.clone(),
        };
        let created_at_ms = self.clock.timestamp_ms();
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &request.session_id)
            .await?;
        let deleted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
             )",
        )
        .bind(request.session_id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if deleted {
            return Err(StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        let inserted = crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Insert,
            created_at_ms,
            self.fence.fleet(),
        )
        .await?;
        if inserted && request.head == lash_core_execution::SessionCreationHead::Config {
            // The creator's config is baked in with the catalog row, in this
            // transaction (FIG-4099).
            let created_head = lash_core_execution::store::SessionHeadMeta::created(
                &request.session_id,
                request.config.clone(),
                self.fleet_format,
            );
            sqlx::query(session_sql().head.insert_created.sql())
                .bind(request.session_id.as_str())
                .bind(encode_json(&created_head.payload())?)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        } else if !inserted {
            let recorded =
                crate::session_meta::load_recorded_lineage_tx(&mut tx, &request.session_id)
                    .await?
                    .ok_or_else(|| StoreError::SessionBindingNotMaterialized {
                        session_id: request.session_id.clone(),
                    })?;
            lash_core_execution::store_backend_support::guard_rebind_lineage(
                &request.session_id,
                &recorded,
                &request.relation,
            )?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if inserted {
            lash_core_execution::SessionAdmission::Created
        } else {
            lash_core_execution::SessionAdmission::Rebound
        })
    }
}
