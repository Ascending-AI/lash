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
        let conn = SqliteConnection::open_with_fault_injector(path, fault_injector).await?;
        #[cfg(not(feature = "testing"))]
        let conn = SqliteConnection::open(path).await?;
        ensure_schema(&conn).await?;
        apply_pragmas(&conn, StoreBacking::File).await?;
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
                    + std::time::Duration::from_millis(crate::conn::BUSY_TIMEOUT_MS as u64);
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
            commit_count: AtomicU64::new(0),
            process_registry_attached,
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
    }

    /// Open the local database read-only. Used by export/resume call sites that
    /// must never mutate the source.
    pub async fn open_readonly(path: &Path) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open_readonly(path).await?;
        Ok(Self {
            conn,
            session_id: OnceLock::new(),
            clock: Arc::new(lash_core::facade_support::SystemClock),
            artifact_cache: Mutex::new(BTreeMap::new()),
            options: StoreOptions::default(),
            commit_count: AtomicU64::new(0),
            process_registry_attached: false,
            #[cfg(test)]
            checkpoint_probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: AtomicUsize::new(0),
        })
    }

    pub async fn load_picker_info(&self) -> Result<Option<SessionPickerInfo>, StoreError> {
        let Some(selected_session_id) = self.session_id.get().cloned() else {
            return Ok(None);
        };
        self.conn
            .call(move |conn| {
                let meta = conn
                    .query_row(
                        "SELECT session_id, cwd, relation_json
                         FROM session_meta WHERE session_id = ?1",
                        params![selected_session_id],
                        |row| {
                            let relation_json: Option<String> = row.get(2)?;
                            let relation = relation_json
                                .map(|json| {
                                    serde_json::from_str(&json).map_err(|error| {
                                        sqlite_conversion_error(stored_data_corrupt(
                                            "SessionMeta relation",
                                            error,
                                        ))
                                    })
                                })
                                .transpose()?
                                .unwrap_or_default();
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                relation,
                            ))
                        },
                    )
                    .optional()?;
                let Some((session_id, cwd, relation)) = meta else {
                    return Ok(None);
                };

                let head_row: Option<(String, i64, Option<String>, Option<String>)> = conn
                    .query_row(
                        "SELECT head_json, head_revision, leaf_node_id, checkpoint_ref
                         FROM session_head WHERE session_id = ?1",
                        params![session_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                let Some((head_json, head_revision, leaf_node_id, checkpoint_ref)) = head_row
                else {
                    return Ok(None);
                };
                let payload = lash_core::store::decode_versioned_json_record::<SessionHeadPayload>(
                    &head_json,
                    "SessionHeadMeta",
                    lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                )
                .map_err(|error| {
                    sqlite_conversion_error(map_record_decode_error("SessionHeadMeta", error))
                })?;
                let head_meta = SessionHeadMeta::assemble(
                    payload,
                    head_revision as u64,
                    checkpoint_ref.map(Into::into),
                    leaf_node_id,
                );
                let graph =
                    Self::load_session_graph_from_conn(conn, &session_id, head_meta.leaf_node_id)
                        .map_err(sqlite_conversion_error)?;

                Ok(Some(SessionPickerInfo {
                    session_id,
                    cwd,
                    relation,
                    first_user_message: graph.first_user_message(),
                    user_message_count: graph.user_message_count(),
                }))
            })
            .await
            .map_err(sqlite_error)
    }

    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options(StoreOptions {
            blob_profile: BuiltinBlobProfile::LowLatency,
            gc_policy: StoreGcPolicy::default(),
        })
        .await
    }

    pub async fn memory_with_clock(
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(
            StoreOptions {
                blob_profile: BuiltinBlobProfile::LowLatency,
                gc_policy: StoreGcPolicy::default(),
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
        let conn = SqliteConnection::open_in_memory().await?;
        ensure_schema(&conn).await?;
        apply_pragmas(&conn, StoreBacking::Memory).await?;
        Ok(Self {
            conn,
            session_id: OnceLock::new(),
            clock,
            artifact_cache: Mutex::new(BTreeMap::new()),
            options,
            commit_count: AtomicU64::new(0),
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
        let Some(session_id) = self.session_id.get().cloned() else {
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
        let relation_json = serde_json::to_string(&meta.relation)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &meta.session_id)?;
                    tx.execute(
                        "INSERT INTO session_meta
                     (session_id, session_name, created_at, model, cwd, relation_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(session_id) DO UPDATE SET
                         session_name = excluded.session_name,
                         created_at = excluded.created_at,
                         model = excluded.model,
                         cwd = excluded.cwd,
                         relation_json = excluded.relation_json",
                        params![
                            meta.session_id,
                            meta.session_name,
                            meta.created_at,
                            meta.model,
                            meta.cwd,
                            relation_json,
                        ],
                    )
                    .map_err(sqlite_error)?;
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
                if let Some(session_id) = selected {
                    return load_session_meta_from_conn(conn, &session_id)
                        .map_err(sqlite_conversion_error);
                }
                let mut stmt = conn.prepare(
                    "SELECT session_id FROM session_meta ORDER BY session_id ASC LIMIT 2",
                )?;
                let session_ids = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                if session_ids.len() == 1 {
                    load_session_meta_from_conn(conn, &session_ids[0])
                        .map_err(sqlite_conversion_error)
                } else {
                    Ok(None)
                }
            })
            .await
            .map_err(sqlite_error)?;
        if let Some(meta) = &meta {
            self.bind_session(&meta.session_id)?;
        }
        Ok(meta)
    }
}
