//! [`Store`] open/memory lifecycle plus session head/meta accessors.
//!
//! This is one of the two reference modules (with `blobs.rs`) establishing the
//! tokio-rusqlite translation pattern every other module follows:
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

impl Store {
    pub(crate) async fn open_bound_with_options_clock_and_process_registry(
        path: &Path,
        session_id: &str,
        options: StoreOptions,
        clock: Arc<dyn lash_core::Clock>,
        process_registry_path: Option<&Path>,
        #[cfg(feature = "testing")] fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        let store = Self::open_with_options_clock_and_process_registry(
            path,
            options,
            clock,
            process_registry_path,
            #[cfg(feature = "testing")]
            fault_injector,
        )
        .await?;
        store
            .session_id
            .set(session_id.to_string())
            .expect("new SQLite store binding is unset");
        Ok(store)
    }

    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options(path, StoreOptions::default()).await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(path, StoreOptions::default(), clock).await
    }

    pub async fn open_with_options(
        path: &Path,
        options: StoreOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(
            path,
            options,
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_clock_and_process_registry(
            path,
            options,
            clock,
            None,
            #[cfg(feature = "testing")]
            None,
        )
        .await
    }

    pub(crate) async fn open_with_options_clock_and_process_registry(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core::Clock>,
        process_registry_path: Option<&Path>,
        #[cfg(feature = "testing")] fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        #[cfg(feature = "testing")]
        let conn = SqliteConnection::open_with_fault_injector(
            path,
            options.connection_policy,
            fault_injector,
        )
        .await?;
        #[cfg(not(feature = "testing"))]
        let conn = SqliteConnection::open_with_policy(path, options.connection_policy).await?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore).await?;
        let process_registry_attached = if let Some(process_registry_path) = process_registry_path {
            if !process_registry_path.exists() {
                return Err(tokio_rusqlite::Error::Error(
                    rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                        Some(format!(
                            "configured Lash process registry does not exist: {}",
                            process_registry_path.display()
                        )),
                    ),
                ));
            }
            let path = process_registry_path.to_string_lossy().into_owned();
            conn.call(move |conn| {
                conn.execute("ATTACH DATABASE ?1 AS process_registry", params![path])?;
                let expected_version = crate::schema::PROCESS_SCHEMA_VERSION;
                let deadline = std::time::Instant::now()
                    + options.connection_policy.busy_timeout;
                loop {
                    let version: i32 = conn.query_row(
                        "PRAGMA process_registry.user_version",
                        [],
                        |row| row.get(0),
                    )?;
                    let has_processes = conn
                        .query_row(
                            "SELECT 1 FROM process_registry.sqlite_master
                             WHERE type = 'table' AND name = 'processes'",
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
            .await?;
            true
        } else {
            false
        };
        Ok(Self {
            conn,
            session_id: OnceLock::new(),
            clock,
            artifact_cache: Mutex::new(BTreeMap::new()),
            options,
            commit_count: AtomicU64::new(commit_count_entropy_seed()),
            process_registry_attached,
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
    }

    /// Open the local database read-only for internal read projections.
    pub(crate) async fn open_readonly(path: &Path) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open_readonly(path).await?;
        Ok(Self {
            conn,
            session_id: OnceLock::new(),
            clock: Arc::new(lash_core::facade_support::SystemClock),
            artifact_cache: Mutex::new(BTreeMap::new()),
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
        path: &Path,
        session_id: &str,
    ) -> tokio_rusqlite::Result<Self> {
        let store = Self::open_readonly(path).await?;
        store
            .session_id
            .set(session_id.to_string())
            .expect("new read-only SQLite store binding is unset");
        Ok(store)
    }

    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options(StoreOptions {
            blob_profile: BuiltinBlobProfile::LowLatency,
            ..StoreOptions::default()
        })
        .await
    }

    pub async fn memory_with_clock(
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(
            StoreOptions {
                blob_profile: BuiltinBlobProfile::LowLatency,
                ..StoreOptions::default()
            },
            clock,
        )
        .await
    }

    pub async fn memory_with_options(options: StoreOptions) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(
            options,
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await
    }

    pub async fn memory_with_options_and_clock(
        options: StoreOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open_in_memory_with_policy(options.connection_policy).await?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore).await?;
        Ok(Self {
            conn,
            session_id: OnceLock::new(),
            clock,
            artifact_cache: Mutex::new(BTreeMap::new()),
            options,
            commit_count: AtomicU64::new(commit_count_entropy_seed()),
            process_registry_attached: false,
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
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
                crate::session_meta::load_session_meta(conn, selected.as_deref())
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
