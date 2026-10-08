use super::*;

#[path = "catalog_reads.rs"]
pub(crate) mod catalog_reads;

#[async_trait::async_trait]
impl lash_core_execution::SessionCatalogStore for SqliteStore {
    async fn admit_session(
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
        let config = request.config.clone();
        self.conn
            .write_flow(move |tx| {
                let fleet_format = tx.fleet();
                let outcome = (|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &meta.session_id)?;
                    let inserted =
                        session_meta::write_session_meta(tx, &meta, created_at_ms, fleet_format)?;
                    if inserted {
                        // The config's plugin namespaces are admitted against
                        // the fleet record's writer ranges before the head
                        // that carries them is written (FIG-4746).
                        tx.admit_plugin_writers(
                            &lash_core_execution::store::plugin_writers::PluginPublication::of_session_config(&config),
                        )
                        .map_err(crate::sqlite_error)?;
                        // The creator's config is baked in with the catalog
                        // row, in this transaction (FIG-4099).
                        let created_head = lash_core_execution::store::SessionHeadMeta::created(
                            &meta.session_id,
                            config.clone(),
                            fleet_format,
                        );
                        let head_json = encode_json(&created_head.payload())?;
                        // The creation revision is an ordinary retained
                        // revision: an empty session forks at it (FIG-4731).
                        crate::revisions::record_revision_conn(
                            tx,
                            &created_head.session_id,
                            0,
                            None,
                            None,
                            &head_json,
                        )?;
                        crate::conn::cached_execute(
                            tx,
                            crate::session_sql::session_sql().head.insert_created.sql(),
                            rusqlite::params![created_head.session_id.as_str()],
                        )
                        .map_err(crate::sqlite_error)?;
                        return Ok(lash_core_execution::SessionAdmission::Created);
                    }
                    let recorded = session_meta::load_recorded_lineage(tx, &meta.session_id)?
                        .ok_or_else(|| StoreError::SessionBindingNotMaterialized {
                            session_id: meta.session_id.clone(),
                        })?;
                    lash_core_execution::store_backend_support::guard_rebind_lineage(
                        &meta.session_id,
                        &recorded,
                        &meta.relation,
                    )?;
                    Ok(lash_core_execution::SessionAdmission::Rebound)
                })();
                Ok(match outcome {
                    Ok(admission) => TxOutcome::Commit(Ok(admission)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn lookup_session(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::SessionLookup, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let session_id = session_id.clone();
        self.read_connection()
            .read(move |conn| {
                let deleted = conn
                    .query_row(
                        crate::session_sql::session_sql()
                            .deleted_sqlite
                            .exists
                            .sql(),
                        params![session_id.as_str()],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if deleted {
                    return Ok(Ok(lash_core_execution::SessionLookup::Deleted));
                }
                let meta = crate::session_meta::load_session_meta_in_tx(conn, Some(&session_id));
                Ok(meta.map(|meta| {
                    meta.map_or(
                        lash_core_execution::SessionLookup::Absent,
                        lash_core_execution::SessionLookup::Live,
                    )
                }))
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionView>, StoreError> {
        let filter = filter.clone();
        self.read_connection()
            .call(move |conn| crate::session_listing::list_session_views(conn, &filter))
            .await
            .map_err(sqlite_error)
    }

    async fn fork_session(
        &self,
        request: &lash_core_execution::ForkSessionRequest,
    ) -> Result<lash_core_execution::ForkSessionReceipt, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        fork_at_in_catalog(
            &self.conn,
            request,
            self.clock.timestamp_ms(),
            self.options.blob_profile,
        )
        .await
    }

    async fn resolve_target(
        &self,
        session_id: &SessionId,
        target: &lash_core_execution::Target,
    ) -> Result<lash_core_execution::RetainedRevision, StoreError> {
        self.resolve_target_in_catalog(session_id, target).await
    }

    async fn revisions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::RetainedRevision>, StoreError> {
        self.revisions_in_catalog(session_id).await
    }

    async fn pin(
        &self,
        session_id: &SessionId,
        target: &lash_core_execution::Target,
    ) -> Result<(), StoreError> {
        self.pin_in_catalog(session_id, target).await
    }

    async fn unpin(
        &self,
        session_id: &SessionId,
        target: &lash_core_execution::Target,
    ) -> Result<(), StoreError> {
        self.unpin_in_catalog(session_id, target).await
    }

    async fn retention(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::Retention, StoreError> {
        self.retention_in_catalog(session_id).await
    }

    async fn set_retention(
        &self,
        session_id: &SessionId,
        retention: lash_core_execution::Retention,
    ) -> Result<(), StoreError> {
        self.set_retention_in_catalog(session_id, retention).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)?;
        delete_session_from_catalog(&self.conn, session_id, self.clock.timestamp_ms()).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::DeploymentStore for SqliteStore {
    async fn artifact_frame_is_retained(
        &self,
        frame: &lash_core_execution::FrameEnvironmentId,
    ) -> Result<bool, StoreError> {
        let frame = frame.clone();
        self.read_connection()
            .read(move |conn| {
                conn.query_row(
                    crate::session_sql::session_sql()
                        .graph_sqlite
                        .artifact_frame_is_retained
                        .sql(),
                    params![frame.session_id().as_str(), frame.frame_node_id().as_str()],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)
    }

    async fn reclaim_retained_evidence(
        &self,
        bound: lash_core_execution::store::RetentionBound,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::store::RetentionReport> {
        let report = crate::retention::reclaim(self, bound)
            .await
            .map_err(|failure| *failure)?;
        Ok(report)
    }
    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash_core_execution::store::UnsettledTurnCounts, StoreError> {
        if !self.location.target().exists() {
            return Ok(lash_core_execution::store::UnsettledTurnCounts::default());
        }
        let conn = self.read_connection();
        let (in_flight, held_by_stalled_close): (i64, i64) = conn
            .read(|conn| {
                conn.query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .family
                        .count_unsettled_turns
                        .sql(),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(lash_core_execution::store::UnsettledTurnCounts {
            in_flight_turns: usize::try_from(in_flight).unwrap_or_default(),
            held_by_stalled_close: usize::try_from(held_by_stalled_close).unwrap_or_default(),
        })
    }
    async fn turns_changed_since(
        &self,
        after: lash_core_execution::store::TurnChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<lash_core_execution::store::TurnChangePage, StoreError> {
        self.read_turn_changes(after, limit).await
    }

    async fn list_control_intents(
        &self,
        after: Option<lash_core_execution::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::store::ControlIntent>, StoreError> {
        let Some(conn) = self.control_ledger().await? else {
            return Ok(Vec::new());
        };
        conn.call(move |conn| {
            let sql = &crate::session_runs::session_runs_sql().intents;
            let mut stmt = conn.prepare_cached(sql.list_after.sql())?;
            let rows = stmt
                .query_map(
                    params![
                        after.map_or(0, |id| id.sequence()) as i64,
                        limit.get() as i64
                    ],
                    crate::session_runs::intent_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .map(crate::session_runs::StoredIntentRow::decode)
                .collect())
        })
        .await
        .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl lash_core_execution::AttachmentRootSet for SqliteStore {
    async fn attachment_root_page(
        &self,
        source: lash_core_execution::attachments::AttachmentRootSource,
        after: Option<&lash_core_execution::AttachmentId>,
    ) -> Result<lash_core_execution::attachments::AttachmentRootPage, StoreError> {
        if !self.location.target().exists() {
            return Err(StoreError::Backend(format!(
                "attachment catalog {} does not exist",
                self.location.target()
            )));
        }
        SqliteStore::attachment_root_page(self, source, after).await
    }
    async fn list_condemnations(
        &self,
    ) -> Result<
        Vec<lash_core_execution::AttachmentCondemnationRecord>,
        lash_core_execution::StoreError,
    > {
        let store = self;
        store.list_attachment_condemnations().await
    }
    async fn has_live_attachment_ref(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<bool, lash_core_execution::StoreError> {
        self.has_attachment_root(id).await
    }
    fn fence(&self) -> lash_core_execution::AttachmentGcFence {
        lash_core_execution::AttachmentGcFence::Fenced
    }
    async fn begin_attachment_sweep(
        &self,
    ) -> Result<lash_core_execution::AttachmentSweepGeneration, lash_core_execution::StoreError>
    {
        SqliteStore::begin_attachment_sweep(self).await
    }
    async fn adopt_attachment_condemnations(
        &self,
        generation: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentCondemnationAdoption, lash_core_execution::StoreError>
    {
        SqliteStore::adopt_attachment_condemnations(self, generation).await
    }
    async fn condemn_attachment(
        &self,
        id: &lash_core_execution::AttachmentId,
        generation: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentCondemnation, lash_core_execution::StoreError> {
        SqliteStore::condemn_attachment(self, id, generation).await
    }
    async fn arm_attachment_delete(
        &self,
        id: &lash_core_execution::AttachmentId,
        generation: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentDeleteArming, lash_core_execution::StoreError> {
        SqliteStore::arm_attachment_delete(self, id, generation).await
    }
    async fn settle_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
        generation: &lash_core_execution::AttachmentSweepGeneration,
        settlement: lash_core_execution::AttachmentCondemnationSettlement,
    ) -> Result<lash_core_execution::AttachmentSettlementOutcome, lash_core_execution::StoreError>
    {
        SqliteStore::settle_attachment_condemnation(self, id, generation, settlement).await
    }
    async fn recover_abandoned_attachment_write(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self;
        store.recover_abandoned_attachment_write(id).await
    }
}

pub(crate) fn retained_artifact_refs(checkpoint: &SessionCheckpoint) -> Vec<RetainedArtifactRef> {
    checkpoint
        .components
        .values()
        .map(|descriptor| RetainedArtifactRef {
            blob_ref: descriptor.blob_ref.clone(),
            kind: PersistedArtifactKind::CheckpointComponent,
        })
        .collect()
}
