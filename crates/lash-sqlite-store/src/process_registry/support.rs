use super::*;

pub(super) fn process_status_label(record: &ProcessRecord) -> &'static str {
    record.status.label()
}

impl SqliteProcessRegistry {
    pub(crate) fn require_process_conn(
        conn: &rusqlite::Connection,
        process_id: &str,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        if let Some(record) = Self::load_process_conn(conn, process_id)? {
            return Ok(record);
        }
        let tombstone = conn
            .query_row(
                "SELECT terminal_label, pruned_at_ms
                 FROM process_tombstones WHERE process_id = ?1",
                params![process_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        match tombstone {
            Some((terminal_label, pruned_at_ms)) => {
                Err(lash_core::PluginError::ProcessNoLongerRetained {
                    terminal_label,
                    pruned_at_ms: pruned_at_ms as u64,
                })
            }
            None => Err(lash_core::PluginError::Session(format!(
                "unknown process `{process_id}`"
            ))),
        }
    }

    pub(crate) async fn set_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
        add: bool,
    ) -> Result<(), lash_core::PluginError> {
        let session_id = session_id.to_string();
        let process_id = process_id.to_string();
        let now = self.clock.timestamp_ms();
        let config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    let changed = if add {
                        tx.execute(
                            "INSERT OR IGNORE INTO process_observers (session_id, process_id)
                             VALUES (?1, ?2)",
                            params![session_id, process_id],
                        )
                    } else {
                        tx.execute(
                            "DELETE FROM process_observers
                             WHERE session_id = ?1 AND process_id = ?2",
                            params![session_id, process_id],
                        )
                    }
                    .map_err(process_sqlite_error)?;
                    if changed > 0 {
                        let request = if add {
                            ProcessEventAppendRequest::observer_added(&process_id, &session_id, &by)
                        } else {
                            ProcessEventAppendRequest::observer_removed(
                                &process_id,
                                &session_id,
                                &by,
                            )
                        };
                        Self::append_event_conn(tx, &mut record, request, now, config)?;
                    }
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    pub(crate) async fn retarget_subscription_impl(
        &self,
        process_id: &str,
        target: Option<&str>,
    ) -> Result<(), lash_core::PluginError> {
        let process_id = process_id.to_string();
        let target = target.map(ToOwned::to_owned);
        let now = self.clock.timestamp_ms();
        let config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    let previous: Option<String> = tx
                        .query_row(
                            "SELECT wake_session_id FROM processes WHERE process_id = ?1",
                            params![process_id],
                            |row| row.get(0),
                        )
                        .map_err(process_sqlite_error)?;
                    if previous == target {
                        return Ok(());
                    }
                    Self::append_event_conn(
                        tx,
                        &mut record,
                        ProcessEventAppendRequest::subscription_retargeted(
                            &process_id,
                            target.as_deref(),
                        ),
                        now,
                        config,
                    )?;
                    tx.execute(
                        "UPDATE processes SET wake_session_id = ?2 WHERE process_id = ?1",
                        params![process_id, target],
                    )
                    .map_err(process_sqlite_error)?;
                    if let Some(previous) = previous {
                        tx.execute(
                            "UPDATE process_wake_deliveries
                             SET state = 'discarded', discard_reason = 'retargeted'
                             WHERE process_id = ?1 AND target_session_id = ?2 AND state = 'pending'",
                            params![process_id, previous],
                        )
                        .map_err(process_sqlite_error)?;
                    }
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    /// Open a process registry whose terminal-retention prune removes the two
    /// process-owned session stores from `session_store_root` before the process
    /// row. The root is required and explicit; no sibling-directory convention
    /// is inferred.
    pub async fn open(
        path: &Path,
        session_store_root: impl Into<PathBuf>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_clock(path, Arc::new(lash_core::SystemClock), session_store_root).await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn lash_core::Clock>,
        session_store_root: impl Into<PathBuf>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_configured(path, clock, session_store_root.into()).await
    }

    async fn open_configured(
        path: &Path,
        clock: Arc<dyn lash_core::Clock>,
        process_session_store_root: PathBuf,
    ) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open(path).await?;
        ensure_process_schema(&conn).await?;
        apply_pragmas(&conn, StoreBacking::File).await?;
        Ok(Self {
            conn,
            clock,
            process_session_store_root: Some(process_session_store_root),
            wake_delivery_config: lash_core::WakeDeliveryConfig::default(),
        })
    }

    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_clock(Arc::new(lash_core::SystemClock)).await
    }

    pub async fn memory_with_clock(
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open_in_memory().await?;
        ensure_process_schema(&conn).await?;
        apply_pragmas(&conn, StoreBacking::Memory).await?;
        Ok(Self {
            conn,
            clock,
            process_session_store_root: None,
            wake_delivery_config: lash_core::WakeDeliveryConfig::default(),
        })
    }

    pub fn with_wake_delivery_config(mut self, config: lash_core::WakeDeliveryConfig) -> Self {
        self.wake_delivery_config = config;
        self
    }

    pub(crate) fn load_process_conn(
        conn: &Connection,
        process_id: &str,
    ) -> Result<Option<ProcessRecord>, lash_core::PluginError> {
        let json: Option<String> = conn
            .query_row(
                "SELECT record_json FROM processes WHERE process_id = ?1",
                params![process_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    pub(crate) fn save_process_conn(
        conn: &Connection,
        record: &ProcessRecord,
    ) -> Result<(), lash_core::PluginError> {
        let change_seq = Self::next_change_seq_conn(conn)?;
        conn.execute(
            "UPDATE processes
             SET updated_at_ms = ?2, change_seq = ?3, status = ?4,
                 is_waiting = ?5, record_json = ?6
             WHERE process_id = ?1",
            params![
                record.id.as_str(),
                record.updated_at_ms as i64,
                change_seq as i64,
                process_status_label(record),
                i64::from(record.wait.is_some()),
                process_encode_json(record)?
            ],
        )
        .map_err(process_sqlite_error)?;
        Ok(())
    }

    pub(crate) fn next_change_seq_conn(conn: &Connection) -> Result<u64, lash_core::PluginError> {
        conn.execute(
            "UPDATE process_change_clock
             SET current_seq = current_seq + 1
             WHERE singleton = 1",
            [],
        )
        .map_err(process_sqlite_error)?;
        conn.query_row(
            "SELECT current_seq FROM process_change_clock WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|seq| seq as u64)
        .map_err(process_sqlite_error)
    }

    pub(crate) fn wake_session_id_conn(
        conn: &Connection,
        process_id: &str,
    ) -> Result<Option<String>, lash_core::PluginError> {
        conn.query_row(
            "SELECT wake_session_id FROM processes WHERE process_id = ?1",
            params![process_id],
            |row| row.get(0),
        )
        .map_err(process_sqlite_error)
    }

    pub(crate) fn load_event_by_key_conn(
        conn: &Connection,
        process_id: &str,
        replay_key: &str,
    ) -> Result<Option<(String, ProcessEvent)>, lash_core::PluginError> {
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT payload_hash, event_json
                 FROM process_events
                 WHERE process_id = ?1 AND idempotency_key = ?2",
                params![process_id, replay_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        row.map(|(hash, json)| {
            serde_json::from_str(&json)
                .map(|event| (hash, event))
                .map_err(process_decode_error)
        })
        .transpose()
    }

    pub(crate) fn append_event_conn(
        conn: &Connection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
        wake_delivery_config: lash_core::WakeDeliveryConfig,
    ) -> Result<(ProcessEventAppendResult, bool), lash_core::PluginError> {
        let process_id = record.id.clone();
        let replay_lookup =
            if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str()) {
                Self::load_event_by_key_conn(conn, &process_id, replay_key)?
            } else {
                None
            };
        let sequence = Self::next_event_sequence_conn(conn, &process_id)?;
        let wake_session_id = Self::wake_session_id_conn(conn, &process_id)?;
        let prepared = prepare_process_event_append(
            record,
            request,
            sequence,
            replay_lookup,
            occurred_at_ms,
            wake_session_id.as_deref(),
        )?;
        match prepared {
            lash_core::ProcessEventAppendPlan::Replay {
                event,
                repair_record,
                wake_delivery,
                ..
            } => {
                Self::insert_wake_delivery_conn(
                    conn,
                    wake_delivery.as_ref(),
                    wake_delivery_config,
                )?;
                let repaired = if let Some(repaired) = repair_record {
                    *record = repaired;
                    Self::save_process_conn(conn, record)?;
                    true
                } else {
                    false
                };
                Ok((
                    ProcessEventAppendResult {
                        event,
                        wake_delivery,
                    },
                    repaired,
                ))
            }
            lash_core::ProcessEventAppendPlan::Insert {
                event,
                payload_hash,
                projected_record,
                wake_delivery,
                occurred_at_ms,
            } => {
                conn.execute(
                    "INSERT INTO process_events (
                        process_id, sequence, event_type, payload_hash, idempotency_key,
                        occurred_at_ms, event_json
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        process_id,
                        sequence as i64,
                        event.event_type.as_str(),
                        payload_hash.as_str(),
                        event.invocation.replay_key(),
                        occurred_at_ms as i64,
                        process_encode_json(&event)?,
                    ],
                )
                .map_err(process_sqlite_error)?;
                *record = projected_record;
                Self::save_process_conn(conn, record)?;
                Self::insert_wake_delivery_conn(
                    conn,
                    wake_delivery.as_ref(),
                    wake_delivery_config,
                )?;
                Ok((
                    ProcessEventAppendResult {
                        event,
                        wake_delivery,
                    },
                    true,
                ))
            }
        }
    }

    pub(crate) fn insert_wake_delivery_conn(
        conn: &Connection,
        wake: Option<&lash_core::ProcessWakeDelivery>,
        config: lash_core::WakeDeliveryConfig,
    ) -> Result<(), lash_core::PluginError> {
        let Some(wake) = wake else {
            return Ok(());
        };
        let delivery = lash_core::WakeDelivery::pending(wake.clone(), config)?;
        conn.execute(
            "INSERT OR IGNORE INTO process_wake_deliveries (
                delivery_id, process_id, target_session_id, sequence, state,
                attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
                discard_reason, delivery_json
             ) VALUES (?1, ?2, ?3, ?4, 'pending', 0, NULL, ?5, ?6, NULL, ?7)",
            params![
                delivery.delivery_id,
                delivery.wake.process_id,
                delivery.wake.target_session_id,
                delivery.wake.sequence as i64,
                delivery.next_attempt_at_ms as i64,
                delivery.expires_at_ms as i64,
                process_encode_json(&delivery.wake)?,
            ],
        )
        .map_err(process_sqlite_error)?;
        Ok(())
    }

    pub(crate) fn next_event_sequence_conn(
        conn: &Connection,
        process_id: &str,
    ) -> Result<u64, lash_core::PluginError> {
        conn.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM process_events WHERE process_id = ?1",
            params![process_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|sequence| sequence as u64)
        .map_err(process_sqlite_error)
    }

    pub(crate) fn load_process_lease_conn(
        conn: &Connection,
        process_id: &str,
    ) -> Result<Option<ProcessLease>, lash_core::PluginError> {
        conn.query_row(
            "SELECT lease_owner_id, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms,
                    lease_owner_incarnation_id, lease_owner_liveness_json
             FROM process_leases
             WHERE process_id = ?1",
            params![process_id],
            |row| {
                let owner_id: Option<String> = row.get(0)?;
                let lease_token: Option<String> = row.get(1)?;
                let incarnation_id: Option<String> = row.get(5)?;
                let (Some(owner_id), Some(lease_token)) = (owner_id, lease_token) else {
                    return Ok(None);
                };
                Ok(Some(ProcessLease {
                    schema_version: PROCESS_LEASE_SCHEMA_VERSION,
                    process_id: process_id.to_string(),
                    owner: process_lease_owner_from_columns(owner_id, incarnation_id),
                    lease_token,
                    fencing_token: row.get::<_, i64>(2)? as u64,
                    claimed_at_epoch_ms: row.get::<_, i64>(3)? as u64,
                    expires_at_epoch_ms: row.get::<_, i64>(4)? as u64,
                }))
            },
        )
        .optional()
        .map(|lease| lease.flatten())
        .map_err(process_sqlite_error)
    }

    /// Insert-or-replace the persisted lease row for `process_id` with a fresh
    /// lease owned by `owner` at `fencing_token`.
    pub(super) fn acquire_process_lease_conn(
        conn: &Connection,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        fencing_token: u64,
        now: u64,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, lash_core::PluginError> {
        let lease = ProcessLease {
            schema_version: PROCESS_LEASE_SCHEMA_VERSION,
            process_id: process_id.to_string(),
            owner: owner.clone(),
            lease_token: format!(
                "{:x}",
                Sha256::digest(
                    format!(
                        "{process_id}:{}:{}:{now}:{fencing_token}",
                        owner.owner_id, owner.incarnation_id
                    )
                    .as_bytes()
                )
            ),
            fencing_token,
            claimed_at_epoch_ms: now,
            expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
        };
        conn.execute(
            "INSERT INTO process_leases (
                process_id, lease_owner_id, lease_owner_incarnation_id,
                lease_owner_liveness_json, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(process_id) DO UPDATE SET
                lease_owner_id = excluded.lease_owner_id,
                lease_owner_incarnation_id = excluded.lease_owner_incarnation_id,
                lease_owner_liveness_json = excluded.lease_owner_liveness_json,
                lease_token = excluded.lease_token,
                lease_fencing_token = excluded.lease_fencing_token,
                lease_claimed_at_ms = excluded.lease_claimed_at_ms,
                lease_expires_at_ms = excluded.lease_expires_at_ms",
            params![
                lease.process_id.as_str(),
                lease.owner.owner_id.as_str(),
                lease.owner.incarnation_id.as_str(),
                Option::<&str>::None,
                lease.lease_token.as_str(),
                lease.fencing_token as i64,
                lease.claimed_at_epoch_ms as i64,
                lease.expires_at_epoch_ms as i64,
            ],
        )
        .map_err(process_sqlite_error)?;
        Ok(lease)
    }
}

/// Map a `Result<T, PluginError>` produced by a synchronous transaction body to
/// a [`TxOutcome`]: commit on success, roll back on logical error. Both arms
/// carry the inner `Result` back so the caller recovers the value or the
/// `PluginError` after the transaction resolves.
pub(crate) fn tx_outcome<T>(
    result: Result<T, lash_core::PluginError>,
) -> TxOutcome<Result<T, lash_core::PluginError>> {
    match result {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(err) => TxOutcome::Rollback(Err(err)),
    }
}
