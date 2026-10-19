use super::*;
use lash_protocol_types::ProcessId;
use lash_protocol_types::SessionId;

/// Which arm of the prepared append plan the store actually applied.
///
/// Entry points map this onto their own outcome type; the shared append
/// sequence never decides what a caller returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessEventAppendArm {
    /// The replay arm. No event row was written. `repaired` reports whether the
    /// stale record projection was repaired from the persisted tail event.
    Replayed { repaired: bool },
    /// The insert arm. Exactly one lifecycle event row was written.
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

/// A batch of process-event appends staged against one in-memory projection
/// inside one transaction (FIG-3571), saved once by [`Self::commit`].
pub(crate) struct ProcessEventBatch {
    fleet_format: lash_core_store::store::FleetFormat,
    record_changed: bool,
    execution_changed: bool,
}

impl ProcessEventBatch {
    /// Start an empty batch whose appends stamp `fleet_format`'s versions.
    pub(crate) fn for_fleet(fleet_format: lash_core_store::store::FleetFormat) -> Self {
        Self {
            fleet_format,
            record_changed: false,
            execution_changed: false,
        }
    }

    /// Stage one preauthorized append of the batch.
    pub(crate) fn stage(
        &mut self,
        conn: &Connection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
    ) -> Result<ProcessEventAppendReceipt, lash_execution_contracts::plugin_error::PluginError>
    {
        self.stage_arm(conn, record, request, occurred_at_ms)
            .map(|(receipt, _)| receipt)
    }

    /// Stage one append of the batch under `authorization`, answering its
    /// arm.
    pub(crate) fn stage_arm(
        &mut self,
        conn: &Connection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
    ) -> Result<
        (ProcessEventAppendReceipt, ProcessEventAppendArm),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (receipt, arm) = SqliteProcessRegistry::stage_process_event_append_conn(
            conn,
            record,
            request,
            occurred_at_ms,
            self.fleet_format,
        )?;
        self.record_changed |= arm.record_changed();
        self.execution_changed |= arm.record_changed()
            && receipt.event.fact.kind()
                == lash_execution_contracts::process::ProcessEventKind::Succeeded;
        Ok((receipt, arm))
    }

    /// Save the process once if any staged append moved its projection.
    pub(crate) fn commit(
        self,
        conn: &Connection,
        record: &ProcessRecord,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        if self.record_changed {
            SqliteProcessRegistry::save_process_conn(conn, record, self.execution_changed)?;
        }
        Ok(())
    }
}

pub(super) async fn recent_events(
    registry: &SqliteProcessRegistry,
    process_id: &ProcessId,
    limit: usize,
) -> Result<Vec<ProcessEvent>, lash_execution_contracts::plugin_error::PluginError> {
    let process_id = process_id.clone();
    let fleet_format = registry.conn.fleet();
    #[cfg(feature = "testing")]
    let pauses = registry.conn.pauses();
    // Identity and tail are one snapshot: a prune between them cannot turn
    // a retained process's tail into an empty success.
    registry
        .conn
        .read(move |conn| {
            Ok((|| {
                SqliteProcessRegistry::require_process_conn(conn, &process_id)?;
                #[cfg(feature = "testing")]
                if let Some(pauses) = pauses.as_ref() {
                    pauses.reach_compound_read(
                        crate::testing::SqliteCompoundRead::RecentProcessEvents,
                    );
                }
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
                        ProcessEvent::decode(&row.map_err(process_sqlite_error)?, fleet_format)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                events.reverse();
                Ok(events)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

impl SqliteProcessRegistry {
    pub(crate) fn require_process_conn(
        conn: &rusqlite::Connection,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, lash_execution_contracts::plugin_error::PluginError> {
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
                    registry_transitions::ProcessTombstoneStamp::from_row(
                        process_id,
                        &terminal_label,
                        plugin_u64_from_sql("ProcessTombstone", "pruned_at_ms", pruned_at_ms)?,
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
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        let session_id = SessionId::parse(session_id.to_string())?;
        let process_id = process_id.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let fleet_format = tx.fleet();
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    let changed = if add {
                        crate::conn::cached_execute(
                            tx,
                            process_sql().observer_sqlite.insert_if_absent.sql(),
                            params![session_id.as_str(), process_id.as_str()],
                        )
                    } else {
                        crate::conn::cached_execute(
                            tx,
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
                        Self::append_event_conn(tx, &mut record, request, now, fleet_format)?;
                    }
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    /// Open a standalone registry on the database file at `path`, outside
    /// any store set.
    ///
    /// Test-only: hosts get their registry from a store set
    /// ([`SqliteStoreSet::process_registry`](crate::SqliteStoreSet::process_registry)).
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub async fn open_standalone_for_testing(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_standalone_with_clock_for_testing(
            path,
            Arc::new(lash_core_ids::clock::SystemClock),
        )
        .await
    }

    /// [`Self::open_standalone_for_testing`] reading time from `clock`.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub async fn open_standalone_with_clock_for_testing(
        path: &Path,
        clock: Arc<dyn lash_core_ids::clock::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        crate::location::validate_file_database_path(path, "SqliteProcessRegistry")?;
        let location = DatabaseLocation::standalone_file(path);
        let conn = SqliteConnection::open_with_policy(
            location.target(),
            crate::SqliteConnectionPolicy::standard(crate::lifecycle::FIXTURE_SYNCHRONOUS),
        )
        .await?;
        ensure_versioned_schema(&conn).await?;
        apply_pragmas(&conn).await?;
        let store = crate::SqliteStore::open_with_clock(
            path,
            crate::lifecycle::FIXTURE_SYNCHRONOUS,
            Arc::clone(&clock),
        )
        .await?;
        lash_execution_contracts::testing::process_execution_env_fixture(&store).await;
        Ok(Self::on_connection(conn, location, clock))
    }

    /// The registry over `conn`, a connection on the deployment's database
    /// whose installer has run: its writes share the one writer gate with
    /// every other table of the database.
    pub(crate) fn on_connection(
        conn: SqliteConnection,
        location: DatabaseLocation,
        clock: Arc<dyn lash_core_ids::clock::Clock>,
    ) -> Self {
        Self {
            conn,
            clock,
            location,
            process_id_mint: lash_core_store::process_identity::ProcessIdMint::default(),
            #[cfg(test)]
            decoded_roster_records: Arc::default(),
        }
    }

    /// Mint registered process ids from `mint` instead of at random: a fixture
    /// generator's artifacts regenerate byte-identically only when its ids do.
    #[doc(hidden)]
    pub fn with_process_id_mint_for_testing(
        mut self,
        mint: lash_core_store::process_identity::ProcessIdMint,
    ) -> Self {
        self.process_id_mint = mint;
        self
    }

    fn decode_process_record(
        json: &str,
    ) -> Result<ProcessRecord, lash_execution_contracts::plugin_error::PluginError> {
        serde_json::from_str(json).map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt {
                record_kind: "process_registry".to_string(),
                message: error.to_string(),
            }
        })
    }

    /// The retained process registered under `start_key`, if any.
    pub(crate) fn load_process_by_start_key_conn(
        conn: &Connection,
        start_key: &lash_core_store::process_identity::StartKey,
    ) -> Result<Option<ProcessRecord>, lash_execution_contracts::plugin_error::PluginError> {
        let json: Option<String> = conn
            .query_row(
                process_sql().process.select_record_json_by_start_key.sql(),
                params![start_key.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        json.map(|json| Self::decode_process_record(&json))
            .transpose()
    }

    pub(crate) fn load_process_conn(
        conn: &Connection,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, lash_execution_contracts::plugin_error::PluginError> {
        let json: Option<String> = conn
            .query_row(
                process_sql().process.select_record_json_by_id.sql(),
                params![process_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        json.map(|json| Self::decode_process_record(&json))
            .transpose()
    }

    pub(crate) fn save_process_conn(
        conn: &Connection,
        record: &ProcessRecord,
        execution_changed: bool,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        let change_seq = Self::next_change_seq_conn(conn)?;
        let lifecycle = process_encode_json(&record.lifecycle_storage())?;
        if execution_changed {
            crate::conn::cached_execute(
                conn,
                process_sql().process.update_execution_columns.sql(),
                params![
                    record.id.as_str(),
                    record.updated_at_ms as i64,
                    change_seq as i64,
                    lifecycle,
                    process_encode_json(&record.execution_storage())?
                ],
            )
        } else {
            crate::conn::cached_execute(
                conn,
                process_sql().process.update_mutable_columns.sql(),
                params![
                    record.id.as_str(),
                    record.updated_at_ms as i64,
                    change_seq as i64,
                    lifecycle
                ],
            )
        }
        .map_err(process_sqlite_error)?;
        Ok(())
    }

    pub(crate) fn next_change_seq_conn(
        conn: &Connection,
    ) -> Result<u64, lash_execution_contracts::plugin_error::PluginError> {
        crate::conn::cached_execute(conn, process_sql().clock_sqlite.bump.sql(), [])
            .map_err(process_sqlite_error)?;
        conn.query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
            u64_from_sql("ProcessChangeClock", "current_seq", row.get::<_, i64>(0)?)
        })
        .map_err(process_sqlite_error)
    }

    /// The event `append`'s replay key already recorded, if any. A released
    /// event comes back with `append`'s fact when it carries the released
    /// digest, and refuses as a conflict when it does not.
    pub(crate) fn load_event_by_key_conn(
        conn: &Connection,
        process_id: &ProcessId,
        append: &lash_execution_contracts::process::CanonicalProcessEventAppend,
        fleet_format: lash_core_store::store::FleetFormat,
    ) -> Result<Option<ProcessEvent>, lash_execution_contracts::plugin_error::PluginError> {
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                process_sql().event.select_by_replay_key.sql(),
                params![process_id.as_str(), append.replay_key()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(process_sqlite_error)?;
        let Some((json, released_digest)) = row else {
            return Ok(None);
        };
        let event = match released_digest {
            Some(digest) => lash_execution_contracts::process::restore_released_process_event(
                serde_json::from_str(&json).map_err(process_decode_error)?,
                &digest,
                append,
            )?,
            None => ProcessEvent::decode(&json, fleet_format)?,
        };
        Ok(Some(event))
    }

    /// One process-event append for the SQLite store: the append sequence
    /// ([`Self::stage_process_event_append_conn`]) followed by the process
    /// save when the append moved the projection.
    pub(crate) fn apply_process_event_append_conn(
        conn: &Connection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
        fleet_format: lash_core_store::store::FleetFormat,
    ) -> Result<
        (ProcessEventAppendReceipt, ProcessEventAppendArm),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (receipt, arm) = Self::stage_process_event_append_conn(
            conn,
            record,
            request,
            occurred_at_ms,
            fleet_format,
        )?;
        if arm.record_changed() {
            Self::save_process_conn(
                conn,
                record,
                receipt.event.fact.kind()
                    == lash_execution_contracts::process::ProcessEventKind::Succeeded,
            )?;
        }
        Ok((receipt, arm))
    }

    /// The one process-event append sequence for the SQLite store, short of
    /// the process save.
    ///
    /// Every entry point runs these steps, in this order: the canonical
    /// preparation, replay-key lookup, next sequence number, prepare, the replay-or-insert decision, the
    /// event insert, the projection update and parent-end retention.
    /// The caller saves the process once the projection
    /// has moved ([`ProcessEventAppendArm::record_changed`]): after this one
    /// append, or after the batch it belongs to. Entry points keep their own
    /// prologue, transaction lifetime and outcome mapping.
    ///
    /// `occurred_at_ms` is the caller's clock and the only clock this function
    /// sees; it never reads one itself.
    pub(crate) fn stage_process_event_append_conn(
        conn: &Connection,
        record: &mut ProcessRecord,
        request: ProcessEventAppendRequest,
        occurred_at_ms: u64,
        fleet_format: lash_core_store::store::FleetFormat,
    ) -> Result<
        (ProcessEventAppendReceipt, ProcessEventAppendArm),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let process_id = record.id.clone();
        let append = request.canonical(record, fleet_format)?;
        let replay_lookup = Self::load_event_by_key_conn(conn, &process_id, &append, fleet_format)?;
        let (last_sequence, sequence) = Self::next_event_sequence_conn(conn, &process_id)?;
        let prepared = prepare_process_event_append(
            record,
            append,
            sequence,
            last_sequence,
            replay_lookup,
            occurred_at_ms,
        )?;
        match prepared {
            lash_execution_contracts::process::ProcessEventAppendPlan::Replay {
                event,
                repair_record,
                ..
            } => {
                let repaired = if let Some(repaired) = repair_record {
                    *record = repaired;
                    true
                } else {
                    false
                };
                Ok((
                    ProcessEventAppendReceipt {
                        last_event_sequence: record.last_event_sequence,
                        realization: lash_execution_contracts::process::StoreRealization::Coalesced,
                        event,
                    },
                    ProcessEventAppendArm::Replayed { repaired },
                ))
            }
            lash_execution_contracts::process::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
            } => {
                crate::conn::cached_execute(
                    conn,
                    process_sql().event.insert.sql(),
                    params![
                        process_id.as_str(),
                        sequence as i64,
                        event.fact.event_type(),
                        event.invocation.effect_replay_key(),
                        process_encode_json(&event)?,
                    ],
                )
                .map_err(process_sqlite_error)?;
                *record = projected_record;
                // A process that just reached a terminal status is an ended
                // parent scope: its ledger row rides the same transaction as
                // the terminal append, so no child can be stranded by a crash
                // between the two.
                if record.is_terminal() {
                    super::parent_end::record_conn(
                        conn,
                        &lash_core_store::scope_identity::ScopeId::process(process_id.clone()),
                        occurred_at_ms,
                        fleet_format,
                    )?;
                }
                Ok((
                    ProcessEventAppendReceipt {
                        last_event_sequence: event.sequence,
                        realization: lash_execution_contracts::process::StoreRealization::Realized,
                        event,
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
        fleet_format: lash_core_store::store::FleetFormat,
    ) -> Result<
        (ProcessEventAppendReceipt, bool),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (receipt, arm) = Self::apply_process_event_append_conn(
            conn,
            record,
            request,
            occurred_at_ms,
            fleet_format,
        )?;
        Ok((receipt, arm.record_changed()))
    }

    pub(crate) fn next_event_sequence_conn(
        conn: &Connection,
        process_id: &ProcessId,
    ) -> Result<(Option<u64>, u64), lash_execution_contracts::plugin_error::PluginError> {
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
        let sequence =
            lash_execution_contracts::process::allocate_process_event_sequence(last_sequence)?;
        Ok((last_sequence, sequence))
    }
}

/// Map a `Result<T, PluginError>` produced by a synchronous transaction body to
/// a [`TxOutcome`]: commit on success, roll back on logical error. Both arms
/// carry the inner `Result` back so the caller recovers the value or the
/// `PluginError` after the transaction resolves.
pub(crate) fn tx_outcome<T>(
    result: Result<T, lash_execution_contracts::plugin_error::PluginError>,
) -> TxOutcome<Result<T, lash_execution_contracts::plugin_error::PluginError>> {
    match result {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(err) => TxOutcome::Rollback(Err(err)),
    }
}
