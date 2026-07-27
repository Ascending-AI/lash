//! [`Store`] open/memory lifecycle plus session head/meta accessors.
//!
//! This is one of the two reference modules (with `blobs.rs`) establishing the
//! tokio-rusqlite translation pattern every other module follows:
//!
//! * The async public methods keep the *exact* the prior store signatures.
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
    ) -> tokio_rusqlite::Result<Self> {
        let store = Self::open_with_options_clock_and_process_registry(
            path,
            options,
            clock,
            process_registry_path,
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
        Self::open_with_options_and_clock(path, options, Arc::new(lash_core::SystemClock)).await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_clock_and_process_registry(path, options, clock, None).await
    }

    pub(crate) async fn open_with_options_clock_and_process_registry(
        path: &Path,
        options: StoreOptions,
        clock: Arc<dyn lash_core::Clock>,
        process_registry_path: Option<&Path>,
    ) -> tokio_rusqlite::Result<Self> {
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
            clock: Arc::new(lash_core::SystemClock),
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

    pub async fn load_picker_info(&self) -> Option<SessionPickerInfo> {
        let selected_session_id = self.session_id.get()?.clone();
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
                                .and_then(|json| serde_json::from_str(&json).ok())
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

                let head_json: String = conn
                    .query_row(
                        "SELECT head_json FROM session_head WHERE session_id = ?1",
                        params![session_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or_else(|| "{}".to_string());
                let head_meta =
                    serde_json::from_str::<SessionHeadMeta>(&head_json).unwrap_or_default();
                let graph =
                    Self::load_session_graph_from_conn(conn, &session_id, head_meta.leaf_node_id)
                        .unwrap_or_default();

                Ok(Some(SessionPickerInfo {
                    session_id,
                    cwd,
                    relation,
                    first_user_message: graph.first_user_message(),
                    user_message_count: graph.user_message_count(),
                }))
            })
            .await
            .ok()
            .flatten()
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
        Self::memory_with_options_and_clock(options, Arc::new(lash_core::SystemClock)).await
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

    pub async fn save_session_head_meta(&self, meta: SessionHeadMeta) {
        if let Err(err) = self.bind_session(&meta.session_id) {
            tracing::warn!(error = %err, "failed to bind SQLite session store");
            return;
        }
        let head_json = encode_json(&meta);
        let session_id = meta.session_id.clone();
        let head_revision = meta.head_revision as i64;
        let leaf_node_id = meta.leaf_node_id.clone();
        let checkpoint_ref = meta.checkpoint_ref.as_ref().map(|value| value.0.clone());
        let result = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO session_head
                     (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        session_id,
                        head_json,
                        head_revision,
                        leaf_node_id,
                        checkpoint_ref
                    ],
                )
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "failed to persist session head");
        }
    }

    pub async fn load_session_head_meta(&self) -> Option<SessionHeadMeta> {
        let session_id = self.session_id.get()?.clone();
        self.conn
            .call(move |conn| Ok(load_session_head_meta_from_conn(conn, &session_id)))
            .await
            .ok()
            .flatten()
    }

    pub async fn save_session_head(&self, head: SessionHead) {
        if let Err(err) = self.bind_session(&head.session_id) {
            tracing::warn!(error = %err, "failed to bind SQLite session store");
            return;
        }
        let checkpoint = match head.checkpoint_ref.as_ref() {
            Some(checkpoint_ref) => match self.get_checkpoint(checkpoint_ref).await {
                Some(checkpoint) => checkpoint,
                None => {
                    tracing::warn!(
                        checkpoint_ref = %checkpoint_ref.0,
                        "cannot save a session head whose checkpoint is missing"
                    );
                    return;
                }
            },
            None => HydratedSessionCheckpoint::default(),
        };
        let graph = if head.graph.nodes.is_empty() {
            GraphCommitDelta::Unchanged {
                leaf_node_id: head.graph.leaf_node_id.clone(),
            }
        } else {
            GraphCommitDelta::Append {
                nodes: head.graph.nodes.clone(),
                leaf_node_id: head.graph.leaf_node_id.clone(),
            }
        };
        let commit = RuntimeCommit {
            session_id: head.session_id,
            expected_head_revision: Some(head.head_revision),
            session_execution_lease: None,
            release_session_execution_lease: None,
            config: head.config,
            current_frame_node_id: head.current_frame_node_id,
            graph,
            checkpoint,
            usage_deltas: head.token_ledger,
            turn_commit: None,
            completed_queue_claims: Vec::new(),
            completed_turn_input_claims: Vec::new(),
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            committed_attachment_ids: Vec::new(),
        };
        if let Err(err) = self.commit_runtime_state(commit).await {
            tracing::warn!(error = %err, "failed to persist session head");
        }
    }

    pub async fn load_session_head(&self) -> Option<SessionHead> {
        let meta = self.load_session_head_meta().await?;
        let mut graph = self.load_session_graph().await;
        graph.set_leaf_node_id(meta.leaf_node_id.clone());
        Some(SessionHead {
            session_id: meta.session_id,
            head_revision: meta.head_revision,
            current_frame_node_id: meta.current_frame_node_id,
            graph,
            config: meta.config,
            checkpoint_ref: meta.checkpoint_ref,
            token_ledger: merge_token_ledger_entries(self.load_usage_deltas().await),
        })
    }

    pub async fn save_session_meta(&self, meta: SessionMeta) {
        if let Err(err) = self.bind_session(&meta.session_id) {
            tracing::warn!(error = %err, "failed to bind SQLite session store");
            return;
        }
        let relation_json = serde_json::to_string(&meta.relation).ok();
        let session_id_for_log = meta.session_id.clone();
        let result = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO session_meta
                     (session_id, session_name, created_at, model, cwd, relation_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        meta.session_id,
                        meta.session_name,
                        meta.created_at,
                        meta.model,
                        meta.cwd,
                        relation_json
                    ],
                )
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(
                error = %err,
                session_id = session_id_for_log,
                "failed to persist session metadata"
            );
        }
    }

    pub async fn load_session_meta(&self) -> Option<SessionMeta> {
        let selected = self.session_id.get().cloned();
        let meta = self
            .conn
            .call(move |conn| {
                if let Some(session_id) = selected {
                    return Ok(load_session_meta_from_conn(conn, &session_id));
                }
                let mut stmt = conn.prepare(
                    "SELECT session_id FROM session_meta ORDER BY session_id ASC LIMIT 2",
                )?;
                let session_ids = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                if session_ids.len() == 1 {
                    Ok(load_session_meta_from_conn(conn, &session_ids[0]))
                } else {
                    Ok(None)
                }
            })
            .await
            .ok()
            .flatten();
        if let Some(meta) = &meta {
            let _ = self.bind_session(&meta.session_id);
        }
        meta
    }
}
