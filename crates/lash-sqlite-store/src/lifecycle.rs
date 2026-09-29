//! [`SqliteStore`] open/memory lifecycle plus session head/meta accessors.
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
use lash_core_execution::FleetFormatStore;
use lash_sansio::SessionId;

impl SqliteStore {
    /// Open a named database file for fixtures that inspect or corrupt raw rows.
    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_for_testing(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_file_with_options_and_clock_for_testing(
            path,
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_with_clock_for_testing(
        path: &Path,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_file_with_options_and_clock_for_testing(path, StoreOptions::default(), clock)
            .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_with_options_for_testing(
        path: &Path,
        options: StoreOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_file_with_options_and_clock_for_testing(
            path,
            options,
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn open_file_with_options_and_clock_for_testing(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        validate_file_database_path(path, "SqliteStore test fixture")?;
        Self::open_at(
            &DatabaseLocation::standalone_file(path),
            options,
            clock,
            None,
            None,
            lash_core_execution::FleetFormat::writable(),
            #[cfg(feature = "testing")]
            None,
        )
        .await
    }

    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            "SqliteStore::open",
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
            "SqliteStore::open_with_clock",
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
            "SqliteStore::open_with_options",
        )
        .await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_direct(
            path,
            options,
            clock,
            "SqliteStore::open_with_options_and_clock",
        )
        .await
    }

    async fn open_direct(
        root: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
        constructor: &'static str,
    ) -> tokio_rusqlite::Result<Self> {
        let location = crate::backend::file_location(root, constructor)?;
        let identity: Arc<str> = location.identity().into();
        let core =
            DatabaseLocation::in_backend(&location, &identity, SqliteDatabase::DurableCore, None);
        let store = Self::open_at(
            &core,
            options,
            clock,
            None,
            None,
            lash_core_execution::FleetFormat::writable(),
            #[cfg(feature = "testing")]
            None,
        )
        .await?;
        warn_process_registry_not_wired(constructor);
        Ok(store)
    }

    /// Open a durable-core catalog under `root` with deterministic write faults.
    #[cfg(feature = "testing")]
    pub async fn open_with_fault_injector_for_testing(
        root: &Path,
        injector: crate::testing::SqliteFaultInjector,
    ) -> tokio_rusqlite::Result<Self> {
        let constructor = "SqliteStore::open_with_fault_injector_for_testing";
        let location = crate::backend::file_location(root, constructor)?;
        let identity: Arc<str> = location.identity().into();
        let core =
            DatabaseLocation::in_backend(&location, &identity, SqliteDatabase::DurableCore, None);
        let store = Self::open_at(
            &core,
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            None,
            None,
            lash_core_execution::FleetFormat::writable(),
            Some(injector),
        )
        .await?;
        warn_process_registry_not_wired(constructor);
        Ok(store)
    }

    /// Open the durable-core database admitting `writable` as the opening
    /// build's fleet-format writable range.
    ///
    /// Testing seam for FIG-3796's rollout proofs: the recorded row is read
    /// against `writable` rather than this binary's own
    /// [`lash_core_execution::FleetFormat::writable_range`], so a test can
    /// stand in for a build whose range does — or does not — still write the
    /// generation the fleet recorded. Production opens always pass this
    /// build's range.
    #[cfg(feature = "testing")]
    pub async fn open_with_fleet_writable_range_for_testing(
        path: &Path,
        writable: lash_core_execution::compat::VersionRange,
    ) -> Result<Self, lash_core_execution::StoreError> {
        validate_file_database_path(path, "Store").map_err(sqlite_async_error)?;
        let store = Self::open_at(
            &DatabaseLocation::standalone_file(path),
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            None,
            None,
            writable,
            None,
        )
        .await
        .map_err(sqlite_async_error)?;
        warn_process_registry_not_wired("SqliteStore::open_with_fleet_writable_range_for_testing");
        Ok(store)
    }

    /// Open the durable-core database at `core`, attaching the process
    /// registry at `process_registry` when given. Internal opens inherit the
    /// factory's warning or the direct entry's warning.
    ///
    /// `writable` is the opening build's fleet-format writable range: the
    /// recorded row is admitted against it, so a generation the build cannot
    /// write refuses the open rather than being wound back.
    pub(crate) async fn open_at(
        core: &DatabaseLocation,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
        process_registry: Option<&DatabaseTarget>,
        turn_cancel_closure_owner: Option<lash_core_execution::TurnCancelClosureOwnerBinding>,
        writable: lash_core_execution::compat::VersionRange,
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
        crate::schema::ensure_versioned_schema_with_writable(
            &conn,
            SqliteDatabase::DurableCore,
            writable,
        )
        .await?;
        let process_registry_attached = if let Some(process_registry) = process_registry {
            attach_process_registry(&conn, process_registry, options.connection_policy).await?;
            true
        } else {
            false
        };
        let mut readers = Vec::with_capacity(options.connection_policy.read_connections.get());
        for _ in 0..options.connection_policy.read_connections.get() {
            readers.push(SqliteConnection::open_readonly(core.target()).await?);
        }
        Ok(Self {
            conn,
            location: core.clone(),
            turn_cancel_closure_owner: Mutex::new(turn_cancel_closure_owner),
            process_registry: process_registry.cloned(),
            readers,
            next_reader: AtomicU64::new(0),
            decoded_graph_node_bodies: Arc::new(AtomicU64::new(0)),
            decoded_usage_rows: Arc::new(AtomicU64::new(0)),
            decoded_usage_holes: Arc::new(AtomicU64::new(0)),
            decoded_turn_receipt_bodies: Arc::new(AtomicU64::new(0)),
            clock,
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

    #[cfg(test)]
    pub(crate) async fn open_readonly(core: &DatabaseLocation) -> tokio_rusqlite::Result<Self> {
        // Read-only projections cannot reconcile intents or run a reclamation sweep.
        let conn = SqliteConnection::open_readonly(core.target()).await?;
        let fleet_format = conn
            .call(|conn| crate::compat::recorded_or_current(conn))
            .await?;
        conn.observe_fleet_for_testing(fleet_format);
        let readers = vec![conn.clone()];
        Ok(Self {
            conn,
            location: core.clone(),
            turn_cancel_closure_owner: Mutex::new(None),
            process_registry: None,
            readers,
            next_reader: AtomicU64::new(0),
            decoded_graph_node_bodies: Arc::new(AtomicU64::new(0)),
            decoded_usage_rows: Arc::new(AtomicU64::new(0)),
            decoded_usage_holes: Arc::new(AtomicU64::new(0)),
            decoded_turn_receipt_bodies: Arc::new(AtomicU64::new(0)),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
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

    #[cfg(test)]
    pub(crate) fn checkpoint_admission_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        let session_id = session_id.clone();
        let fleet = self.fleet_format();
        self.read_connection()
            .call(move |conn| {
                try_load_session_head_meta_from_conn(conn, &session_id, fleet)
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }

    pub async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        let created_at_ms = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let fleet_format = tx.fleet();
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
                        fleet_format,
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

    pub async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        let selected = session_id.clone();
        self.read_connection()
            .call(move |conn| {
                crate::session_meta::load_session_meta(conn, Some(&selected))
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
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
        crate::conn::cached_execute(
            conn,
            crate::connection_sql::ATTACH_PROCESS_REGISTRY,
            params![name],
        )?;
        let deadline = std::time::Instant::now() + policy.busy_timeout;
        loop {
            let has_processes = conn
                .query_row(
                    crate::connection_sql::SELECT_PROCESS_REGISTRY_IS_PROVISIONED,
                    [],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if has_processes {
                break;
            }
            if std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
                Some("configured database has no Lash process registry table".to_owned()),
            ));
        }
        let row: Option<(String, i64, i64, i64)> = conn
            .query_row(
                "SELECT component, version, min_reader, fleet_format
             FROM process_registry.lash_compat WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let descriptor = lash_core_execution::compat::descriptor(
            lash_core_execution::compat::ComponentId::SQLITE_REGISTRY,
        )
        .ok_or(rusqlite::Error::InvalidQuery)?;
        let stamp = match row {
            Some((component, version, min_reader, _fleet))
                if component == descriptor.component.as_str() =>
            {
                match (u32::try_from(version), u32::try_from(min_reader)) {
                    (Ok(version), Ok(min_reader)) => {
                        lash_core_execution::compat::StampRead::Present(
                            lash_core_execution::compat::CompatStamp {
                                version,
                                min_reader,
                            },
                        )
                    }
                    _ => lash_core_execution::compat::StampRead::Unreadable(format!(
                        "version {version}, min_reader {min_reader} is negative"
                    )),
                }
            }
            Some((component, ..)) => lash_core_execution::compat::StampRead::Unreadable(format!(
                "component is {component}"
            )),
            None => lash_core_execution::compat::StampRead::Absent { populated: true },
        };
        lash_core_execution::compat::admit(descriptor, stamp).map_err(|refusal| {
            crate::sqlite_conversion_error(StoreError::Incompatible { refusal })
        })?;
        Ok(())
    })
    .await
}
