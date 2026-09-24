//! [`Store`] open/memory lifecycle plus session head/meta accessors.
//!
//! * Async public reads return `Result` so SQLite and decode failures cannot be
//!   mistaken for missing session state.
//! * A read goes through `self.conn.call(move |c| { ... })`, where the closure
//!   is a *synchronous* rusqlite body returning `rusqlite::Result<T>`.
//! * A read-then-write goes through `self.conn.write(move |tx| { ... })`.
//! * The shared `*_from_conn` helpers in `lib.rs` are synchronous and take a
//!   `&rusqlite::Connection`, so they can be called from inside either closure.
//! * Closures must be `'static` + `Send`: capture owned values (clone strings,
//!   move them in), not borrows of `self`.

use super::*;
use crate::location::{DatabaseLocation, DatabaseTarget, validate_file_database_path};
use lash_sansio::SessionId;

impl SqliteSessionStoreFactory {
    pub(super) fn turn_cancel_closure_owner_binding(
        &self,
    ) -> Option<lash_core_execution::TurnCancelClosureOwnerBinding> {
        self.turn_cancel_closure_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Store {
    pub(crate) async fn open_bound_at(
        core: &DatabaseLocation,
        session_id: &SessionId,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
        turn_cancel_closure_owner: Option<lash_core_execution::TurnCancelClosureOwnerBinding>,
        #[cfg(feature = "testing")] fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        let store = Self::open_at(
            core,
            options,
            clock,
            None,
            turn_cancel_closure_owner,
            #[cfg(feature = "testing")]
            fault_injector,
        )
        .await?;
        #[expect(
            clippy::expect_used,
            reason = "the `OnceLock` belongs to the store value constructed on the line above, so nothing else can have set it"
        )]
        store
            .session_id
            .set(session_id.clone())
            .expect("new SQLite store binding is unset");
        Ok(store)
    }

    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            "Store::open",
        )
        .await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            StoreOptions::default(),
            clock,
            "Store::open_with_clock",
        )
        .await
    }

    pub async fn open_with_options(
        path: &Path,
        options: StoreOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            options,
            Arc::new(lash_core_execution::facade_support::SystemClock),
            "Store::open_with_options",
        )
        .await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(path, options, clock, "Store::open_with_options_and_clock").await
    }

    async fn open_direct(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
        constructor: &'static str,
    ) -> tokio_rusqlite::Result<Self> {
        validate_file_database_path(path, "Store")?;
        let store = Self::open_at(
            &DatabaseLocation::standalone_file(path),
            options,
            clock,
            None,
            None,
            #[cfg(feature = "testing")]
            None,
        )
        .await?;
        warn_process_registry_not_wired(constructor);
        Ok(store)
    }

    /// Open the durable-core database at `core`, attaching the process
    /// registry at `process_registry` when given. Internal opens inherit the
    /// factory's warning or the direct entry's warning.
    pub(crate) async fn open_at(
        core: &DatabaseLocation,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
        process_registry: Option<&DatabaseTarget>,
        turn_cancel_closure_owner: Option<lash_core_execution::TurnCancelClosureOwnerBinding>,
        #[cfg(feature = "testing")] fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        #[cfg(feature = "testing")]
        let conn = SqliteConnection::open_with_fault_injector(
            core.target(),
            options.connection_policy,
            fault_injector,
        )
        .await?;
        #[cfg(not(feature = "testing"))]
        let conn =
            SqliteConnection::open_with_policy(core.target(), options.connection_policy).await?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore).await?;
        let process_registry_attached = if let Some(process_registry) = process_registry {
            attach_process_registry(&conn, process_registry, options.connection_policy).await?;
            true
        } else {
            false
        };
        Ok(Self {
            conn,
            location: core.clone(),
            turn_cancel_closure_owner,
            session_id: Arc::new(OnceLock::new()),
            clock,
            #[cfg(feature = "lashlang")]
            artifact_cache: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "lashlang")]
            artifact_publication_pause: Mutex::new(None),
            options,
            commit_count: AtomicU64::new(commit_count_entropy_seed()),
            process_registry_attached,
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
    }

    pub(crate) async fn open_readonly(core: &DatabaseLocation) -> tokio_rusqlite::Result<Self> {
        // Read-only projections cannot reconcile intents or run a reclamation sweep.
        let conn = SqliteConnection::open_readonly(core.target()).await?;
        Ok(Self {
            conn,
            location: core.clone(),
            turn_cancel_closure_owner: None,
            session_id: Arc::new(OnceLock::new()),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
            #[cfg(feature = "lashlang")]
            artifact_cache: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "lashlang")]
            artifact_publication_pause: Mutex::new(None),
            options: StoreOptions::default(),
            commit_count: AtomicU64::new(commit_count_entropy_seed()),
            process_registry_attached: false,
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
    }

    pub(crate) async fn open_bound_readonly(
        core: &DatabaseLocation,
        session_id: &SessionId,
    ) -> tokio_rusqlite::Result<Self> {
        let store = Self::open_readonly(core).await?;
        #[expect(
            clippy::expect_used,
            reason = "the `OnceLock` belongs to the store value constructed on the line above, so nothing else can have set it"
        )]
        store
            .session_id
            .set(session_id.clone())
            .expect("new read-only SQLite store binding is unset");
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn checkpoint_claim_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(None);
        };
        self.conn
            .call(move |conn| {
                try_load_session_head_meta_from_conn(conn, &session_id)
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }

    pub async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        self.bind_session(&meta.session_id)?;
        let created_at_ms = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &meta.session_id)?;
                    // FIG-3045: the recorded lineage is write-once, so a
                    // metadata replace that moves it is refused here exactly
                    // as admission refuses a conflicting rebind.
                    if let Some(recorded) =
                        crate::session_meta::load_recorded_lineage(tx, &meta.session_id)?
                    {
                        lash_core_execution::store_backend_support::guard_session_meta_relation_rewrite(
                            &meta.session_id,
                            &recorded,
                            &meta.relation,
                        )?;
                    }
                    crate::session_meta::write_session_meta(
                        tx,
                        &meta,
                        crate::session_meta::SessionMetaWrite::Replace,
                        created_at_ms,
                    )?;
                    Ok(())
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)??;
        Ok(())
    }

    pub async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        let selected = self.session_id.get().cloned();
        let meta = self
            .conn
            .call(move |conn| {
                crate::session_meta::load_session_meta(conn, selected.as_ref())
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)?;
        if let Some(meta) = &meta {
            self.bind_session(&meta.session_id)?;
        }
        Ok(meta)
    }
}

/// Attach the configured process registry as `process_registry` and verify it
/// is a Lash process registry at this build's schema version. A registry still
/// being created (version 0, no tables) is waited for up to the connection's
/// busy timeout.
pub(crate) async fn attach_process_registry(
    conn: &SqliteConnection,
    process_registry: &DatabaseTarget,
    policy: SqliteConnectionPolicy,
) -> rusqlite::Result<()> {
    if !process_registry.exists() {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!(
                "configured Lash process registry does not exist: {process_registry}"
            )),
        ));
    }
    let name = process_registry.open_name();
    conn.call(move |conn| {
        conn.execute(crate::connection_sql::ATTACH_PROCESS_REGISTRY, params![name])?;
        let expected_version = crate::schema::PROCESS_SCHEMA_VERSION;
        let deadline = std::time::Instant::now() + policy.busy_timeout;
        loop {
            let version: i32 = conn.query_row(
                crate::connection_sql::SELECT_PROCESS_REGISTRY_USER_VERSION,
                [],
                |row| row.get(0),
            )?;
            let has_processes = conn
                .query_row(
                    crate::connection_sql::SELECT_PROCESS_REGISTRY_IS_PROVISIONED,
                    [],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if version == expected_version && has_processes {
                break;
            }
            if version == 0 && !has_processes && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
                Some(format!(
                    "configured database is not a Lash process registry: expected schema version {expected_version} with table `processes`, found version {version}"
                )),
            ));
        }
        Ok(())
    })
    .await
}
