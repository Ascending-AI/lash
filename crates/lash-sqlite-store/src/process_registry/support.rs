use super::*;

pub(super) fn process_status_label(record: &ProcessRecord) -> &'static str {
    record.status.label()
}

pub(super) async fn wake_allocation_floor_for_testing(
    registry: &SqliteProcessRegistry,
    target_session_id: &str,
    process_id: &str,
) -> Result<Option<u64>, lash_core::PluginError> {
    let target_session_id = target_session_id.to_string();
    let process_id = process_id.to_string();
    registry
        .conn
        .call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT allocation_floor FROM wake_allocation_floors
                     WHERE target_session_id = ?1 AND process_id = ?2",
                    params![target_session_id, process_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map(|value| {
                    value
                        .map(|value| u64_from_sql("WakeAllocationFloor", "allocation_floor", value))
                        .transpose()
                })
                .and_then(|value| value)
                .map_err(process_sqlite_error))
        })
        .await
        .map_err(process_sqlite_error)?
}

impl SqliteProcessRegistry {
    pub(crate) fn retained_process_lease_fencing_token_conn(
        conn: &Connection,
        process_id: &str,
    ) -> Result<u64, lash_core::PluginError> {
        conn.query_row(
            "SELECT lease_fencing_token FROM process_leases WHERE process_id = ?1",
            params![process_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(process_sqlite_error)?
        .map(|value| plugin_u64_from_sql("ProcessLease", "lease_fencing_token", value))
        .transpose()
        .map(|value| value.unwrap_or(0))
    }

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
        Err(registry_transitions::absent_process_error(
            process_id,
            tombstone
                .map(|(terminal_label, pruned_at_ms)| {
                    Ok::<_, lash_core::PluginError>(registry_transitions::ProcessTombstoneStamp {
                        terminal_label,
                        pruned_at_ms: plugin_u64_from_sql(
                            "ProcessTombstone",
                            "pruned_at_ms",
                            pruned_at_ms,
                        )?,
                    })
                })
                .transpose()?,
        ))
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
        Self::open_with_clock(
            path,
            Arc::new(lash_core::facade_support::SystemClock),
            session_store_root,
        )
        .await
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
        Self::memory_with_clock(Arc::new(lash_core::facade_support::SystemClock)).await
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
            |row| u64_from_sql("ProcessChangeClock", "current_seq", row.get::<_, i64>(0)?),
        )
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
    ) -> Result<Option<ProcessEvent>, lash_core::PluginError> {
        let row: Option<String> = conn
            .query_row(
                "SELECT event_json
                 FROM process_events
                 WHERE process_id = ?1 AND idempotency_key = ?2",
                params![process_id, replay_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        row.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
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
        let wake_session_id = Self::wake_session_id_conn(conn, &process_id)?;
        let (last_sequence, sequence) =
            Self::next_event_sequence_conn(conn, &process_id, wake_session_id.as_deref())?;
        let prepared = prepare_process_event_append(
            record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            occurred_at_ms,
            wake_session_id.as_deref(),
        )?;
        match prepared {
            lash_core::facade_support::ProcessEventAppendPlan::Replay {
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
            lash_core::facade_support::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                wake_delivery,
                occurred_at_ms,
            } => {
                conn.execute(
                    "INSERT INTO process_events (
                        process_id, sequence, event_type, idempotency_key,
                        occurred_at_ms, event_json
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        process_id,
                        sequence as i64,
                        event.event_type.as_str(),
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
                Self::advance_wake_allocation_floor_conn(
                    conn,
                    wake_session_id.as_deref(),
                    &process_id,
                    sequence,
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
                claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
                discard_reason, delivery_json
             ) VALUES (?1, ?2, ?3, ?4, 'pending', NULL, 0, NULL, ?5, ?6, NULL, ?7)",
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
        target_session_id: Option<&str>,
    ) -> Result<(Option<u64>, u64), lash_core::PluginError> {
        let last_sequence = conn
            .query_row(
                "SELECT MAX(sequence) FROM process_events WHERE process_id = ?1",
                params![process_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(process_sqlite_error)?;
        let last_sequence = last_sequence
            .map(|sequence| plugin_u64_from_sql("ProcessEvent", "sequence", sequence))
            .transpose()?;
        let sender_floor = target_session_id
            .map(|target_session_id| {
                conn.query_row(
                    "SELECT allocation_floor FROM wake_allocation_floors
                     WHERE target_session_id = ?1 AND process_id = ?2",
                    params![target_session_id, process_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(process_sqlite_error)
            })
            .transpose()?
            .flatten()
            .map(|floor| plugin_u64_from_sql("WakeAllocationFloor", "allocation_floor", floor))
            .transpose()?;
        let sequence =
            lash_core::runtime::allocate_process_event_sequence(last_sequence, sender_floor)?;
        Ok((last_sequence, sequence))
    }

    pub(crate) fn advance_wake_allocation_floor_conn(
        conn: &Connection,
        target_session_id: Option<&str>,
        process_id: &str,
        sequence: u64,
    ) -> Result<(), lash_core::PluginError> {
        let Some(target_session_id) = target_session_id else {
            return Ok(());
        };
        conn.execute(
            "INSERT INTO wake_allocation_floors (
                target_session_id, process_id, allocation_floor
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT (target_session_id, process_id) DO UPDATE SET
                allocation_floor = MAX(
                    wake_allocation_floors.allocation_floor,
                    excluded.allocation_floor
                )",
            params![target_session_id, process_id, sequence as i64],
        )
        .map_err(process_sqlite_error)?;
        Ok(())
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
                Ok(registry_transitions::ProcessLeaseRow {
                    owner_id: row.get(0)?,
                    incarnation_id: row.get(5)?,
                    lease_token: row.get(1)?,
                    fencing_token: row.get(2)?,
                    claimed_at_ms: row.get(3)?,
                    expires_at_ms: row.get(4)?,
                }
                .project(process_id))
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
        let lease = registry_transitions::acquired_process_lease(
            process_id,
            owner,
            fencing_token,
            now,
            lease_ttl_ms,
        );
        let sql_fencing_token = plugin_sql_monotonic_counter_value(
            "process_lease_fencing_token",
            fencing_token.saturating_sub(1),
            lease.fencing_token,
        )?;
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
                sql_fencing_token,
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
