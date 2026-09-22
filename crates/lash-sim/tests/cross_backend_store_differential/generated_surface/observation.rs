//! Observation and normalization for the generated surface differential:
//! per-backend readers over the durable tables, the normalized row shapes,
//! and the cross-backend agreement predicate.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct ProcessLeaseObservation {
    pub(super) process_id: ProcessId,
    pub(super) owner: serde_json::Value,
    pub(super) lease_token_present: bool,
    pub(super) fencing_token: u64,
    pub(super) claimed: bool,
    pub(super) ttl_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct ProcessRows {
    pub(super) records: Vec<serde_json::Value>,
    pub(super) events: Vec<serde_json::Value>,
    pub(super) observers: Vec<(SessionId, ProcessId, u64)>,
    pub(super) leases: Vec<ProcessLeaseObservation>,
    pub(super) wake_deliveries: Vec<serde_json::Value>,
    pub(super) wake_allocation_floors: Vec<(SessionId, ProcessId, u64)>,
    pub(super) tombstones: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct TriggerRows {
    pub(super) subscriptions: Vec<serde_json::Value>,
    pub(super) mutation_receipts: Vec<serde_json::Value>,
    pub(super) occurrences: Vec<serde_json::Value>,
    pub(super) deliveries: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct SurfaceState {
    pub(super) processes: ProcessRows,
    pub(super) wake_redelivery_fences: Vec<(String, String, u64)>,
    pub(super) triggers: TriggerRows,
    pub(super) effect_journal: Option<Vec<serde_json::Value>>,
    /// `runtime_effect_group`, normalized: the durable group shape and the
    /// `next_seq`/`next_commit_seq` counters the arbitration ops move.
    /// `None` on the in-memory runner, which has no durable tables.
    pub(super) effect_groups: Option<Vec<serde_json::Value>>,
    /// `runtime_effect_group_child`: the retained accepted envelopes a
    /// successor reconstructs the group's children from.
    pub(super) effect_group_children: Option<Vec<serde_json::Value>>,
    /// The outcome every grouped op recorded, in operation order. Compared
    /// SQLite vs PostgreSQL only: the in-memory runner exercises no groups.
    pub(super) group_outcomes: Vec<serde_json::Value>,
    pub(super) await_journal: Option<Vec<serde_json::Value>>,
}

pub(super) enum SurfaceReader {
    InMemory {
        runtime: Arc<InMemorySessionStore>,
        registry: Arc<TestLocalProcessRegistry>,
        triggers: Arc<InMemoryTriggerStore>,
    },
    Sqlite {
        runtime_path: PathBuf,
        process_path: PathBuf,
        trigger_path: PathBuf,
        effect_path: PathBuf,
        group_path: PathBuf,
    },
    Postgres {
        pool: PgPool,
    },
}

impl SurfaceReader {
    pub(super) async fn observe(&self) -> SurfaceState {
        match self {
            Self::InMemory {
                runtime,
                registry,
                triggers,
            } => SurfaceState {
                processes: process_rows_from_memory(registry).await,
                wake_redelivery_fences: runtime.raw_wake_redelivery_fences_for_testing(),
                triggers: trigger_rows_from_memory(triggers),
                effect_journal: None,
                effect_groups: None,
                effect_group_children: None,
                group_outcomes: Vec::new(),
                await_journal: None,
            },
            Self::Sqlite {
                runtime_path,
                process_path,
                trigger_path,
                effect_path,
                group_path,
            } => {
                let mut state = read_sqlite_surface(
                    runtime_path,
                    process_path,
                    trigger_path,
                    effect_path,
                    group_path,
                );
                state.normalize_unordered_group();
                state
            }
            Self::Postgres { pool } => {
                let mut state = read_postgres_surface(pool).await;
                state.normalize_unordered_group();
                state
            }
        }
    }

    /// Whether the `(group_scope_id, replay_key)` journal row already holds
    /// a settlement rank — the witness `EffectGroupRelease` polls so a
    /// prefix ending on a release observes a quiesced row.
    pub(super) async fn group_row_settled(&self, scope_id: &str, replay_key: &str) -> bool {
        match self {
            Self::InMemory { .. } => true,
            Self::Sqlite { group_path, .. } => {
                let connection = match rusqlite::Connection::open(group_path) {
                    Ok(connection) => connection,
                    Err(_) => return false,
                };
                connection
                    .query_row(
                        "SELECT settlement_seq FROM runtime_effect_replay
                         WHERE scope_id = ?1 AND replay_key = ?2",
                        rusqlite::params![scope_id, replay_key],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .flatten()
                    .is_some()
            }
            Self::Postgres { pool } => sqlx::query_scalar::<_, Option<i64>>(
                "SELECT settlement_seq FROM lash_runtime_effect_replay
                 WHERE scope_id = $1 AND replay_key = $2",
            )
            .bind(scope_id)
            .bind(replay_key)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten()
            .is_some(),
        }
    }
}

impl SurfaceState {
    /// Erase the scheduler-owned rank order of the unordered group from its
    /// replay rows and attach the sorted sequence sets to its group row.
    pub(super) fn normalize_unordered_group(&mut self) {
        if let (Some(journal), Some(groups)) = (&mut self.effect_journal, &mut self.effect_groups) {
            normalize_unordered_group(journal, groups);
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn process_rows_from_memory(registry: &TestLocalProcessRegistry) -> ProcessRows {
    let raw = registry.raw_state_for_testing().await;
    ProcessRows {
        records: raw
            .records
            .into_iter()
            .map(|(record, change_seq)| {
                normalized_json(serde_json::json!({"change_seq": change_seq, "record": record}))
            })
            .collect(),
        events: raw
            .events
            .into_iter()
            .map(|(process_id, event)| {
                normalized_json(serde_json::json!({"process_id": process_id, "event": event}))
            })
            .collect(),
        observers: raw.observers,
        leases: raw
            .leases
            .into_iter()
            .map(|lease| ProcessLeaseObservation {
                process_id: lease.process_id,
                lease_token_present: !lease.lease_token.is_empty(),
                owner: if lease.lease_token.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::to_value(lease.owner).expect("encode process lease owner")
                },
                fencing_token: lease.fencing_token,
                claimed: lease.claimed_at_epoch_ms != 0,
                ttl_ms: (lease.claimed_at_epoch_ms != 0).then_some(
                    lease
                        .expires_at_epoch_ms
                        .saturating_sub(lease.claimed_at_epoch_ms),
                ),
            })
            .collect(),
        wake_deliveries: raw
            .wake_deliveries
            .into_iter()
            .map(normalized_memory_wake_delivery)
            .collect(),
        wake_allocation_floors: raw.wake_allocation_floors,
        tombstones: raw
            .tombstones
            .into_iter()
            .map(|row| normalized_json(serde_json::to_value(row).expect("encode tombstone")))
            .collect(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn normalized_memory_wake_delivery(
    delivery: lash_core::WakeDelivery,
) -> serde_json::Value {
    let state = delivery.state();
    let claim_token = match &delivery.disposition {
        WakeDeliveryDisposition::Enqueuing { claim_token } => Some(claim_token.clone()),
        _ => None,
    };
    let discard_reason = delivery.disposition.discard_reason();
    let mut value = serde_json::json!({
        "delivery_id": delivery.delivery_id,
        "wake": delivery.wake,
        "state": state,
        "attempts": delivery.attempts,
        "first_attempt_ms": delivery.first_attempt_ms,
        "next_attempt_at_ms": delivery.next_attempt_at_ms,
        "expires_at_ms": delivery.expires_at_ms,
        "discard_reason": discard_reason,
    });
    if let Some(claim_token) = claim_token {
        value
            .as_object_mut()
            .expect("wake delivery projection is an object")
            .insert("claim_token".to_string(), serde_json::json!(claim_token));
    }
    normalized_json(value)
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn trigger_rows_from_memory(store: &InMemoryTriggerStore) -> TriggerRows {
    let raw = store.raw_state_for_testing();
    let mut incarnations = BTreeMap::new();
    TriggerRows {
        subscriptions: raw
            .subscriptions
            .into_iter()
            .map(|row| {
                normalized_trigger_json(serde_json::to_value(row).unwrap(), &mut incarnations)
            })
            .collect(),
        mutation_receipts: raw
            .mutation_receipts
            .into_iter()
            .map(
                |(
                    operation_id,
                    owner_kind,
                    owner_id,
                    request_fingerprint,
                    result,
                    _created_at_ms,
                )| {
                    normalized_trigger_receipt_json(
                        serde_json::json!({
                            "operation_id": operation_id,
                            "owner_kind": owner_kind,
                            "owner_id": owner_id,
                            "request_fingerprint": request_fingerprint,
                            "result": result,
                        }),
                        &mut incarnations,
                    )
                },
            )
            .collect(),
        occurrences: raw
            .occurrences
            .into_iter()
            .map(|record| {
                normalized_trigger_json(serde_json::json!({"record": record}), &mut incarnations)
            })
            .collect(),
        deliveries: raw
            .deliveries
            .into_iter()
            .map(
                |(occurrence_id, subscription_id, process_id, _created_at_ms, snapshot)| {
                    normalized_trigger_delivery_json(
                        serde_json::json!({
                            "occurrence_id": occurrence_id,
                            "subscription_id": subscription_id,
                            "process_id": process_id,
                            "subscription_snapshot": snapshot,
                        }),
                        &mut incarnations,
                    )
                },
            )
            .collect(),
    }
}

pub(super) fn normalized_json(mut value: serde_json::Value) -> serde_json::Value {
    normalize_json_fields(&mut value, None);
    value
}

pub(super) fn normalized_trigger_json(
    mut value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    normalize_json_fields(&mut value, Some(incarnations));
    value
}

pub(super) fn normalized_trigger_receipt_json(
    value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    normalized_trigger_json(value, incarnations)
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn normalized_trigger_delivery_json(
    mut value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    let fields = value
        .as_object_mut()
        .expect("trigger delivery observation must be an object");
    let occurrence_id = fields
        .get("occurrence_id")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery occurrence id");
    let subscription_id = fields
        .get("subscription_id")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery subscription id");
    let process_id = fields
        .get("process_id")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery process id");
    let snapshot = fields
        .get("subscription_snapshot")
        .and_then(serde_json::Value::as_object)
        .expect("trigger delivery subscription snapshot");
    let incarnation = snapshot
        .get("incarnation")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery subscription incarnation");
    let revision = snapshot
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .expect("trigger delivery subscription revision");
    let expected_process_id = lash_core::facade_support::deterministic_delivery_process_id(
        occurrence_id,
        subscription_id,
        incarnation,
        revision,
    )
    .expect("derive trigger delivery process id");
    let process_id_matches_derivation = process_id == expected_process_id;
    fields.remove("process_id");
    fields.insert(
        "process_id_matches_derivation".to_string(),
        serde_json::Value::Bool(process_id_matches_derivation),
    );
    normalized_trigger_json(value, incarnations)
}

pub(super) fn normalize_json_fields(
    value: &mut serde_json::Value,
    mut incarnations: Option<&mut BTreeMap<String, String>>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_json_fields(value, incarnations.as_deref_mut());
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                match name.as_str() {
                    "created_at_ms"
                    | "updated_at_ms"
                    | "occurred_at_ms"
                    | "deleted_at_ms"
                    | "pruned_at_ms"
                    | "first_attempt_ms"
                    | "next_attempt_at_ms"
                    | "expires_at_ms"
                    | "created_at_epoch_ms"
                    | "updated_at_epoch_ms"
                    | "claimed_at_epoch_ms"
                    | "expires_at_epoch_ms"
                    | "resolved_at_ms"
                    | "lease_expires_at_ms"
                    | "due_at_ms"
                    | "occurred_at" => {
                        if !value.is_null() {
                            *value = serde_json::json!("normalized_timestamp");
                        }
                    }
                    "claim_token" | "lease_token" | "lease_owner_id" => {
                        *value = serde_json::Value::Bool(!value.is_null());
                    }
                    "incarnation" | "subscription_incarnation" => {
                        if let (Some(raw), Some(map)) =
                            (value.as_str(), incarnations.as_deref_mut())
                        {
                            let next = map.len();
                            let alias = map
                                .entry(raw.to_string())
                                .or_insert_with(|| format!("incarnation-{next}"))
                                .clone();
                            *value = serde_json::Value::String(alias);
                        }
                    }
                    _ => normalize_json_fields(value, incarnations.as_deref_mut()),
                }
            }
        }
        _ => {}
    }
}

/// The replay-row read every grouped surface shares: the pre-FIG-3471
/// columns plus the durable arbitration and rank columns (`group_key`,
/// `settlement_seq`, `commit_state`, `commit_seq`, `drain_input`). The
/// SQLite journal spans `effect.db` and `groups.db`, so this statement is
/// deliberately unordered; the union is sorted in memory.
pub(crate) const SQLITE_EFFECT_REPLAY_READ: &str =
    "SELECT scope_id, session_id, replay_key, envelope_hash, envelope_json, status,
            outcome_json, error_json, lease_owner_id, lease_token,
            lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
            commit_state, commit_seq, drain_input
     FROM runtime_effect_replay";

/// `runtime_effect_group`: the durable group shape and the `next_seq` /
/// `next_commit_seq` counters the arbitration ops move. `created_at_ms` is a
/// wall clock and is not compared.
pub(crate) const SQLITE_EFFECT_GROUP_READ: &str =
    "SELECT group_key, scope_id, session_id, wake, loser_disposition,
            expected_children, next_seq, next_commit_seq, lifecycle
     FROM runtime_effect_group ORDER BY group_key";

/// `runtime_effect_group_child`: the retained accepted envelopes a successor
/// reconstructs the group's children from.
pub(crate) const SQLITE_EFFECT_GROUP_CHILD_READ: &str =
    "SELECT group_key, position, replay_key, envelope_json, command_version
     FROM runtime_effect_group_child ORDER BY group_key, position";

pub(crate) const POSTGRES_EFFECT_REPLAY_READ: &str =
    "SELECT scope_id, session_id, replay_key, envelope_hash, envelope_json, status,
            outcome_json, error_json, lease_owner_id, lease_token,
            lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
            commit_state, commit_seq, drain_input
     FROM lash_runtime_effect_replay ORDER BY scope_id, replay_key";

pub(crate) const POSTGRES_EFFECT_GROUP_READ: &str =
    "SELECT group_key, scope_id, session_id, wake, loser_disposition,
            expected_children, next_seq, next_commit_seq, lifecycle
     FROM lash_runtime_effect_group ORDER BY group_key";

pub(crate) const POSTGRES_EFFECT_GROUP_CHILD_READ: &str =
    "SELECT group_key, position, replay_key, envelope_json, command_version
     FROM lash_runtime_effect_group_child ORDER BY group_key, position";

/// One normalized replay row. Rows that are not `completed` may still hold a
/// live lease, so their lease columns compare as presence facts — `leased`
/// for the expiry, and the owner/token booleans `normalized_json` already
/// produces — while completed rows keep the pre-FIG-3471 normalization.
#[expect(
    clippy::expect_used,
    reason = "test support: reader SQL only produces object rows; a non-object is a harness defect"
)]
pub(super) fn normalize_effect_journal_row(mut row: serde_json::Value) -> serde_json::Value {
    let completed = row.get("status").and_then(serde_json::Value::as_str) == Some("completed");
    if !completed {
        let fields = row.as_object_mut().expect("a replay row is an object");
        let leased = fields
            .get("lease_expires_at_ms")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|value| value != 0);
        fields.remove("lease_expires_at_ms");
        fields.insert("leased".to_string(), serde_json::Value::Bool(leased));
    }
    normalized_json(row)
}

/// The two-concurrent-finals group decides its ranks by scheduler order, so
/// the comparison erases its per-row sequence values and attaches the sorted
/// `commit_seq`/`settlement_seq` sets to the group row instead.
#[expect(
    clippy::expect_used,
    reason = "test support: reader SQL only produces object rows; a non-object is a harness defect"
)]
pub(super) fn normalize_unordered_group(
    journal: &mut [serde_json::Value],
    groups: &mut [serde_json::Value],
) {
    let mut commit_seqs = Vec::new();
    let mut settlement_seqs = Vec::new();
    for row in journal.iter_mut() {
        if row.get("group_key").and_then(serde_json::Value::as_str) != Some(UNORDERED_GROUP_KEY) {
            continue;
        }
        let fields = row.as_object_mut().expect("a replay row is an object");
        if let Some(seq) = fields.get("commit_seq").and_then(serde_json::Value::as_u64) {
            commit_seqs.push(seq);
        }
        fields.insert("commit_seq".to_string(), serde_json::Value::Null);
        if let Some(seq) = fields
            .get("settlement_seq")
            .and_then(serde_json::Value::as_u64)
        {
            settlement_seqs.push(seq);
        }
        fields.insert("settlement_seq".to_string(), serde_json::Value::Null);
    }
    commit_seqs.sort_unstable();
    settlement_seqs.sort_unstable();
    for row in groups.iter_mut() {
        if row.get("group_key").and_then(serde_json::Value::as_str) != Some(UNORDERED_GROUP_KEY) {
            continue;
        }
        let fields = row.as_object_mut().expect("a group row is an object");
        fields.insert("commit_seq_set".to_string(), serde_json::json!(commit_seqs));
        fields.insert(
            "settlement_seq_set".to_string(),
            serde_json::json!(settlement_seqs),
        );
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn read_sqlite_effect_journal(
    connection: &rusqlite::Connection,
) -> Vec<serde_json::Value> {
    sqlite_simple_json_rows(connection, SQLITE_EFFECT_REPLAY_READ, |row| {
        let envelope: String = row.get(4)?;
        Ok(normalize_effect_journal_row(serde_json::json!({
            "scope_id": row.get::<_, String>(0)?,
            "session_id": row.get::<_, Option<String>>(1)?,
            "replay_key": row.get::<_, String>(2)?,
            "envelope_hash": row.get::<_, String>(3)?,
            "envelope": serde_json::from_str::<serde_json::Value>(&envelope).unwrap(),
            "status": row.get::<_, String>(5)?,
            "outcome": row.get::<_, Option<String>>(6)?.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
            "error": row.get::<_, Option<String>>(7)?.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
            "lease_owner_id": row.get::<_, Option<String>>(8)?,
            "lease_token": row.get::<_, Option<String>>(9)?,
            "lease_expires_at_ms": row.get::<_, i64>(10)?,
            "due_at_ms": row.get::<_, Option<i64>>(11)?,
            "group_key": row.get::<_, Option<String>>(12)?,
            "settlement_seq": row.get::<_, Option<i64>>(13)?,
            "commit_state": row.get::<_, String>(14)?,
            "commit_seq": row.get::<_, Option<i64>>(15)?,
            "drain_input": row.get::<_, Option<String>>(16)?,
        })))
    })
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn read_sqlite_effect_groups(
    connection: &rusqlite::Connection,
) -> Vec<serde_json::Value> {
    sqlite_simple_json_rows(connection, SQLITE_EFFECT_GROUP_READ, |row| {
        let lifecycle: String = row.get(8)?;
        Ok(normalized_json(serde_json::json!({
            "group_key": row.get::<_, String>(0)?,
            "scope_id": row.get::<_, String>(1)?,
            "session_id": row.get::<_, Option<String>>(2)?,
            "wake": row.get::<_, String>(3)?,
            "loser_disposition": row.get::<_, String>(4)?,
            "expected_children": row.get::<_, i64>(5)?,
            "next_seq": row.get::<_, i64>(6)?,
            "next_commit_seq": row.get::<_, i64>(7)?,
            "lifecycle": serde_json::from_str::<serde_json::Value>(&lifecycle).unwrap(),
        })))
    })
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn read_sqlite_effect_group_children(
    connection: &rusqlite::Connection,
) -> Vec<serde_json::Value> {
    sqlite_simple_json_rows(connection, SQLITE_EFFECT_GROUP_CHILD_READ, |row| {
        let envelope: String = row.get(3)?;
        Ok(normalized_json(serde_json::json!({
            "group_key": row.get::<_, String>(0)?,
            "position": row.get::<_, i64>(1)?,
            "replay_key": row.get::<_, String>(2)?,
            "envelope": serde_json::from_str::<serde_json::Value>(&envelope).unwrap(),
            "command_version": row.get::<_, i64>(4)?,
        })))
    })
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn read_sqlite_surface(
    runtime_path: &Path,
    process_path: &Path,
    trigger_path: &Path,
    effect_path: &Path,
    group_path: &Path,
) -> SurfaceState {
    let runtime = rusqlite::Connection::open(runtime_path).expect("open SQLite runtime reader");
    let process = rusqlite::Connection::open(process_path).expect("open SQLite process reader");
    let trigger = rusqlite::Connection::open(trigger_path).expect("open SQLite trigger reader");
    let effect = rusqlite::Connection::open(effect_path).expect("open SQLite effect reader");
    let groups = rusqlite::Connection::open(group_path).expect("open SQLite group reader");
    let records = sqlite_simple_json_rows(
        &process,
        "SELECT record_json, change_seq FROM processes ORDER BY process_id",
        |row| {
            let record: String = row.get(0)?;
            Ok(normalized_json(serde_json::json!({
                "change_seq": row.get::<_, i64>(1)?,
                "record": serde_json::from_str::<serde_json::Value>(&record).unwrap(),
            })))
        },
    );
    let events = sqlite_simple_json_rows(
        &process,
        "SELECT process_id, event_json FROM process_events ORDER BY process_id, sequence",
        |row| {
            let event: String = row.get(1)?;
            Ok(normalized_json(serde_json::json!({
                "process_id": row.get::<_, String>(0)?,
                "event": serde_json::from_str::<serde_json::Value>(&event).unwrap(),
            })))
        },
    );
    let observers = {
        let mut stmt = process
            .prepare("SELECT session_id, process_id, process_incarnation FROM process_observers ORDER BY session_id, process_id, process_incarnation")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                SessionId::from(row.get::<_, String>(0)?),
                ProcessId::from(row.get::<_, String>(1)?),
                u64::try_from(row.get::<_, i64>(2)?).expect("non-negative process incarnation"),
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let leases = {
        let mut stmt = process
            .prepare(
                "SELECT process_id, lease_owner_id, lease_owner_incarnation_id,
                    lease_token, lease_fencing_token, lease_claimed_at_ms,
                    lease_expires_at_ms
             FROM process_leases ORDER BY process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            let owner_id: Option<String> = row.get(1)?;
            let incarnation_id: Option<String> = row.get(2)?;
            let claimed: i64 = row.get(5)?;
            let expires: i64 = row.get(6)?;
            Ok(ProcessLeaseObservation {
                process_id: ProcessId::from(row.get::<_, String>(0)?),
                lease_token_present: row.get::<_, Option<String>>(3)?.is_some(),
                owner: if row.get::<_, Option<String>>(3)?.is_some() {
                    serde_json::to_value(decode_lease_owner(owner_id, incarnation_id)).unwrap()
                } else {
                    serde_json::Value::Null
                },
                fencing_token: row.get::<_, i64>(4)? as u64,
                claimed: claimed != 0,
                ttl_ms: (claimed != 0).then_some((expires - claimed) as u64),
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let wake_deliveries = sqlite_simple_json_rows(
        &process,
        "SELECT delivery_id, delivery_json, state, claim_token, attempts, first_attempt_ms,
                next_attempt_at_ms, expires_at_ms, discard_reason
         FROM process_wake_deliveries ORDER BY delivery_id",
        |row| {
            let json: String = row.get(1)?;
            let wake: serde_json::Value = serde_json::from_str(&json).unwrap();
            let mut value = serde_json::json!({
                "delivery_id": row.get::<_, String>(0)?,
                "wake": wake,
            });
            let fields = value.as_object_mut().unwrap();
            fields.insert(
                "state".to_string(),
                serde_json::json!(row.get::<_, String>(2)?),
            );
            if let Some(token) = row.get::<_, Option<String>>(3)? {
                fields.insert("claim_token".to_string(), serde_json::json!(token));
            } else {
                fields.remove("claim_token");
            }
            fields.insert(
                "attempts".to_string(),
                serde_json::json!(row.get::<_, i64>(4)?),
            );
            fields.insert(
                "first_attempt_ms".to_string(),
                serde_json::json!(row.get::<_, Option<i64>>(5)?),
            );
            fields.insert(
                "next_attempt_at_ms".to_string(),
                serde_json::json!(row.get::<_, i64>(6)?),
            );
            fields.insert(
                "expires_at_ms".to_string(),
                serde_json::json!(row.get::<_, i64>(7)?),
            );
            fields.insert(
                "discard_reason".to_string(),
                serde_json::json!(row.get::<_, Option<String>>(8)?),
            );
            Ok(normalized_json(value))
        },
    );
    let wake_allocation_floors = {
        let mut stmt = process
            .prepare(
                "SELECT target_session_id, process_id, allocation_floor
                 FROM wake_allocation_floors ORDER BY target_session_id, process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                SessionId::from(row.get::<_, String>(0)?),
                ProcessId::from(row.get::<_, String>(1)?),
                row.get::<_, i64>(2)? as u64,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let tombstones = sqlite_simple_json_rows(
        &process,
        "SELECT process_id, incarnation, terminal_label, pruned_at_ms, pruned_change_seq
         FROM process_tombstones ORDER BY process_id, incarnation",
        |row| {
            Ok(normalized_json(serde_json::json!({
                "process_id": row.get::<_, String>(0)?,
                "incarnation": row.get::<_, i64>(1)?,
                "terminal_label": row.get::<_, String>(2)?,
                "pruned_at_ms": row.get::<_, i64>(3)?,
                "pruned_change_seq": row.get::<_, i64>(4)?,
            })))
        },
    );
    let wake_redelivery_fences = {
        let mut stmt = runtime
            .prepare(
                "SELECT session_id, process_id, allocation_floor
             FROM wake_redelivery_fences ORDER BY session_id, process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            leases,
            wake_deliveries,
            wake_allocation_floors,
            tombstones,
        },
        wake_redelivery_fences,
        triggers: read_sqlite_triggers(&trigger),
        effect_journal: Some({
            // The ungrouped journal lives in `effect.db`, the grouped
            // children's journal in `groups.db`; the surface reads the union
            // in `(scope_id, replay_key)` order.
            let mut journal = read_sqlite_effect_journal(&effect);
            journal.extend(read_sqlite_effect_journal(&groups));
            journal.sort_by(|left, right| {
                (
                    left["scope_id"].as_str().unwrap_or_default(),
                    left["replay_key"].as_str().unwrap_or_default(),
                )
                    .cmp(&(
                        right["scope_id"].as_str().unwrap_or_default(),
                        right["replay_key"].as_str().unwrap_or_default(),
                    ))
            });
            journal
        }),
        effect_groups: Some(read_sqlite_effect_groups(&groups)),
        effect_group_children: Some(read_sqlite_effect_group_children(&groups)),
        group_outcomes: Vec::new(),
        await_journal: Some(read_sqlite_await(&effect, &process)),
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn sqlite_simple_json_rows<F>(
    connection: &rusqlite::Connection,
    query: &str,
    decode: F,
) -> Vec<serde_json::Value>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value>,
{
    let mut stmt = connection.prepare(query).unwrap();
    stmt.query_map([], decode)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn read_sqlite_triggers(connection: &rusqlite::Connection) -> TriggerRows {
    let mut incarnations = BTreeMap::new();
    let subscriptions = sqlite_simple_json_rows(
        connection,
        "SELECT record_json FROM trigger_subscriptions ORDER BY subscription_id",
        |row| {
            let json: String = row.get(0)?;
            Ok(serde_json::from_str(&json).unwrap())
        },
    )
    .into_iter()
    .map(|row| normalized_trigger_json(row, &mut incarnations))
    .collect();
    let mutation_receipts = sqlite_simple_json_rows(connection, "SELECT operation_id, owner_kind, owner_id, request_fingerprint, result_json FROM trigger_mutation_receipts ORDER BY operation_id", |row| {
        let result: String = row.get(4)?;
        Ok(serde_json::json!({"operation_id": row.get::<_, String>(0)?, "owner_kind": row.get::<_, String>(1)?, "owner_id": row.get::<_, String>(2)?, "request_fingerprint": row.get::<_, String>(3)?, "result": serde_json::from_str::<serde_json::Value>(&result).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_receipt_json(row, &mut incarnations)).collect();
    let occurrences = sqlite_simple_json_rows(connection, "SELECT record_json FROM trigger_occurrences ORDER BY occurrence_id", |row| {
        let record: String = row.get(0)?;
        Ok(serde_json::json!({"record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_json(row, &mut incarnations)).collect();
    let deliveries = sqlite_simple_json_rows(connection, "SELECT occurrence_id, subscription_id, process_id, subscription_snapshot_json FROM trigger_deliveries ORDER BY occurrence_id, subscription_id", |row| {
        let snapshot: String = row.get(3)?;
        Ok(serde_json::json!({"occurrence_id": row.get::<_, String>(0)?, "subscription_id": row.get::<_, String>(1)?, "process_id": row.get::<_, String>(2)?, "subscription_snapshot": serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_delivery_json(row, &mut incarnations)).collect();
    TriggerRows {
        subscriptions,
        mutation_receipts,
        occurrences,
        deliveries,
    }
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn read_sqlite_await(
    connection: &rusqlite::Connection,
    process_registry: &rusqlite::Connection,
) -> Vec<serde_json::Value> {
    let mut rows = sqlite_simple_json_rows(
        connection,
        "SELECT key_id, scope_json, wait_json, session_id, turn_control, terminal_json, resolved_at_ms FROM await_event_waits ORDER BY key_id",
        |row| {
            let scope: String = row.get(1)?;
            let wait: String = row.get(2)?;
            let terminal: Option<String> = row.get(5)?;
            Ok(normalized_json(serde_json::json!({
                "kind": "wait", "key_id": row.get::<_, String>(0)?,
                "scope": serde_json::from_str::<serde_json::Value>(&scope).unwrap(),
                "wait": serde_json::from_str::<serde_json::Value>(&wait).unwrap(),
                "session_id": row.get::<_, Option<String>>(3)?,
                "turn_control": row.get::<_, i64>(4)? != 0,
                "terminal": terminal.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
                "resolved_at_ms": row.get::<_, Option<i64>>(6)?,
            })))
        },
    );
    rows.extend(sqlite_simple_json_rows(connection, "SELECT session_id FROM await_event_revoked_sessions ORDER BY session_id", |row| {
        Ok(serde_json::json!({"kind": "revoked_session", "session_id": row.get::<_, String>(0)?}))
    }));
    // A scope fence is one row in one of the two SQLite files — the journal
    // for runtime operations and unbound process scopes, the registry for a
    // registered process (ADR 0049) — while PostgreSQL holds them in one
    // table; the surface reads the union in one order.
    let mut fences: Vec<String> = Vec::new();
    for reader in [connection, process_registry] {
        fences.extend(
            sqlite_simple_json_rows(
                reader,
                "SELECT scope_id FROM effect_scope_retirements ORDER BY scope_id",
                |row| Ok(serde_json::Value::String(row.get::<_, String>(0)?)),
            )
            .into_iter()
            .map(|value| value.as_str().expect("scope id").to_string()),
        );
    }
    fences.sort();
    fences.dedup();
    rows.extend(
        fences
            .into_iter()
            .map(|scope_id| serde_json::json!({"kind": "retired_scope", "scope_id": scope_id})),
    );
    rows
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_surface(pool: &PgPool) -> SurfaceState {
    let record_rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT record_json, change_seq FROM lash_processes ORDER BY process_id")
            .fetch_all(pool)
            .await
            .unwrap();
    let records = record_rows.into_iter().map(|(record, change_seq)| normalized_json(serde_json::json!({"change_seq": change_seq, "record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}))).collect();
    let event_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT process_id, event_json FROM lash_process_events ORDER BY process_id, sequence",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let events = event_rows.into_iter().map(|(process_id, event)| normalized_json(serde_json::json!({"process_id": process_id, "event": serde_json::from_str::<serde_json::Value>(&event).unwrap()}))).collect();
    let observers = sqlx::query_as(
        "SELECT session_id, process_id, process_incarnation FROM lash_process_observers ORDER BY session_id, process_id, process_incarnation",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(session_id, process_id, incarnation): (String, String, i64)| {
        (
            SessionId::from(session_id),
            ProcessId::from(process_id),
            u64::try_from(incarnation).expect("non-negative process incarnation"),
        )
    })
    .collect();
    type PgLeaseRow = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        i64,
    );
    let lease_rows: Vec<PgLeaseRow> = sqlx::query_as("SELECT process_id, lease_owner_id, lease_owner_incarnation_id, lease_token, lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms FROM lash_process_leases ORDER BY process_id").fetch_all(pool).await.unwrap();
    let leases = lease_rows
        .into_iter()
        .map(
            |(process_id, owner_id, incarnation, token, fencing, claimed, expires)| {
                ProcessLeaseObservation {
                    process_id: ProcessId::from(process_id),
                    owner: if token.is_some() {
                        serde_json::to_value(decode_lease_owner(owner_id, incarnation)).unwrap()
                    } else {
                        serde_json::Value::Null
                    },
                    lease_token_present: token.is_some(),
                    fencing_token: fencing as u64,
                    claimed: claimed != 0,
                    ttl_ms: (claimed != 0).then_some((expires - claimed) as u64),
                }
            },
        )
        .collect();
    type PgWakeRow = (
        String,
        String,
        String,
        Option<String>,
        i64,
        Option<i64>,
        i64,
        i64,
        Option<String>,
    );
    let wake_rows: Vec<PgWakeRow> = sqlx::query_as("SELECT delivery_id, delivery_json, state, claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms, discard_reason FROM lash_process_wake_deliveries ORDER BY delivery_id").fetch_all(pool).await.unwrap();
    let wake_deliveries = wake_rows
        .into_iter()
        .map(
            |(
                delivery_id,
                json,
                state,
                token,
                attempts,
                first_attempt,
                next_attempt,
                expires,
                discard,
            )| {
                let wake: serde_json::Value = serde_json::from_str(&json).unwrap();
                let mut value = serde_json::json!({
                    "delivery_id": delivery_id,
                    "wake": wake,
                });
                let fields = value.as_object_mut().unwrap();
                fields.insert("state".to_string(), serde_json::json!(state));
                if let Some(token) = token {
                    fields.insert("claim_token".to_string(), serde_json::json!(token));
                } else {
                    fields.remove("claim_token");
                }
                fields.insert("attempts".to_string(), serde_json::json!(attempts));
                fields.insert(
                    "first_attempt_ms".to_string(),
                    serde_json::json!(first_attempt),
                );
                fields.insert(
                    "next_attempt_at_ms".to_string(),
                    serde_json::json!(next_attempt),
                );
                fields.insert("expires_at_ms".to_string(), serde_json::json!(expires));
                fields.insert("discard_reason".to_string(), serde_json::json!(discard));
                normalized_json(value)
            },
        )
        .collect();
    let allocation_rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT target_session_id, process_id, allocation_floor
         FROM lash_wake_allocation_floors ORDER BY target_session_id, process_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let wake_allocation_floors = allocation_rows
        .into_iter()
        .map(|(session, process, sequence)| {
            (
                SessionId::from(session),
                ProcessId::from(process),
                sequence as u64,
            )
        })
        .collect();
    let tombstone_rows: Vec<(String, i64, String, i64, i64)> = sqlx::query_as("SELECT process_id, incarnation, terminal_label, pruned_at_ms, pruned_change_seq FROM lash_process_tombstones ORDER BY process_id, incarnation").fetch_all(pool).await.unwrap();
    let tombstones = tombstone_rows.into_iter().map(|(process_id, incarnation, terminal_label, pruned_at_ms, pruned_change_seq)| normalized_json(serde_json::json!({"process_id": process_id, "incarnation": incarnation, "terminal_label": terminal_label, "pruned_at_ms": pruned_at_ms, "pruned_change_seq": pruned_change_seq}))).collect();
    let fence_rows: Vec<(String, String, i64)> = sqlx::query_as("SELECT session_id, process_id, allocation_floor FROM lash_wake_redelivery_fences ORDER BY session_id, process_id").fetch_all(pool).await.unwrap();
    let wake_redelivery_fences = fence_rows
        .into_iter()
        .map(|(session, process, sequence)| (session, process, sequence as u64))
        .collect();
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            leases,
            wake_deliveries,
            wake_allocation_floors,
            tombstones,
        },
        wake_redelivery_fences,
        triggers: read_postgres_triggers(pool).await,
        effect_journal: Some(read_postgres_effects(pool).await),
        effect_groups: Some(read_postgres_effect_groups(pool).await),
        effect_group_children: Some(read_postgres_effect_group_children(pool).await),
        group_outcomes: Vec::new(),
        await_journal: Some(read_postgres_await(pool).await),
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_effect_groups(pool: &PgPool) -> Vec<serde_json::Value> {
    type Row = (
        String,
        String,
        Option<String>,
        String,
        String,
        i64,
        i64,
        i64,
        serde_json::Value,
    );
    let rows: Vec<Row> = sqlx::query_as(POSTGRES_EFFECT_GROUP_READ)
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter()
        .map(
            |(
                group_key,
                scope_id,
                session_id,
                wake,
                loser_disposition,
                expected_children,
                next_seq,
                next_commit_seq,
                lifecycle,
            )| {
                normalized_json(serde_json::json!({
                    "group_key": group_key,
                    "scope_id": scope_id,
                    "session_id": session_id,
                    "wake": wake,
                    "loser_disposition": loser_disposition,
                    "expected_children": expected_children,
                    "next_seq": next_seq,
                    "next_commit_seq": next_commit_seq,
                    "lifecycle": lifecycle,
                }))
            },
        )
        .collect()
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_effect_group_children(pool: &PgPool) -> Vec<serde_json::Value> {
    type Row = (String, i64, String, String, i64);
    let rows: Vec<Row> = sqlx::query_as(POSTGRES_EFFECT_GROUP_CHILD_READ)
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter()
        .map(
            |(group_key, position, replay_key, envelope, command_version)| {
                normalized_json(serde_json::json!({
                    "group_key": group_key,
                    "position": position,
                    "replay_key": replay_key,
                    "envelope": serde_json::from_str::<serde_json::Value>(&envelope).unwrap(),
                    "command_version": command_version,
                }))
            },
        )
        .collect()
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_triggers(pool: &PgPool) -> TriggerRows {
    let mut incarnations = BTreeMap::new();
    let subscriptions: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM lash_trigger_subscriptions ORDER BY subscription_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let subscriptions = subscriptions
        .into_iter()
        .map(|row| normalized_trigger_json(serde_json::from_str(&row).unwrap(), &mut incarnations))
        .collect();
    let receipts: Vec<(String, String, String, String, String)> = sqlx::query_as("SELECT operation_id, owner_kind, owner_id, request_fingerprint, result_json FROM lash_trigger_mutation_receipts ORDER BY operation_id").fetch_all(pool).await.unwrap();
    let mutation_receipts = receipts.into_iter().map(|(operation_id, owner_kind, owner_id, request_fingerprint, result)| normalized_trigger_receipt_json(serde_json::json!({"operation_id": operation_id, "owner_kind": owner_kind, "owner_id": owner_id, "request_fingerprint": request_fingerprint, "result": serde_json::from_str::<serde_json::Value>(&result).unwrap()}), &mut incarnations)).collect();
    let occurrence_rows: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM lash_trigger_occurrences ORDER BY occurrence_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let occurrences = occurrence_rows.into_iter().map(|record| normalized_trigger_json(serde_json::json!({"record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}), &mut incarnations)).collect();
    let delivery_rows: Vec<(String, String, String, String)> = sqlx::query_as("SELECT occurrence_id, subscription_id, process_id, subscription_snapshot_json FROM lash_trigger_deliveries ORDER BY occurrence_id, subscription_id").fetch_all(pool).await.unwrap();
    let deliveries = delivery_rows.into_iter().map(|(occurrence_id, subscription_id, process_id, snapshot)| normalized_trigger_delivery_json(serde_json::json!({"occurrence_id": occurrence_id, "subscription_id": subscription_id, "process_id": process_id, "subscription_snapshot": serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()}), &mut incarnations)).collect();
    TriggerRows {
        subscriptions,
        mutation_receipts,
        occurrences,
        deliveries,
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_effects(pool: &PgPool) -> Vec<serde_json::Value> {
    let rows = sqlx::query(POSTGRES_EFFECT_REPLAY_READ)
        .fetch_all(pool)
        .await
        .unwrap();
    rows.iter()
        .map(|row| {
            use sqlx::Row as _;
            let envelope: String = row.get(4);
            let outcome: Option<String> = row.get(6);
            let error: Option<String> = row.get(7);
            normalize_effect_journal_row(serde_json::json!({
                "scope_id": row.get::<String, _>(0),
                "session_id": row.get::<Option<String>, _>(1),
                "replay_key": row.get::<String, _>(2),
                "envelope_hash": row.get::<String, _>(3),
                "envelope": serde_json::from_str::<serde_json::Value>(&envelope).unwrap(),
                "status": row.get::<String, _>(5),
                "outcome": outcome.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
                "error": error.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
                "lease_owner_id": row.get::<Option<String>, _>(8),
                "lease_token": row.get::<Option<String>, _>(9),
                "lease_expires_at_ms": row.get::<i64, _>(10),
                "due_at_ms": row.get::<Option<i64>, _>(11),
                "group_key": row.get::<Option<String>, _>(12),
                "settlement_seq": row.get::<Option<i64>, _>(13),
                "commit_state": row.get::<String, _>(14),
                "commit_seq": row.get::<Option<i64>, _>(15),
                "drain_input": row.get::<Option<String>, _>(16),
            }))
        })
        .collect()
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_await(pool: &PgPool) -> Vec<serde_json::Value> {
    type Row = (
        String,
        String,
        String,
        Option<String>,
        bool,
        Option<String>,
        Option<i64>,
    );
    let waits: Vec<Row> = sqlx::query_as("SELECT key_id, scope_json, wait_json, session_id, turn_control, terminal_json, resolved_at_ms FROM lash_await_event_waits ORDER BY key_id").fetch_all(pool).await.unwrap();
    let mut rows = waits.into_iter().map(|(key_id, scope, wait, session_id, turn_control, terminal, resolved)| normalized_json(serde_json::json!({"kind": "wait", "key_id": key_id, "scope": serde_json::from_str::<serde_json::Value>(&scope).unwrap(), "wait": serde_json::from_str::<serde_json::Value>(&wait).unwrap(), "session_id": session_id, "turn_control": turn_control, "terminal": terminal.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()), "resolved_at_ms": resolved}))).collect::<Vec<_>>();
    let revoked: Vec<String> = sqlx::query_scalar(
        "SELECT session_id FROM lash_await_event_revoked_sessions ORDER BY session_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.extend(revoked.into_iter().map(
        |session_id| serde_json::json!({"kind": "revoked_session", "session_id": session_id}),
    ));
    let retired: Vec<String> =
        sqlx::query_scalar("SELECT scope_id FROM lash_effect_scope_retirements ORDER BY scope_id")
            .fetch_all(pool)
            .await
            .unwrap();
    rows.extend(
        retired
            .into_iter()
            .map(|scope_id| serde_json::json!({"kind": "retired_scope", "scope_id": scope_id})),
    );
    rows
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn states_agree(observations: &[(&str, SurfaceState)]) -> bool {
    let common = observations.windows(2).all(|pair| {
        pair[0].1.processes == pair[1].1.processes
            && pair[0].1.wake_redelivery_fences == pair[1].1.wake_redelivery_fences
            && pair[0].1.triggers == pair[1].1.triggers
    });
    let sqlite = observations
        .iter()
        .find(|(name, _)| *name == "sqlite")
        .unwrap()
        .1
        .clone();
    let postgres = observations
        .iter()
        .find(|(name, _)| *name == "postgres")
        .unwrap()
        .1
        .clone();
    common
        && sqlite.effect_journal == postgres.effect_journal
        && sqlite.effect_groups == postgres.effect_groups
        && sqlite.effect_group_children == postgres.effect_group_children
        && sqlite.group_outcomes == postgres.group_outcomes
        && sqlite.await_journal == postgres.await_journal
}
