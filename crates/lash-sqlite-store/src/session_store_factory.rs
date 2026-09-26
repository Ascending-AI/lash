use super::*;

#[path = "factory_reads.rs"]
mod factory_reads;

type BoundArtifactStores = (
    Arc<dyn lash_core_execution::ProcessExecutionEnvStore>,
    lash_core_execution::ProcessEngineRegistry,
);
type SharedArtifactStores = Arc<std::sync::Mutex<Option<BoundArtifactStores>>>;

/// Explicit first-party factory for one SQLite durable-core catalog.
///
/// A [`SqliteBackend`] or [`SqliteStoreSet`] opens the one a host's core
/// runs on; the factory never becomes a default: app storage and runtime
/// storage remain host-owned decisions.
#[derive(Clone)]
pub struct SqliteSessionStoreFactory {
    /// The one durable-core catalog every session of this factory lives in.
    pub(crate) core: DatabaseLocation,
    /// The process registry maintenance attaches beside the catalog.
    pub(crate) process_registry: Option<DatabaseTarget>,
    pub(crate) options: StoreOptions,
    pub(crate) clock: Arc<dyn lash_core_execution::Clock>,
    #[cfg(feature = "testing")]
    pub(crate) fault_injector: Option<testing::SqliteFaultInjector>,
    /// The backend's effect journal: the retained-evidence sweep attaches
    /// it to retire quiescent operation scopes whose receipt this catalog
    /// holds (ADR 0067). Fixed by the backend's location when the factory
    /// is opened; `None` when the backend journals effects elsewhere.
    pub(crate) effect_journal: Option<DatabaseLocation>,
    pub(crate) turn_cancel_closure_owner:
        Arc<std::sync::Mutex<Option<lash_core_execution::TurnCancelClosureOwnerBinding>>>,
    effect_host: Arc<std::sync::Mutex<Option<Arc<dyn lash_core_execution::EffectHost>>>>,
    artifact_stores: SharedArtifactStores,
}

impl SqliteSessionStoreFactory {
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

    pub fn new(root: impl Into<PathBuf>) -> Self {
        warn_process_registry_not_wired("SqliteSessionStoreFactory::new");
        Self::for_root(root.into(), StoreOptions::default(), None)
    }

    pub fn with_options(root: impl Into<PathBuf>, options: StoreOptions) -> Self {
        warn_process_registry_not_wired("SqliteSessionStoreFactory::with_options");
        Self::for_root(root.into(), options, None)
    }

    /// This is the warning-free durable constructor when the deployment uses a Lash SQLite
    /// process registry.
    pub fn new_with_process_registry(
        root: impl Into<PathBuf>,
        process_registry_path: impl Into<PathBuf>,
    ) -> Self {
        Self::for_root(
            root.into(),
            StoreOptions::default(),
            Some(process_registry_path.into()),
        )
    }

    pub fn with_options_and_process_registry(
        root: impl Into<PathBuf>,
        options: StoreOptions,
        process_registry_path: impl Into<PathBuf>,
    ) -> Self {
        Self::for_root(root.into(), options, Some(process_registry_path.into()))
    }

    fn for_root(root: PathBuf, options: StoreOptions, process_registry: Option<PathBuf>) -> Self {
        Self::at(
            DatabaseLocation::standalone_file(&root.join(DURABLE_CORE_DB_FILE)),
            process_registry.map(DatabaseTarget::File),
            None,
            options,
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
    }

    /// The factory over `core` in one backend, with its registry and
    /// effect journal fixed by the backend's location.
    pub(crate) fn at(
        core: DatabaseLocation,
        process_registry: Option<DatabaseTarget>,
        effect_journal: Option<DatabaseLocation>,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        Self {
            core,
            process_registry,
            options,
            clock,
            #[cfg(feature = "testing")]
            fault_injector: None,
            effect_journal,
            turn_cancel_closure_owner: Arc::new(std::sync::Mutex::new(None)),
            effect_host: Arc::new(std::sync::Mutex::new(None)),
            artifact_stores: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn lash_core_execution::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The clock a session store opened by this factory runs on.
    fn session_store_clock(&self) -> Arc<dyn lash_core_execution::Clock> {
        Arc::clone(&self.clock)
    }

    /// The method and backing field do not exist without the `testing` feature.
    #[cfg(feature = "testing")]
    pub fn with_fault_injector(mut self, injector: testing::SqliteFaultInjector) -> Self {
        self.fault_injector = Some(injector);
        self
    }

    /// The URI a raw SQLite connection opens this factory's durable-core
    /// catalog through, file or memory.
    pub fn catalog_uri(&self) -> String {
        self.core.target().uri()
    }

    /// Open and project one committed session through SQLite's read-only mode.
    ///
    /// The raw SQLite handle stays private so callers receive only the
    /// canonical [`lash_core_execution::SessionReadView`], which has no mutating store
    /// operations. This path does not mutate durable session, lease, claim, or
    /// graph state. SQLite may materialize its `-wal` and `-shm` wal-index
    /// sidecars while reading a cold WAL catalog. Consequently a catalog on
    /// read-only media is inspectable only when the required sidecars already
    /// exist; otherwise the SQLite failure surfaces as
    /// [`lash_core_execution::StoreError::Backend`]. `immutable=1` is deliberately not
    /// used because another process may still hold a writer.
    pub async fn open_read_only(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::SessionReadView>, lash_core_execution::StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        if !self.core.target().exists() {
            return Ok(None);
        }
        let store = Store::open_bound_readonly(&self.core, session_id)
            .await
            .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
        lash_core_execution::store::load_persisted_session_read_view(&store).await
    }
}

impl SqliteSessionStoreFactory {
    /// Concrete constructor behind [`SessionStoreFactory::create_store`]; the
    /// gated conformance factory shares it.
    #[expect(
        clippy::disallowed_methods,
        reason = "the sqlite store factory ensures the host-supplied store root exists before opening (FIG-2971)"
    )]
    pub(crate) async fn create_bound_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<Store>, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        if let Some(root) = self.core.target().file_path().and_then(Path::parent) {
            std::fs::create_dir_all(root).map_err(|err| StoreError::Backend(err.to_string()))?;
        }
        let store = Arc::new(
            Store::open_bound_at(
                &self.core,
                &request.session_id,
                self.options,
                self.session_store_clock(),
                self.turn_cancel_closure_owner_binding(),
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?,
        );
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
            pending_observer_intents: request.pending_observer_intents.clone(),
        };
        let created_at_ms = self.clock.timestamp_ms();
        let fleet_format = store.fleet_format();
        store
            .conn
            .write_flow(move |tx| {
                let deleted = tx
                    .query_row(
                        crate::session_sql::session_sql()
                            .deleted_sqlite
                            .exists
                            .sql(),
                        params![meta.session_id.as_str()],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if deleted {
                    return Ok(TxOutcome::Rollback(Err(
                        lash_core_execution::StoreError::SessionDeleted {
                            session_id: meta.session_id,
                        },
                    )));
                }
                session_meta::write_session_meta(
                    tx,
                    &meta,
                    session_meta::SessionMetaWrite::Insert,
                    created_at_ms,
                    fleet_format,
                )
                .map_err(sqlite_conversion_error)?;
                Ok(TxOutcome::Commit(Ok(())))
            })
            .await
            .map_err(sqlite_error)??;
        Ok(store)
    }

    /// Concrete reopen behind [`SessionStoreFactory::open_existing_store`];
    /// the gated conformance factory shares it.
    pub(crate) async fn open_existing_bound_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<Store>>, String> {
        if !self.core.target().exists() {
            return Ok(None);
        }
        let store = Arc::new(
            Store::open_bound_at(
                &self.core,
                &request.session_id,
                self.options,
                self.session_store_clock(),
                self.turn_cancel_closure_owner_binding(),
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|err| err.to_string())?,
        );
        if store
            .load_session_meta()
            .await
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(store))
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SqliteSessionStoreFactory {
    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core_execution::EffectHost>) {
        let catalog = self.core.target().canonical_name();
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

    fn bind_artifact_stores(
        &self,
        process_env_store: Arc<dyn lash_core_execution::ProcessExecutionEnvStore>,
        process_engines: lash_core_execution::ProcessEngineRegistry,
    ) {
        *self
            .artifact_stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((process_env_store, process_engines));
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

    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        Ok(self.create_bound_store(request).await? as Arc<dyn RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        lash_core_execution::store::validate_session_id(&request.session_id)
            .map_err(|error| error.to_string())?;
        Ok(self
            .open_existing_bound_store(request)
            .await?
            .map(|store| store as Arc<dyn RuntimePersistence>))
    }

    /// The store-set [`open_store`] port, as the factory answers it: a
    /// fresh, unbound [`Store`] on the factory's durable-core catalog, on a
    /// connection of its own.
    ///
    /// [`open_store`]: crate::SqliteStoreSet::open_store
    async fn open_unbound_store(&self) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        Ok(Arc::new(
            Store::open_at(
                &self.core,
                self.options,
                self.session_store_clock(),
                None,
                None,
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?,
        ))
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::SessionReadView>, lash_core_execution::StoreError> {
        self.open_read_only(session_id).await
    }

    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, StoreError> {
        if !self.core.target().exists() {
            return Ok(Vec::new());
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let filter = filter.clone();
        conn.call(move |conn| super::session_listing::list_session_summaries(conn, &filter))
            .await
            .map_err(sqlite_error)
    }

    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash_core_execution::store::UnsettledTurnCounts, StoreError> {
        if !self.core.target().exists() {
            return Ok(lash_core_execution::store::UnsettledTurnCounts::default());
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        conn.read(|conn| {
            let (parked, oldest_since_ms, in_flight): (i64, Option<i64>, i64) = conn
                .query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .family
                        .count_unsettled_turns
                        .sql(),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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
        if !self.core.target().exists() {
            return Ok(Vec::new());
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
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
                let mut statement = conn.prepare(
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

    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &lash_sansio::TurnId,
    ) -> Result<Option<lash_core_execution::store::RootTerminal>, StoreError> {
        self.read_root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash_core_execution::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::store::ControlIntent>, StoreError> {
        self.read_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core_execution::store::ParkFeedCursor,
    ) -> Result<(), StoreError> {
        if !self.core.target().exists() {
            return Ok(());
        }
        let conn =
            SqliteConnection::open_with_policy(self.core.target(), self.options.connection_policy)
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
            tx.execute(
                sql.turn_park_events.delete_events_through.sql(),
                params![through_seq],
            )?;
            tx.execute(
                sql.turn_park_clock.raise_compaction_horizon.sql(),
                params![through_seq],
            )?;
            Ok(TxOutcome::Commit(Ok(())))
        })
        .await
        .map_err(sqlite_error)?
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        if !self.core.target().exists() {
            return Ok(None);
        }
        let store = Arc::new(
            Store::open_bound_at(
                &self.core,
                session_id,
                self.options,
                self.session_store_clock(),
                self.turn_cancel_closure_owner_binding(),
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?,
        );
        if store.load_session_meta().await?.is_none() {
            return Ok(None);
        }
        Ok(Some(store as Arc<dyn RuntimePersistence>))
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnCancelClosureAuthorization>, StoreError> {
        let Some(store) = self.open_existing_store_by_id(session_id).await? else {
            return Ok(Vec::new());
        };
        store.pending_turn_cancel_closure_pins().await
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
        let store = self
            .open_catalog_for_maintenance("turn cancellation scope retirement")
            .await?;
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
                    tx.execute(
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

    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        if !self.core.target().exists() {
            return Ok(Some(false));
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let session_id = request.session_id.clone();
        conn.call(move |conn| {
            conn.query_row(
                crate::turn_ingress::turn_ingress_sql()
                    .family
                    .has_claimable_work
                    .sql(),
                params![session_id.as_str(), now_epoch_ms as i64],
                |row| row.get(0),
            )
        })
        .await
        .map(Some)
        .map_err(sqlite_error)
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(|error| error.to_string())?;
        if !self.core.target().exists() {
            return Ok(false);
        }
        let conn =
            SqliteConnection::open_with_policy(self.core.target(), self.options.connection_policy)
                .await
                .map_err(|err| err.to_string())?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
            .await
            .map_err(|err| err.to_string())?;
        let session_id = SessionId::from(session_id.to_string());
        conn.call(move |conn| {
            conn.query_row(
                crate::session_sql::session_sql()
                    .deleted_sqlite
                    .exists
                    .sql(),
                params![session_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
        })
        .await
        .map_err(|err| err.to_string())
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)?;
        let report = delete_session_from_catalog(
            &self.core,
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
                    lash_core_execution::StoreError::Backend(message),
                    report.clone(),
                )
            })?;
        }
        Ok(report)
    }

    async fn pin(
        &self,
        node_id: &str,
    ) -> Result<lash_core_execution::ForkPoint, lash_core_execution::StoreError> {
        pin_in_catalog(&self.core, node_id, self.options.connection_policy).await
    }

    async fn unpin(&self, node_id: &str) -> Result<(), lash_core_execution::StoreError> {
        unpin_in_catalog(&self.core, node_id, self.options.connection_policy).await
    }

    async fn fork_points(
        &self,
    ) -> Result<Vec<lash_core_execution::ForkPoint>, lash_core_execution::StoreError> {
        fork_points_in_catalog(&self.core, self.options.connection_policy).await
    }

    async fn fork_at(
        &self,
        request: &lash_core_execution::ForkSessionRequest,
    ) -> Result<lash_core_execution::ForkSessionReceipt, lash_core_execution::StoreError> {
        fork_at_in_catalog(
            &self.core,
            request,
            self.clock.timestamp_ms(),
            self.options.connection_policy,
        )
        .await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::AttachmentRootSet for SqliteSessionStoreFactory {
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
        let catalog = self.core.target();
        if !catalog.exists() {
            return Err(lash_core_execution::StoreError::Backend(format!(
                "attachment GC aborted: durable-core catalog {catalog} does not exist, so live attachment refs cannot be enumerated"
            )));
        }
        let store = Store::open_at(
            &self.core,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry.as_ref(),
            self.turn_cancel_closure_owner_binding(),
            #[cfg(feature = "testing")]
            self.fault_injector.clone(),
        )
        .await
        .map_err(|err| {
            lash_core_execution::StoreError::Backend(format!(
                "attachment GC aborted: durable-core catalog {catalog} could not be opened: {err}"
            ))
        })?;
        lash_core_execution::AttachmentManifest::forget_aged_uncommitted_intents(
            &store,
            intent_grace_cutoff_epoch_ms,
        )
        .await?;
        Ok(
            lash_core_execution::AttachmentManifest::list_all_refs(&store)
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
        let store = self
            .open_catalog_for_maintenance("condemnation enumeration")
            .await?;
        store.list_attachment_condemnations().await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core_execution::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core_execution::StoreError> {
        let store = self.open_catalog_for_maintenance("root re-check").await?;
        lash_core_execution::AttachmentManifest::has_live_ref_for_id(
            &store,
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
        let store = self.open_catalog_for_maintenance("condemnation").await?;
        store
            .condemn_attachment(id, intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn arm_attachment_delete(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<lash_core_execution::AttachmentDeleteArming, lash_core_execution::StoreError> {
        let store = self.open_catalog_for_maintenance("delete arming").await?;
        store.arm_attachment_delete(id).await
    }

    async fn release_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self
            .open_catalog_for_maintenance("condemnation release")
            .await?;
        store.release_attachment_condemnation(id).await
    }

    async fn recover_abandoned_attachment_write(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self
            .open_catalog_for_maintenance("abandoned attachment write recovery")
            .await?;
        store.recover_abandoned_attachment_write(id).await
    }

    async fn retire_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self
            .open_catalog_for_maintenance("condemnation retirement")
            .await?;
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
