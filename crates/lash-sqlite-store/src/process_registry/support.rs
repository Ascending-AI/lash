use super::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

/// Which arm of the prepared append plan the store actually applied.
///
/// Entry points map this onto their own outcome type; the shared append
/// sequence never decides what a caller returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessEventAppendArm {
    /// The replay arm. No event row was written, so the wake allocation floor
    /// stays where the original insert left it. `repaired` reports whether the
    /// stale record projection was repaired from the persisted tail event.
    Replayed { repaired: bool },
    /// The insert arm. Exactly one event row was written and the wake
    /// allocation floor advanced to its sequence.
    Inserted,
}

impl ProcessEventAppendArm {
    /// Did this append rewrite the stored process projection?
    pub(crate) fn record_changed(self) -> bool {
        match self {
            Self::Replayed { repaired } => repaired,
            Self::Inserted => true,
        }
    }
}

/// Where the write authority for one process-event append is settled.
pub(crate) enum ProcessEventWriteAuthorization<'a> {
    /// The entry point authorized the write before the append sequence began.
    Preauthorized,
    /// Re-read the persisted lease and authorize against it after the
    /// replay-or-insert decision and before the first row is written.
    Lease(&'a ProcessLease),
}

pub(super) async fn recent_events(
    registry: &SqliteProcessRegistry,
    process_id: &ProcessId,
    limit: usize,
) -> Result<Vec<ProcessEvent>, lash_core_execution::PluginError> {
    let process_id = process_id.clone();
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                SqliteProcessRegistry::require_process_conn(conn, &process_id)?;
                let mut stmt = conn
                    .prepare(process_sql().event.list_recent.sql())
                    .map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(params![process_id.as_str(), limit as i64], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(process_sqlite_error)?;
                let mut events = rows
                    .map(|row| {
                        serde_json::from_str(&row.map_err(process_sqlite_error)?)
                            .map_err(process_decode_error)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                events.reverse();
                Ok(events)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) fn process_status_label(record: &ProcessRecord) -> &'static str {
    record.status.label()
}

/// The `cancel_requested_at_ms` column: the first accepted cancellation's
/// timestamp, or `NULL` when no cancel has been requested. One column carries
/// the fact and its age, so "a cancel is pending" and "it was requested at T"
/// cannot disagree.
pub(super) fn cancel_requested_at_ms(record: &ProcessRecord) -> Option<i64> {
    record
        .cancel_request
        .as_ref()
        .map(|request| request.requested_at_ms as i64)
}

#[cfg(any(test, feature = "testing"))]
pub(super) async fn wake_allocation_floor_for_testing(
    registry: &SqliteProcessRegistry,
    target_session_id: &SessionId,
    process_id: &ProcessId,
) -> Result<Option<u64>, lash_core_execution::PluginError> {
    let target_session_id = SessionId::from(target_session_id.to_string());
    let process_id = process_id.clone();
    registry
        .conn
        .call(move |conn| {
            Ok(conn
                .query_row(
                    process_sql().floor.select_floor.sql(),
                    params![target_session_id.as_str(), process_id.as_str()],
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
        process_id: &ProcessId,
    ) -> Result<u64, lash_core_execution::PluginError> {
        conn.query_row(
            process_sql().lease_sqlite.select_fencing_token.sql(),
            params![process_id.as_str()],
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
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
        if let Some(record) = Self::load_process_conn(conn, process_id)? {
            return Ok(record);
        }
        let tombstone = conn
            .query_row(
                process_sql().tombstone.select_terminal.sql(),
                params![process_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        Err(registry_transitions::absent_process_error(
            process_id,
            tombstone
                .map(|(terminal_label, pruned_at_ms)| {
                    Ok::<_, lash_core_execution::PluginError>(
                        registry_transitions::ProcessTombstoneStamp {
                            terminal_label,
                            pruned_at_ms: plugin_u64_from_sql(
                                "ProcessTombstone",
                                "pruned_at_ms",
                                pruned_at_ms,
                            )?,
                        },
                    )
                })
                .transpose()?,
        ))
    }

    pub(crate) async fn set_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
        add: bool,
    ) -> Result<(), lash_core_execution::PluginError> {
        let session_id = SessionId::from(session_id.to_string());
        let process_id = process_id.clone();
        let now = self.clock.timestamp_ms();
        let config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    let changed = if add {
                        tx.execute(
                            process_sql().observer_sqlite.insert_if_absent.sql(),
                            params![session_id.as_str(), process_id.as_str()],
                        )
                    } else {
                        tx.execute(
                            process_sql().observer.delete.sql(),
                            params![session_id.as_str(), process_id.as_str()],
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
        process_id: &ProcessId,
        target: Option<&str>,
    ) -> Result<(), lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let target = target.map(ToOwned::to_owned);
        let now = self.clock.timestamp_ms();
        let config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    let previous: Option<String> = tx
                        .query_row(
                            process_sql().process.select_wake_session_id.sql(),
                            params![process_id.as_str()],
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
                        process_sql().process.set_wake_session_id.sql(),
                        params![process_id.as_str(), target],
                    )
                    .map_err(process_sqlite_error)?;
                    if let Some(previous) = previous {
                        tx.execute(
                            process_sql().wake.discard_retargeted.sql(),
                            params![process_id.as_str(), previous],
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
            Arc::new(lash_core_execution::facade_support::SystemClock),
            session_store_root,
        )
        .await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn lash_core_execution::Clock>,
        session_store_root: impl Into<PathBuf>,
    ) -> tokio_rusqlite::Result<Self> {
        crate::location::validate_file_database_path(path, "SqliteProcessRegistry")?;
        Self::open_at(
            &DatabaseLocation::standalone_file(path),
            clock,
            DatabaseLocation::standalone_file(
                &session_store_root.into().join(crate::DURABLE_CORE_DB_FILE),
            ),
            #[cfg(feature = "testing")]
            None,
        )
        .await
    }

    #[cfg(feature = "testing")]
    pub async fn open_with_fault_injector_for_testing(
        path: &Path,
        session_store_root: impl Into<PathBuf>,
        fault_injector: crate::testing::SqliteFaultInjector,
    ) -> tokio_rusqlite::Result<Self> {
        crate::location::validate_file_database_path(path, "SqliteProcessRegistry")?;
        Self::open_at(
            &DatabaseLocation::standalone_file(path),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            DatabaseLocation::standalone_file(
                &session_store_root.into().join(crate::DURABLE_CORE_DB_FILE),
            ),
            Some(fault_injector),
        )
        .await
    }

    /// The registry at `location`, pruning process-owned sessions out of
    /// `process_session_catalog`.
    pub(crate) async fn open_at(
        location: &DatabaseLocation,
        clock: Arc<dyn lash_core_execution::Clock>,
        process_session_catalog: DatabaseLocation,
        #[cfg(feature = "testing")] fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        #[cfg(feature = "testing")]
        let conn = SqliteConnection::open_with_fault_injector(
            location.target(),
            SqliteConnectionPolicy::default(),
            fault_injector,
        )
        .await?;
        #[cfg(not(feature = "testing"))]
        let conn = SqliteConnection::open(location.target()).await?;
        ensure_versioned_schema(&conn, SqliteDatabase::ProcessRegistry).await?;
        apply_pragmas(&conn).await?;
        Ok(Self {
            conn,
            clock,
            process_session_catalog,
            wake_delivery_config: lash_core_execution::WakeDeliveryConfig::default(),
            scope_fence_hosts: lash_core_execution::ProcessScopeFenceHosts::default(),
            location: location.clone(),
            process_id_mint: lash_core_execution::ProcessIdMint::default(),
        })
    }

    /// Mint registered process ids from `mint` instead of at random: a fixture
    /// generator's artifacts regenerate byte-identically only when its ids do.
    #[doc(hidden)]
    pub fn with_process_id_mint_for_testing(
        mut self,
        mint: lash_core_execution::ProcessIdMint,
    ) -> Self {
        self.process_id_mint = mint;
        self
    }

    pub fn with_wake_delivery_config(
        mut self,
        config: lash_core_execution::WakeDeliveryConfig,
    ) -> Self {
        self.wake_delivery_config = config;
        self
    }

    /// The retained process registered under `start_key`, if any.
    pub(crate) fn load_process_by_start_key_conn(
        conn: &Connection,
        start_key: &lash_core_execution::StartKey,
    ) -> Result<Option<ProcessRecord>, lash_core_execution::PluginError> {
        let json: Option<String> = conn
            .query_row(
                process_sql().process.select_record_json_by_start_key.sql(),
                params![start_key.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    pub(crate) fn load_process_conn(
        conn: &Connection,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, lash_core_execution::PluginError> {
        let json: Option<String> = conn
            .query_row(
                process_sql().process.select_record_json_by_id.sql(),
                params![process_id.as_str()],
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
    ) -> Result<(), lash_core_execution::PluginError> {
        let change_seq = Self::next_change_seq_conn(conn)?;
        conn.execute(
            process_sql().process.update_mutable_columns.sql(),
            params![
                record.id.as_str(),
                record.updated_at_ms as i64,
                change_seq as i64,
                process_status_label(record),
                record.last_event_sequence as i64,
                cancel_requested_at_ms(record),
                process_encode_json(record)?,
                record
                    .park
                    .as_deref()
                    .map(|park| crate::clamp_epoch_ms(park.since_ms)),
                record
                    .park
                    .as_deref()
                    .map(|park| park.reason.code().as_str()),
                record
                    .park
                    .as_deref()
                    .and_then(|park| park.reason.retired_executable_generation_key())
            ],
        )
        .map_err(process_sqlite_error)?;
        Ok(())
    }

    pub(crate) fn next_change_seq_conn(
        conn: &Connection,
    ) -> Result<u64, lash_core_execution::PluginError> {
        conn.execute(process_sql().clock_sqlite.bump.sql(), [])
            .map_err(process_sqlite_error)?;
        conn.query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
            u64_from_sql("ProcessChangeClock", "current_seq", row.get::<_, i64>(0)?)
        })
        .map_err(process_sqlite_error)
    }

    pub(crate) fn wake_session_id_conn(
        conn: &Connection,
        process_id: &ProcessId,
    ) -> Result<Option<SessionId>, lash_core_execution::PluginError> {
        conn.query_row(
            process_sql().process.select_wake_session_id.sql(),
            params![process_id.as_str()],
            |row| row.get::<_, Option<String>>(0),
        )
        .map(|session_id| session_id.map(SessionId::from))
        .map_err(process_sqlite_error)
    }

    pub(crate) fn load_event_by_key_conn(
        conn: &Connection,
        process_id: &ProcessId,
        replay_key: &str,
    ) -> Result<Option<ProcessEvent>, lash_core_execution::PluginError> {
        let row: Option<String> = conn
            .query_row(
                process_sql().event.select_by_replay_key.sql(),
                params![process_id.as_str(), replay_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        row.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    /// The one process-event append sequence for the SQLite store.
    ///
    /// Every entry point runs these steps, in this order: replay-key lookup,
    /// wake session id, next sequence number, prepare, the replay-or-insert
    /// decision, the five-bind event insert, the process save, the parent-end
    /// retention, the wake-delivery insert, and the wake allocation floor.
    /// Entry points keep their own prologue, transaction lifetime and outcome
    /// mapping.
    ///
    /// `occurred_at_ms` is the caller's clock and the only clock this function
    /// sees; it never reads one itself. The `Lease` authorization compares that
    /// same value against the stored lease, exactly as the leased entry point
    /// did inline.
    pub(crate) fn apply_process_event_append_conn(
        conn: &Connection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
        wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
        authorization: ProcessEventWriteAuthorization<'_>,
    ) -> Result<(ProcessEventAppendReceipt, ProcessEventAppendArm), lash_core_execution::PluginError>
    {
        let process_id = record.id.clone();
        let replay_lookup =
            if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str()) {
                Self::load_event_by_key_conn(conn, &process_id, replay_key)?
            } else {
                None
            };
        let wake_session_id = Self::wake_session_id_conn(conn, &process_id)?;
        let (last_sequence, sequence) =
            Self::next_event_sequence_conn(conn, &process_id, wake_session_id.as_ref())?;
        let prepared = prepare_process_event_append(
            record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            occurred_at_ms,
            wake_session_id.as_ref(),
        )?;
        match prepared {
            lash_core_execution::facade_support::ProcessEventAppendPlan::Replay {
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
                    ProcessEventAppendReceipt {
                        last_event_sequence: record.last_event_sequence,
                        realization: lash_core_execution::StoreRealization::Coalesced,
                        event,
                        wake_delivery,
                    },
                    ProcessEventAppendArm::Replayed { repaired },
                ))
            }
            lash_core_execution::facade_support::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                wake_delivery,
            } => {
                match authorization {
                    ProcessEventWriteAuthorization::Preauthorized => {}
                    ProcessEventWriteAuthorization::Lease(lease) => {
                        // The shared process-lease verdict is the decision
                        // here (FIG-3388): the write flow's lock is already
                        // held and the release statement's predicate backstops
                        // this call.
                        let current = Self::load_process_lease_row_conn(conn, &process_id)?;
                        let verdict =
                            lash_core_execution::store_backend_support::process_lease_verdict(
                                current
                                    .as_ref()
                                    .map(registry_transitions::ProcessLeaseRow::facts),
                                lash_core_execution::store_backend_support::ProcessLeaseAuthority {
                                    lease_token: &lease.lease_token,
                                    fencing_token: lease.fencing_token,
                                },
                                occurred_at_ms,
                            );
                        if !verdict.is_current() {
                            return Err(lash_core_execution::PluginError::ProcessLeaseSuperseded {
                                process_id,
                            });
                        }
                    }
                }
                conn.execute(
                    process_sql().event.insert.sql(),
                    params![
                        process_id.as_str(),
                        sequence as i64,
                        event.event_type.as_str(),
                        event.invocation.replay_key(),
                        process_encode_json(&event)?,
                    ],
                )
                .map_err(process_sqlite_error)?;
                let park_transitions = lash_core_execution::runtime::process_park_transitions(
                    record.park.as_deref(),
                    &projected_record,
                );
                *record = projected_record;
                Self::save_process_conn(conn, record)?;
                // The park feed rides the event's own transaction (FIG-3659
                // NOW-B): a park that opened or closed here is durable in the
                // feed exactly when the fact that moved it is.
                super::park_feed::log_process_park_transitions_conn(
                    conn,
                    &record.park_key(),
                    &park_transitions,
                    occurred_at_ms,
                )?;
                // A process that just reached a terminal status is an ended
                // parent scope: its ledger row rides the same transaction as
                // the terminal append, so no child can be stranded by a crash
                // between the two.
                if record.is_terminal() {
                    super::parent_end::record_conn(
                        conn,
                        &lash_core_execution::ParentScope::process(process_id.clone()),
                        occurred_at_ms,
                    )?;
                }
                Self::insert_wake_delivery_conn(
                    conn,
                    wake_delivery.as_ref(),
                    wake_delivery_config,
                )?;
                Self::advance_wake_allocation_floor_conn(
                    conn,
                    wake_session_id.as_ref(),
                    &process_id,
                    sequence,
                )?;
                Ok((
                    ProcessEventAppendReceipt {
                        last_event_sequence: event.sequence,
                        realization: lash_core_execution::StoreRealization::Realized,
                        event,
                        wake_delivery,
                    },
                    ProcessEventAppendArm::Inserted,
                ))
            }
        }
    }

    pub(crate) fn append_event_conn(
        conn: &Connection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
        wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
    ) -> Result<(ProcessEventAppendReceipt, bool), lash_core_execution::PluginError> {
        let (receipt, arm) = Self::apply_process_event_append_conn(
            conn,
            record,
            request,
            occurred_at_ms,
            wake_delivery_config,
            ProcessEventWriteAuthorization::Preauthorized,
        )?;
        Ok((receipt, arm.record_changed()))
    }

    pub(crate) fn insert_wake_delivery_conn(
        conn: &Connection,
        wake: Option<&lash_core_execution::ProcessWakeDelivery>,
        config: lash_core_execution::WakeDeliveryConfig,
    ) -> Result<(), lash_core_execution::PluginError> {
        let Some(wake) = wake else {
            return Ok(());
        };
        let delivery = lash_core_execution::WakeDelivery::pending(wake.clone(), config)?;
        conn.execute(
            process_sql().wake_sqlite.insert_pending.sql(),
            params![
                delivery.delivery_id.as_str(),
                delivery.wake.process_id.as_str(),
                delivery.wake.target_session_id.as_str(),
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
        process_id: &ProcessId,
        target_session_id: Option<&SessionId>,
    ) -> Result<(Option<u64>, u64), lash_core_execution::PluginError> {
        let last_sequence = conn
            .query_row(
                process_sql().event.select_max_sequence.sql(),
                params![process_id.as_str()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(process_sqlite_error)?;
        let last_sequence = last_sequence
            .map(|sequence| plugin_u64_from_sql("ProcessEvent", "sequence", sequence))
            .transpose()?;
        let sender_floor = target_session_id
            .map(|target_session_id| {
                conn.query_row(
                    process_sql().floor.select_floor.sql(),
                    params![target_session_id.as_str(), process_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(process_sqlite_error)
            })
            .transpose()?
            .flatten()
            .map(|floor| plugin_u64_from_sql("WakeAllocationFloor", "allocation_floor", floor))
            .transpose()?;
        let sequence = lash_core_execution::runtime::allocate_process_event_sequence(
            last_sequence,
            sender_floor,
        )?;
        Ok((last_sequence, sequence))
    }

    pub(crate) fn advance_wake_allocation_floor_conn(
        conn: &Connection,
        target_session_id: Option<&SessionId>,
        process_id: &ProcessId,
        sequence: u64,
    ) -> Result<(), lash_core_execution::PluginError> {
        let Some(target_session_id) = target_session_id else {
            return Ok(());
        };
        conn.execute(
            process_sql().floor_sqlite.upsert_max.sql(),
            params![
                target_session_id.as_str(),
                process_id.as_str(),
                sequence as i64
            ],
        )
        .map_err(process_sqlite_error)?;
        Ok(())
    }

    /// The lease row under the write flow's lock, unprojected: the release
    /// verdict needs the raw holder columns to tell a released row
    /// (`Released`) from an absent one (`Absent`) and a held row from its
    /// successor.
    pub(crate) fn load_process_lease_row_conn(
        conn: &Connection,
        process_id: &ProcessId,
    ) -> Result<Option<registry_transitions::ProcessLeaseRow>, lash_core_execution::PluginError>
    {
        conn.query_row(
            process_sql().lease_sqlite.select_by_process.sql(),
            params![process_id.as_str()],
            |row| {
                Ok(registry_transitions::ProcessLeaseRow {
                    owner_id: row.get(0)?,
                    incarnation_id: row.get(5)?,
                    lease_token: row.get(1)?,
                    fencing_token: row.get(2)?,
                    claimed_at_ms: row.get(3)?,
                    expires_at_ms: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(process_sqlite_error)
    }

    pub(crate) fn load_process_lease_conn(
        conn: &Connection,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessLease>, lash_core_execution::PluginError> {
        Ok(Self::load_process_lease_row_conn(conn, process_id)?
            .and_then(|row| row.project(process_id)))
    }

    /// Insert-or-replace the persisted lease row for `process_id` with a fresh
    /// lease owned by `owner` at `fencing_token`.
    pub(super) fn acquire_process_lease_conn(
        conn: &Connection,
        process_id: &ProcessId,
        owner: &LeaseOwnerIdentity,
        fencing_token: u64,
        now: u64,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, lash_core_execution::PluginError> {
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
            process_sql().lease_sqlite.upsert_acquired.sql(),
            params![
                lease.process_id.as_str(),
                lease.owner.owner_id.as_str(),
                lease.owner.incarnation_id.as_str(),
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
    result: Result<T, lash_core_execution::PluginError>,
) -> TxOutcome<Result<T, lash_core_execution::PluginError>> {
    match result {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(err) => TxOutcome::Rollback(Err(err)),
    }
}

/// This registry's registration truth for a bound effect host (ADR 0049).
pub(super) struct SqliteRegistrationProbe {
    pub(super) conn: SqliteConnection,
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistrationProbe for SqliteRegistrationProbe {
    async fn process_is_registered(
        &self,
        process_id: &ProcessId,
    ) -> Result<bool, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        self.conn
            .call(move |connection| {
                connection.query_row(
                    process_sql().process.exists_by_id.sql(),
                    params![process_id.as_str()],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(process_sqlite_error)
    }
}
