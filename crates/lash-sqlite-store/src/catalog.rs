use super::*;

#[path = "factory_reads.rs"]
mod factory_reads;

impl SqliteStore {
    pub(crate) async fn resume_artifact_owner_retirements(
        &self,
    ) -> Result<(), lash_core_execution::StoreError> {
        let effect_host = self
            .effect_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let artifact_stores = self
            .artifact_stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let (Some(effect_host), Some((process_env_store, process_engines))) =
            (effect_host, artifact_stores)
        else {
            return Ok(());
        };
        let scopes = effect_host
            .pending_artifact_owner_retirements()
            .await
            .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
        for scope in scopes {
            let owner = lash_core_execution::ArtifactOwner::execution(scope.clone());
            process_env_store
                .retire_process_execution_env_owner(&owner)
                .await
                .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
            process_engines
                .retire_artifact_owner(&owner)
                .await
                .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
            effect_host
                .complete_artifact_owner_retirement(&scope)
                .await
                .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
        }
        Ok(())
    }
}

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
        let fleet_format = self.fleet_format();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &meta.session_id)?;
                    let inserted = session_meta::write_session_meta(
                        tx,
                        &meta,
                        session_meta::SessionMetaWrite::Insert,
                        created_at_ms,
                        fleet_format,
                    )?;
                    if inserted {
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
            .call(move |conn| {
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
                Ok(
                    crate::session_meta::load_session_meta(conn, Some(&session_id)).map(|meta| {
                        meta.map_or(
                            lash_core_execution::SessionLookup::Absent,
                            lash_core_execution::SessionLookup::Live,
                        )
                    }),
                )
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, StoreError> {
        let filter = filter.clone();
        self.read_connection()
            .call(move |conn| crate::session_listing::list_session_summaries(conn, &filter))
            .await
            .map_err(sqlite_error)
    }

    async fn fork_session(
        &self,
        request: &lash_core_execution::ForkSessionRequest,
    ) -> Result<lash_core_execution::ForkSessionReceipt, StoreError> {
        fork_at_in_catalog(
            &self.location,
            request,
            self.clock.timestamp_ms(),
            self.options.connection_policy,
        )
        .await
    }

    async fn pin(
        &self,
        node_id: &lash_core_execution::NodeId,
    ) -> Result<lash_core_execution::ForkPoint, StoreError> {
        pin_in_catalog(
            &self.location,
            node_id.as_str(),
            self.options.connection_policy,
        )
        .await
    }

    async fn unpin(&self, node_id: &lash_core_execution::NodeId) -> Result<(), StoreError> {
        unpin_in_catalog(
            &self.location,
            node_id.as_str(),
            self.options.connection_policy,
        )
        .await
    }

    async fn fork_points(&self) -> Result<Vec<lash_core_execution::ForkPoint>, StoreError> {
        fork_points_in_catalog(&self.location, self.options.connection_policy).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)?;
        let report = delete_session_from_catalog(
            &self.location,
            session_id,
            self.options.connection_policy,
            self.clock.timestamp_ms(),
        )
        .await?;
        if let Some(process_registry) = self.process_registry.as_ref() {
            delete_wake_allocation_floors_from_process_registry(
                process_registry,
                session_id,
                self.options.connection_policy,
            )
            .await
            .map_err(|message| {
                lash_core_execution::MaintenanceFailure::failed(
                    StoreError::Backend(message),
                    report.clone(),
                )
            })?;
        }
        Ok(report)
    }
}

#[async_trait::async_trait]
impl lash_core_execution::DeploymentStore for SqliteStore {
    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core_execution::EffectHost>) {
        let catalog = self.location.target().canonical_name();
        *self
            .turn_cancel_closure_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(lash_core_execution::TurnCancelClosureOwnerBinding::new(
                format!("sqlite-catalog:{catalog}"),
                Arc::clone(effect_host),
            ));
        *self
            .effect_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(effect_host));
    }
    async fn reclaim_retained_evidence(
        &self,
        bound: lash_core_execution::store::RetentionBound,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::store::RetentionReport> {
        let report = crate::retention::reclaim(self, bound)
            .await
            .map_err(|failure| *failure)?;
        if let Err(error) = self.resume_artifact_owner_retirements().await {
            return Err(lash_core_execution::MaintenanceFailure::failed(
                error, report,
            ));
        }
        Ok(report)
    }
    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash_core_execution::store::UnsettledTurnCounts, StoreError> {
        if !self.location.target().exists() {
            return Ok(lash_core_execution::store::UnsettledTurnCounts::default());
        }
        let conn = self.read_connection();
        conn.read(|conn| {
            let (parked, oldest_since_ms, in_flight, held_by_stalled_close): (
                i64,
                Option<i64>,
                i64,
                i64,
            ) = conn
                .query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .family
                        .count_unsettled_turns
                        .sql(),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
            let mut statement = conn
                .prepare(
                    crate::turn_ingress::turn_ingress_sql()
                        .family
                        .count_parks_by_reason
                        .sql(),
                )
                .map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
            let mut parked_by_reason = std::collections::BTreeMap::new();
            for row in rows {
                let (code, count) = row.map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
                let Some(code) = lash_core_execution::store::ParkReasonCode::from_code(&code)
                else {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        StoreError::StoredDataCorrupt {
                            record_kind: "TurnPark",
                            message: format!("stored park reason code `{code}` is unknown"),
                        },
                    )));
                };
                parked_by_reason.insert(code, usize::try_from(count).unwrap_or_default());
            }
            let retired_by_executable_generation =
                crate::turn_ingress::count_retired_parks_by_executable_generation(conn)?;
            Ok(lash_core_execution::store::UnsettledTurnCounts {
                parked_turns: usize::try_from(parked).unwrap_or_default(),
                in_flight_turns: usize::try_from(in_flight).unwrap_or_default(),
                held_by_stalled_close: usize::try_from(held_by_stalled_close).unwrap_or_default(),
                oldest_parked_since_ms: oldest_since_ms
                    .map(|ms| u64::try_from(ms).unwrap_or_default()),
                parked_by_reason,
                retired_by_executable_generation,
            })
        })
        .await
        .map_err(sqlite_error)
    }
    async fn list_turn_parks(
        &self,
        query: &lash_core_execution::store::TurnParkQuery,
    ) -> Result<Vec<lash_core_execution::store::TurnPark>, StoreError> {
        if !self.location.target().exists() {
            return Ok(Vec::new());
        }
        let conn = self.read_connection();
        let limit = i64::try_from(query.limit.get()).unwrap_or(i64::MAX);
        let session = query.session.as_ref().map(|id| id.as_str().to_string());
        let at_or_before = query
            .parked_at_or_before_ms
            .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX));
        let (after_since, after_session) = match query.after.as_ref() {
            Some((since_ms, session_id)) => (
                Some(i64::try_from(*since_ms).unwrap_or(i64::MAX)),
                Some(session_id.as_str().to_string()),
            ),
            None => (None, None),
        };
        let reasons = query
            .reasons
            .as_ref()
            .filter(|reasons| !reasons.is_empty())
            .map(|reasons| {
                serde_json::to_string(&reasons.iter().map(|code| code.as_str()).collect::<Vec<_>>())
                    .map_err(|error| StoreError::Backend(error.to_string()))
            })
            .transpose()?;
        let rows = conn
            .call(move |conn| {
                let mut statement = conn.prepare_cached(
                    crate::turn_ingress::turn_ingress_sql()
                        .turn_parks_sqlite
                        .list
                        .sql(),
                )?;
                let rows = statement.query_map(
                    params![
                        limit,
                        session,
                        at_or_before,
                        after_since,
                        after_session,
                        reasons
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, Option<String>>(8)?,
                            row.get::<_, Option<i64>>(9)?,
                            row.get::<_, Option<String>>(10)?,
                        ))
                    },
                )?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(
                |(
                    session_id,
                    turn_id,
                    park_id,
                    reason_code,
                    reason_json,
                    since_ms,
                    last_refused_ms,
                    attempts,
                    engine_ref,
                    resume_intent,
                    build_generation,
                )| {
                    lash_core_execution::store::TurnPark::decode(
                        SessionId::from(session_id),
                        lash_sansio::TurnId::from(turn_id),
                        lash_core_execution::store::ParkId::from_feed_sequence(
                            u64::try_from(park_id).unwrap_or_default(),
                        ),
                        &reason_code,
                        &reason_json,
                        u64::try_from(since_ms).unwrap_or_default(),
                        u64::try_from(last_refused_ms).unwrap_or_default(),
                        u32::try_from(attempts).unwrap_or(u32::MAX),
                        engine_ref,
                        resume_intent.and_then(|intent| u64::try_from(intent).ok()),
                        build_generation.as_deref(),
                    )
                },
            )
            .collect()
    }
    async fn turn_park_feed(
        &self,
        after: lash_core_execution::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core_execution::store::ParkFeedPage<lash_core_execution::store::TurnParkTarget>,
        StoreError,
    > {
        self.read_turn_park_feed(after, limit).await
    }
    async fn non_terminal_roots_page(
        &self,
        after: Option<&lash_core_execution::engine::RootRef>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::engine::RootRef>, StoreError> {
        let Some(conn) = self.control_ledger().await? else {
            return Ok(Vec::new());
        };
        let session = after.map_or_else(String::new, |key| key.session.to_string());
        let root = after.map_or_else(String::new, |key| key.root.to_string());
        conn.call(move |conn| {
            let mut stmt = conn.prepare_cached(
                crate::session_roots::session_roots_sql()
                    .roots
                    .select_open_page
                    .sql(),
            )?;
            let rows = stmt.query_map(params![session, root, limit.get() as i64], |row| {
                Ok(lash_core_execution::engine::RootRef {
                    session: SessionId::from(row.get::<_, String>(0)?),
                    root: lash_sansio::TurnId::from(row.get::<_, String>(1)?),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(sqlite_error)
    }
    async fn end_lost_root(
        &self,
        target: &lash_core_execution::engine::RootRef,
        at_ms: u64,
    ) -> Result<Option<lash_core_execution::store::RootTerminal>, StoreError> {
        let Some(conn) = self.control_ledger().await? else {
            return Ok(None);
        };
        let target = target.clone();
        conn.write_flow(move |tx| {
            Ok(
                match crate::session_roots::end_lost_root_conn(tx, &target, at_ms) {
                    Ok(terminal) => crate::conn::TxOutcome::Commit(Ok(terminal)),
                    Err(error) => crate::conn::TxOutcome::Rollback(Err(error)),
                },
            )
        })
        .await
        .map_err(sqlite_error)?
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
            let sql = &crate::session_roots::session_roots_sql().verbs;
            let mut stmt = conn.prepare_cached(sql.intents.sql())?;
            let rows = stmt
                .query_map(
                    params![
                        after.map_or(0, |id| id.sequence()) as i64,
                        limit.get() as i64
                    ],
                    crate::session_roots::intent_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .map(crate::session_roots::StoredIntentRow::decode)
                .collect())
        })
        .await
        .map_err(sqlite_error)?
    }
    async fn compact_turn_park_feed(
        &self,
        through: lash_core_execution::store::ParkFeedCursor,
    ) -> Result<(), StoreError> {
        if !self.location.target().exists() {
            return Ok(());
        }
        let conn = SqliteConnection::open_with_policy(
            self.location.target(),
            self.options.connection_policy,
        )
        .await
        .map_err(|error| StoreError::Backend(error.to_string()))?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        let through_seq = i64::try_from(through.store_sequence()).unwrap_or(i64::MAX);
        conn.write_flow(move |tx| {
            let sql = crate::turn_ingress::turn_ingress_sql();
            // The write lock is held, so this read is the clock's committed
            // sequence. `through` is clamped to it: raising the horizon past
            // `current_seq` would strand events the feed has not yet
            // appended.
            let through_seq = tx
                .query_row(sql.turn_park_clock.select_current.sql(), [], |row| {
                    row.get::<_, i64>(0)
                })?
                .min(through_seq);
            crate::conn::cached_execute(
                tx,
                sql.turn_park_events.delete_events_through.sql(),
                params![through_seq],
            )?;
            crate::conn::cached_execute(
                tx,
                sql.turn_park_clock.raise_compaction_horizon.sql(),
                params![through_seq],
            )?;
            Ok(TxOutcome::Commit(Ok(())))
        })
        .await
        .map_err(sqlite_error)?
    }
    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &lash_core_execution::ExecutionScope,
    ) -> Result<(), StoreError> {
        let scope = scope.clone();
        let scope_id = scope
            .journal_identity()
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .key()
            .to_string();
        let store = self;
        let inspected_scope = scope.clone();
        store
            .conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    let mut statement = tx
                        .prepare(
                            crate::turn_ingress::turn_ingress_sql()
                                .closures
                                .list_all
                                .sql(),
                        )
                        .map_err(sqlite_error)?;
                    let rows = statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(sqlite_error)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sqlite_error)?;
                    drop(statement);
                    for (session_id, encoded) in rows {
                        let authorization: lash_core_execution::TurnCancelClosureAuthorization =
                            serde_json::from_str(&encoded).map_err(|error| {
                                StoreError::StoredDataCorrupt {
                                    record_kind: "TurnCancelClosureAuthorization",
                                    message: error.to_string(),
                                }
                            })?;
                        if authorization.admitted_scope() == &inspected_scope {
                            return Err(StoreError::TurnCancelClosureLifecyclePinned {
                                session_id: SessionId::from(session_id),
                                pending_count: 1,
                            });
                        }
                    }
                    crate::conn::cached_execute(
                        tx,
                        crate::turn_ingress::turn_ingress_sql()
                            .retired_scopes_sqlite
                            .insert_new
                            .sql(),
                        params![scope_id],
                    )
                    .map_err(sqlite_error)?;
                    Ok(())
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)??;
        if let Some(owner) = self.turn_cancel_closure_owner_binding() {
            owner
                .release(&scope)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl lash_core_execution::AttachmentRootSet for SqliteStore {
    fn can_prove_process_owner_death(&self) -> bool {
        self.process_registry.is_some()
    }
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<
        std::collections::BTreeSet<lash_core_execution::AttachmentId>,
        lash_core_execution::StoreError,
    > {
        let catalog = self.location.target();
        if !catalog.exists() {
            return Err(lash_core_execution::StoreError::Backend(format!(
                "attachment GC aborted: durable-core catalog {catalog} does not exist, so live attachment refs cannot be enumerated"
            )));
        }
        let store = self;
        lash_core_execution::AttachmentManifest::forget_aged_uncommitted_intents(
            store,
            intent_grace_cutoff_epoch_ms,
        )
        .await?;
        Ok(
            lash_core_execution::AttachmentManifest::list_all_refs(store)
                .await?
                .into_iter()
                .collect(),
        )
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
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core_execution::StoreError> {
        let store = self;
        lash_core_execution::AttachmentManifest::has_live_ref_for_id(
            store,
            id,
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }
    fn fence(&self) -> lash_core_execution::AttachmentGcFence {
        lash_core_execution::AttachmentGcFence::Fenced
    }
    async fn condemn_attachment(
        &self,
        id: &lash_core_execution::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<lash_core_execution::AttachmentCondemnation, lash_core_execution::StoreError> {
        let store = self;
        store
            .condemn_attachment(id, intent_grace_cutoff_epoch_ms)
            .await
    }
    async fn arm_attachment_delete(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<lash_core_execution::AttachmentDeleteArming, lash_core_execution::StoreError> {
        let store = self;
        store.arm_attachment_delete(id).await
    }
    async fn release_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self;
        store.release_attachment_condemnation(id).await
    }
    async fn recover_abandoned_attachment_write(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self;
        store.recover_abandoned_attachment_write(id).await
    }
    async fn retire_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self;
        store.retire_attachment_condemnation(id).await
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
