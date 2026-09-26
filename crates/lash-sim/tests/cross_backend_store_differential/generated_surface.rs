//! The generated cross-backend store differential: a fixed operation stream
//! is applied to a SQLite file store and a PostgreSQL store, and their
//! durable storage rows are compared after every step.
//!
//! It compares storage surfaces only. Under ADR 0104 (FIG-3664) Restate is
//! the only effect engine: the PostgreSQL engine is deleted (FIG-3667) and the
//! SQLite engine is being deleted (FIG-3668), so the SQL effect-engine
//! operations — effect
//! records, tool-intent batches, await-event resolution and revocation,
//! runtime-operation journaling and retirement, and the whole effect-group
//! lifecycle — are not part of this surface.

use super::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;

use lash_conformance::{
    StoreContractHandles, StoreContractOp, StoreContractScenario, sample_store_contract_operations,
};
use lash_core::{
    AttachmentCreateMeta, AttachmentStore, MediaType, ProcessExecutionEnvRef, ProcessIdentity,
    ProcessInput, ProcessOriginator, RuntimePersistence, SessionScope, TriggerCommand,
    TriggerInputBinding, TriggerOccurrenceRequest, TriggerOwnerScope, TriggerStore,
    TriggerSubscriptionDraft,
};
use lash_s3_store::{S3AttachmentStore, S3AttachmentStoreConfig};
use lash_sqlite_store::{SqliteProcessRegistry, SqliteTriggerStore, Store as SqliteStore};

const DEFAULT_CASES: usize = 4;
const DEFAULT_SEED: u64 = 852;
const OPS_PER_CASE: usize = 55;
const SURFACE_SESSION: &str = "surface-session";
/// The session the scenario's runtime store is bound to: the runtime ops the
/// generated contract history drives all commit against it, so a turn park
/// lands in a session the history already has.
const SURFACE_RUNTIME_SESSION: &str = "prop-runtime-session";

const ALL_SURFACE_OPERATION_KINDS: &[&str] = &[
    "register",
    "first_start",
    "enter_wait",
    "clear_wait",
    "set_external_ref",
    "signal",
    "cancel_request",
    "terminal",
    "add_observer",
    "remove_observer",
    "retarget",
    "claim_lease",
    "release_lease",
    "claim_wake",
    "mark_wake",
    "discard_wake",
    "defer_wake",
    "enqueue_wake",
    "consume_wake",
    "prune",
    "compact_tombstones",
    "trigger_register",
    "trigger_disable",
    "trigger_occurrence",
    "trigger_occurrence_null_source",
    "process_signal_zero",
    "turn_park_record",
    "turn_park_load",
    "turn_park_settle",
];

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "surface", content = "operation", rename_all = "snake_case")]
enum SurfaceOperation {
    StoreContract(StoreContractOp),
    TriggerRegister {
        key: u8,
    },
    TriggerDisable {
        key: u8,
    },
    TriggerOccurrence {
        key: u8,
    },
    TriggerOccurrenceNullSource {
        key: u8,
    },
    ProcessSignalZero {
        negative: bool,
    },
    /// Park turn `key` of the runtime session with generated but valid
    /// fields (FIG-3586). One record per session: a second record replaces
    /// the first.
    TurnParkRecord {
        key: u8,
    },
    /// Read the runtime session's park back through `load_turn_park` and
    /// record the answer for the cross-backend comparison.
    TurnParkLoad,
    /// Commit turn `key` on the runtime session. A turn's commit settles its
    /// own park inside the commit's transaction and leaves another turn's
    /// (FIG-3586), so which park a settle clears is decided by the turn it
    /// names.
    TurnParkSettle {
        key: u8,
    },
}

impl SurfaceOperation {
    fn kind(&self) -> &'static str {
        match self {
            Self::StoreContract(operation) => match operation {
                StoreContractOp::Register { .. } => "register",
                StoreContractOp::FirstStart { .. } => "first_start",
                StoreContractOp::EnterWait { .. } => "enter_wait",
                StoreContractOp::ClearWait { .. } => "clear_wait",
                StoreContractOp::SetExternalRef { .. } => "set_external_ref",
                StoreContractOp::Signal { .. } => "signal",
                StoreContractOp::CancelRequest { .. } => "cancel_request",
                StoreContractOp::Terminal { .. } => "terminal",
                StoreContractOp::AddObserver { .. } => "add_observer",
                StoreContractOp::RemoveObserver { .. } => "remove_observer",
                StoreContractOp::Retarget { .. } => "retarget",
                StoreContractOp::ClaimLease { .. } => "claim_lease",
                StoreContractOp::ReleaseLease { .. } => "release_lease",
                StoreContractOp::ClaimWake => "claim_wake",
                StoreContractOp::MarkWake { .. } => "mark_wake",
                StoreContractOp::DiscardWake { .. } => "discard_wake",
                StoreContractOp::DeferWake { .. } => "defer_wake",
                StoreContractOp::EnqueueWake { .. } => "enqueue_wake",
                StoreContractOp::ConsumeWake { .. } => "consume_wake",
                StoreContractOp::Prune { .. } => "prune",
                StoreContractOp::CompactTombstones { .. } => "compact_tombstones",
            },
            Self::TriggerRegister { .. } => "trigger_register",
            Self::TriggerDisable { .. } => "trigger_disable",
            Self::TriggerOccurrence { .. } => "trigger_occurrence",
            Self::TriggerOccurrenceNullSource { .. } => "trigger_occurrence_null_source",
            Self::ProcessSignalZero { .. } => "process_signal_zero",
            Self::TurnParkRecord { .. } => "turn_park_record",
            Self::TurnParkLoad => "turn_park_load",
            Self::TurnParkSettle { .. } => "turn_park_settle",
        }
    }
}

#[path = "generated_surface/observation.rs"]
mod observation;
use observation::*;

struct SurfaceRunner {
    name: &'static str,
    scenario: StoreContractScenario,
    process_registry: Arc<dyn lash_core::ProcessRegistry>,
    trigger_store: Arc<dyn TriggerStore>,
    /// The session-bound runtime store the scenario drives; the turn-park
    /// ops apply to it directly.
    runtime: Arc<dyn RuntimePersistence>,
    /// The `load_turn_park` answers this runner observed, in operation
    /// order. Compared across every backend: each lane's runtime store is a
    /// real durable one.
    turn_park_loads: Vec<serde_json::Value>,
    reader: SurfaceReader,
}

fn surface_parked_turn_id(key: u8) -> lash_core::TurnId {
    lash_core::TurnId::from(format!("surface-parked-turn-{key}"))
}

/// One deterministic park record per key, cycling every reason shape the
/// persisted `reason_json` carries so each variant round-trips.
fn surface_turn_park(key: u8) -> lash_core::store::TurnParkWrite {
    let message = format!("surface park {key}: the journal refused replay");
    let reason = match key % 4 {
        0 => lash_core::store::ParkReason::ReplayDivergence { message },
        1 => lash_core::store::ParkReason::RetiredGeneration {
            generation: Some(lash_core::ExecutableGeneration::new(format!(
                "blake3:surface-{key}"
            ))),
            message,
        },
        2 => lash_core::store::ParkReason::BindingDrift { message },
        _ => lash_core::store::ParkReason::EffectReplayDivergence {
            effect_kind: "llm_call".to_string(),
            message,
        },
    };
    lash_core::store::TurnParkWrite {
        session_id: SessionId::from(SURFACE_RUNTIME_SESSION.to_string()),
        turn_id: surface_parked_turn_id(key),
        reason,
        at_ms: 1_000 + u64::from(key),
        engine: None,
        after_redrive: None,
    }
}

fn generated_surface_operations(seed: u64) -> Vec<SurfaceOperation> {
    let contract = sample_store_contract_operations(seed, OPS_PER_CASE - 12);
    let mut operations = vec![
        SurfaceOperation::TriggerRegister { key: 0 },
        SurfaceOperation::TriggerOccurrence { key: 0 },
    ];
    for (index, operation) in contract.into_iter().enumerate() {
        operations.push(SurfaceOperation::StoreContract(operation));
        if index == 5 {
            operations.push(SurfaceOperation::TriggerDisable { key: 0 });
        }
        // Park turn 0, read it back, then replace it with turn 1's park —
        // one record per session. Turn 0's settle leaves turn 1's park (a
        // commit clears only the park naming its own turn), and turn 1's
        // settle clears it.
        if index == 6 {
            operations.extend([
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkRecord { key: 0 },
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkRecord { key: 1 },
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkSettle { key: 0 },
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkSettle { key: 1 },
                SurfaceOperation::TurnParkLoad,
            ]);
        }
    }
    operations
}

impl SurfaceRunner {
    async fn apply(&mut self, operation: &SurfaceOperation) -> Result<(), String> {
        match operation {
            SurfaceOperation::StoreContract(operation) => self.scenario.apply(operation).await,
            SurfaceOperation::TriggerRegister { key } => {
                let subscription_key = format!("surface-{key}");
                let mut inputs = BTreeMap::new();
                inputs.insert("event".to_string(), TriggerInputBinding::Event);
                let command = TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::session(SURFACE_SESSION),
                    actor: ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                    draft: TriggerSubscriptionDraft {
                        source_capture: lash_core::TriggerSourceCapture::provider(
                            ["surface", "event"],
                            lash_core::LashSchema::any(),
                            "surface-provider",
                            serde_json::json!({"account": "surface"}),
                        ),
                        subscription_key,
                        env_ref: ProcessExecutionEnvRef::new("surface-env"),
                        wake_target: Some(SessionScope::new(SURFACE_SESSION)),
                        name: Some("surface-worker".to_string()),
                        source_type: "surface.event".to_string(),
                        source_key: format!("source-{key}"),
                        source: serde_json::json!({"source": key}),
                        payload_schema: lash_core::LashSchema::any(),
                        target: ProcessInput::Engine {
                            kind: "surface".to_string(),
                            payload: serde_json::json!({"key": key}),
                        },
                        target_identity: ProcessIdentity::labelled(
                            "surface",
                            Some("surface-worker".to_string()),
                        ),
                        event_types: Vec::new(),
                        input_template: inputs,
                        target_label: Some("surface-worker".to_string()),
                    },
                };
                self.trigger_store
                    .execute_command(&format!("surface-register-{key}"), command)
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerDisable { key } => {
                let command = TriggerCommand::Disable {
                    owner_scope: TriggerOwnerScope::session(SURFACE_SESSION),
                    actor: ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                    subscription_key: format!("surface-{key}"),
                    expected_revision: 1,
                };
                self.trigger_store
                    .execute_command(&format!("surface-disable-{key}"), command)
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerOccurrence { key } => {
                self.trigger_store
                    .ingest_occurrence(
                        TriggerOccurrenceRequest::new(
                            "surface.event",
                            format!("source-{key}"),
                            serde_json::json!({"event": key}),
                            format!("surface-occurrence-{key}"),
                        )
                        .with_source(serde_json::json!({"source": key}))
                        .for_session(SURFACE_SESSION),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerOccurrenceNullSource { key } => {
                self.trigger_store
                    .ingest_occurrence(
                        TriggerOccurrenceRequest::new(
                            "surface.event",
                            format!("null-source-{key}"),
                            serde_json::json!({"event": key}),
                            format!("surface-null-source-occurrence-{key}"),
                        )
                        .with_source(serde_json::Value::Null)
                        .for_session(SURFACE_SESSION),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::ProcessSignalZero { negative } => {
                let payload = if *negative {
                    serde_json::json!({"value": -0.0})
                } else {
                    serde_json::json!({"value": 0.0})
                };
                self.process_registry
                    .append_event(
                        &self.scenario.slot_process_id(0),
                        lash_core::ProcessEventAppendRequest::new("property.signal", payload)
                            .with_replay_key("surface-zero-replay"),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TurnParkRecord { key } => self
                .runtime
                .record_turn_park(&surface_turn_park(*key))
                .await
                .map(|_park| ())
                .map_err(|error| error.to_string()),
            SurfaceOperation::TurnParkLoad => {
                let loaded = self
                    .runtime
                    .load_turn_park(&SessionId::from(SURFACE_RUNTIME_SESSION.to_string()))
                    .await
                    .map_err(|error| error.to_string())?;
                self.turn_park_loads
                    .push(serde_json::to_value(&loaded).map_err(|error| error.to_string())?);
                Ok(())
            }
            SurfaceOperation::TurnParkSettle { key } => {
                let session = SessionId::from(SURFACE_RUNTIME_SESSION.to_string());
                let state = lash_core::store::load_persisted_session_state(self.runtime.as_ref())
                    .await
                    .map_err(|error| error.to_string())?
                    .unwrap_or_else(|| RuntimeSessionState {
                        session_id: session.clone(),
                        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                            lash_core::TurnBudget::Unbounded,
                        ))
                    });
                let commit = RuntimeCommit::persisted_state_with_operation_for_testing(
                    &state,
                    &[],
                    lash_core::store::OperationId::turn(
                        session,
                        surface_parked_turn_id(*key),
                        format!("surface-park-settle-{key}"),
                    ),
                );
                lash_core::testing::store_fixtures::commit_runtime_state_for_test(
                    &self.runtime,
                    commit,
                    "surface-park-settler",
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            }
        }
    }

    async fn observe(&self) -> SurfaceState {
        let mut state = self.reader.observe().await;
        state.turn_park_loads = self.turn_park_loads.clone();
        state
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn reset_postgres_surface(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'lash\\_%' AND tablename NOT IN ('lash_schema_versions', 'lash_catalog_identity') ORDER BY tablename").fetch_all(storage.pool()).await.unwrap();
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO lash_process_change_clock (singleton, current_seq, tombstone_compaction_horizon) VALUES (TRUE, 0, 0) ON CONFLICT (singleton) DO UPDATE SET current_seq = 0, tombstone_compaction_horizon = 0").execute(storage.pool()).await.unwrap();
    sqlx::query("INSERT INTO lash_turn_park_clock (singleton, current_seq, compaction_horizon) VALUES (TRUE, 0, 0) ON CONFLICT (singleton) DO UPDATE SET current_seq = 0, compaction_horizon = 0").execute(storage.pool()).await.unwrap();
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn surface_runners(
    root: &Path,
    storage: &PostgresStorage,
    clock: Arc<dyn Clock>,
) -> Vec<SurfaceRunner> {
    // Two runners: SQLite file and PostgreSQL, compared on their storage
    // surfaces only — the process registry, the trigger store and the
    // session-bound runtime store. The SQL effect engines are not storage
    // (ADR 0104); FIG-3667 and FIG-3668 delete them.
    let sqlite_runtime_path = root.join("runtime.db");
    let sqlite_process_path = root.join("process.db");
    let sqlite_trigger_path = root.join("trigger.db");
    let sqlite_runtime: Arc<dyn RuntimePersistence> =
        Arc::new(SqliteStore::open(&sqlite_runtime_path).await.unwrap());
    // The two registrars mint the same ids in the same order, so the
    // generated slots name the same process on both backends.
    let (sqlite_mint, postgres_mint) = super::paired_process_id_mints();
    let sqlite_registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &sqlite_process_path,
            Arc::clone(&clock),
            root.join("sessions"),
        )
        .await
        .unwrap()
        .with_process_id_mint_for_testing(sqlite_mint),
    );
    let sqlite_triggers = Arc::new(
        SqliteTriggerStore::open_with_clock(&sqlite_trigger_path, Arc::clone(&clock))
            .await
            .unwrap(),
    );

    let postgres_runtime: Arc<dyn RuntimePersistence> = Arc::new(
        storage
            .session_store("prop-runtime-session")
            .with_clock(Arc::clone(&clock)),
    );
    let postgres_registry = Arc::new(
        storage
            .process_registry()
            .with_clock(Arc::clone(&clock))
            .with_process_id_mint_for_testing(postgres_mint),
    );
    let postgres_triggers = Arc::new(storage.trigger_store());

    vec![
        SurfaceRunner {
            name: "sqlite",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: sqlite_registry.clone(),
                runtime: Arc::clone(&sqlite_runtime),
            }),
            process_registry: sqlite_registry,
            trigger_store: sqlite_triggers,
            runtime: sqlite_runtime,
            turn_park_loads: Vec::new(),
            reader: SurfaceReader::Sqlite {
                runtime_path: sqlite_runtime_path,
                process_path: sqlite_process_path,
                trigger_path: sqlite_trigger_path,
            },
        },
        SurfaceRunner {
            name: "postgres",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: postgres_registry.clone(),
                runtime: Arc::clone(&postgres_runtime),
            }),
            process_registry: postgres_registry,
            trigger_store: postgres_triggers,
            runtime: postgres_runtime,
            turn_park_loads: Vec::new(),
            reader: SurfaceReader::Postgres {
                pool: storage.pool().clone(),
            },
        },
    ]
}

fn operation_results_agree(results: &[(&str, Option<String>)]) -> bool {
    results.windows(2).all(|pair| pair[0].1 == pair[1].1)
}

#[derive(Debug)]
struct SurfaceDivergence {
    step: usize,
    operation: SurfaceOperation,
    operation_results: Vec<(&'static str, Option<String>)>,
    observations: Vec<(&'static str, SurfaceState)>,
}

async fn apply_and_observe(
    runners: &mut [SurfaceRunner],
    operation: &SurfaceOperation,
) -> (
    Vec<(&'static str, Option<String>)>,
    Vec<(&'static str, SurfaceState)>,
) {
    let mut operation_results = Vec::with_capacity(runners.len());
    for runner in runners.iter_mut() {
        operation_results.push((runner.name, Box::pin(runner.apply(operation)).await.err()));
    }
    let mut observations = Vec::with_capacity(runners.len());
    for runner in runners {
        observations.push((runner.name, runner.observe().await));
    }
    (operation_results, observations)
}

fn counterexample_path(seed: u64) -> PathBuf {
    std::env::var_os("LASH_CROSS_BACKEND_COUNTEREXAMPLE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("cross-backend-counterexamples")
        .join(format!("seed-{seed}.json"))
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn persist_counterexample(
    seed: u64,
    operations: &[SurfaceOperation],
    divergence: &SurfaceDivergence,
) -> PathBuf {
    let path = counterexample_path(seed);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "seed": seed,
            "minimal_operations": operations,
            "first_diverging_step": divergence.step,
            "operation": divergence.operation,
            "operation_results": divergence.operation_results,
            "rows": divergence.observations,
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn first_divergence(
    storage: &PostgresStorage,
    operations: &[SurfaceOperation],
) -> Option<SurfaceDivergence> {
    reset_postgres_surface(storage).await;
    let root = tempfile::tempdir().unwrap();
    let clock = Arc::new(DifferentialClock) as Arc<dyn Clock>;
    let mut runners = surface_runners(root.path(), storage, clock).await;
    for (step, operation) in operations.iter().enumerate() {
        let (operation_results, observations) =
            Box::pin(apply_and_observe(&mut runners, operation)).await;
        if !operation_results_agree(&operation_results) || !states_agree(&observations) {
            return Some(SurfaceDivergence {
                step: step + 1,
                operation: operation.clone(),
                operation_results,
                observations,
            });
        }
    }
    None
}

async fn minimize_diverging_prefix(
    storage: &PostgresStorage,
    operations: &[SurfaceOperation],
) -> Vec<SurfaceOperation> {
    let mut minimal = operations.to_vec();
    let mut index = 0;
    while index + 1 < minimal.len() {
        let mut candidate = minimal.clone();
        candidate.remove(index);
        if Box::pin(first_divergence(storage, &candidate))
            .await
            .is_some()
        {
            minimal = candidate;
        } else {
            index += 1;
        }
    }
    minimal
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares the SQLite and PostgreSQL stores; requires Postgres (`just cross-backend-store-soak`, or LASH_POSTGRES_DATABASE_URL with --include-ignored)"]
async fn generated_cross_backend_surface_differential_agrees() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(value) if !value.is_empty() => value,
        _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
            panic!("LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1")
        }
        _ => {
            eprintln!(
                "SKIPPED generated cross-backend surface differential: PostgreSQL is not configured"
            );
            return;
        }
    };
    let mut database_lock = PgConnection::connect(&database_url).await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .unwrap();
    // Worker open never provisions (FIG-3797): apply the committed artifact,
    // the same step `lash migrate` performs, before opening.
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut database_lock)
        .await
        .unwrap();
    let storage = PostgresStorage::connect(&database_url).await.unwrap();
    // CI seed 852 minimized to occurrence ingestion with no subscription state.
    if let Some(divergence) = Box::pin(first_divergence(
        &storage,
        &[SurfaceOperation::TriggerOccurrence { key: 0 }],
    ))
    .await
    {
        panic!("seed-852 minimized trigger-occurrence regression diverged: {divergence:#?}");
    }
    // PR #570 seed 852 at 9eef49f32 minimized to one session-owned registration.
    if let Some(divergence) = Box::pin(first_divergence(
        &storage,
        &[SurfaceOperation::TriggerRegister { key: 0 }],
    ))
    .await
    {
        panic!("seed-852 minimized trigger-register regression diverged: {divergence:#?}");
    }
    let canonical_conflict_material = [
        SurfaceOperation::TriggerOccurrenceNullSource { key: 0 },
        SurfaceOperation::TriggerOccurrenceNullSource { key: 0 },
        SurfaceOperation::StoreContract(StoreContractOp::Register {
            process: 0,
            disposition: 0,
            max_attempts: 1,
            wake_target: None,
        }),
        SurfaceOperation::ProcessSignalZero { negative: true },
        SurfaceOperation::ProcessSignalZero { negative: false },
    ];
    if let Some(divergence) =
        Box::pin(first_divergence(&storage, &canonical_conflict_material)).await
    {
        panic!("canonical conflict-material differential diverged: {divergence:#?}");
    }
    let cases = std::env::var("LASH_CROSS_BACKEND_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| lash_sim::quick_seed_sweep(DEFAULT_CASES));
    let runner_seed = std::env::var("LASH_CROSS_BACKEND_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SEED);
    assert!(
        cases > 0,
        "LASH_CROSS_BACKEND_CASES must be greater than zero"
    );
    eprintln!(
        "cross-backend generated coverage is bounded: cases={cases} first_seed={runner_seed} \
         operations_per_case={OPS_PER_CASE}; omitted_seeds=all seeds outside the configured \
         contiguous range; backends=sqlite,postgres"
    );
    for case_index in 0..cases {
        reset_postgres_surface(&storage).await;
        let root = tempfile::tempdir().unwrap();
        let seed = runner_seed.wrapping_add(case_index as u64);
        let operations = generated_surface_operations(seed);
        let covered = operations
            .iter()
            .map(SurfaceOperation::kind)
            .collect::<BTreeSet<_>>();
        let omitted = ALL_SURFACE_OPERATION_KINDS
            .iter()
            .copied()
            .filter(|kind| !covered.contains(kind))
            .collect::<Vec<_>>();
        eprintln!(
            "cross-backend generated case seed={seed}: covered_operation_kinds={covered:?}; \
             omitted_operation_kinds={omitted:?}"
        );
        let clock = Arc::new(DifferentialClock) as Arc<dyn Clock>;
        let mut runners = surface_runners(root.path(), &storage, clock).await;
        for (step, operation) in operations.iter().enumerate() {
            let (operation_results, observations) =
                Box::pin(apply_and_observe(&mut runners, operation)).await;
            if !operation_results_agree(&operation_results) || !states_agree(&observations) {
                let observed = SurfaceDivergence {
                    step: step + 1,
                    operation: operation.clone(),
                    operation_results,
                    observations,
                };
                // Minimizing replays prefixes on every backend and can outrun
                // the test timeout, so the divergence is on record before it
                // starts.
                eprintln!(
                    "cross-backend generated case seed={seed} diverged at step={} \
                     operation={:?}; minimizing the prefix",
                    observed.step, observed.operation,
                );
                let minimal =
                    Box::pin(minimize_diverging_prefix(&storage, &operations[..=step])).await;
                // A prefix that stops reproducing is a harness defect, not a
                // clean run: say which divergence was observed and then lost,
                // so the report never hides behind a bare expect.
                let Some(minimal_divergence) = Box::pin(first_divergence(&storage, &minimal)).await
                else {
                    let path = persist_counterexample(seed, &operations[..=step], &observed);
                    panic!(
                        "cross-backend surface state diverged, but replaying the same prefix \
                         stopped diverging: the differential harness is not replay-deterministic. \
                         Observed divergence persisted to {}\nseed={seed} step={} \
                         operation={:?} operation_results={:#?} rows={:#?}",
                        path.display(),
                        observed.step,
                        observed.operation,
                        observed.operation_results,
                        observed.observations,
                    );
                };
                let divergence = format!(
                    "seed={seed} step={} operation={:?} operation_results={:#?} rows={:#?}",
                    minimal_divergence.step,
                    minimal_divergence.operation,
                    minimal_divergence.operation_results,
                    minimal_divergence.observations,
                );
                let path = persist_counterexample(seed, &minimal, &minimal_divergence);
                panic!(
                    "cross-backend surface state diverged; prefix-minimized reproduction persisted to {}\n{divergence}",
                    path.display()
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
enum BlobOperation {
    Put(Vec<u8>),
    DeleteFirst,
    DeleteAbsent,
}

#[tokio::test]
#[ignore = "compares file and S3 blob stores; requires a live S3 server (`scripts/ci/with-service.sh s3`, or LASH_REQUIRE_S3=1 with --include-ignored)"]
async fn attachment_blob_store_differential_agrees() {
    if std::env::var("LASH_REQUIRE_S3").as_deref() != Ok("1") {
        eprintln!("SKIPPED attachment blob-store differential: LASH_REQUIRE_S3 is not set");
        return;
    }
    let memory_backend = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let memory = memory_backend.attachment_store();
    let root = tempfile::tempdir().unwrap();
    let file = lash_core::facade_support::FileAttachmentStore::new(root.path());
    // The S3 server this runs against is named by the same LASH_S3_* settings
    // the lash-s3-store suite reads (`scripts/ci/s3-service.sh` owns them), so
    // no literal here pins the endpoint or the credentials to one deployment.
    let required = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("LASH_REQUIRE_S3=1 requires {name}"))
    };
    let s3 = S3AttachmentStore::from_config(S3AttachmentStoreConfig {
        endpoint_url: Some(required("LASH_S3_ENDPOINT")),
        region: std::env::var("LASH_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
        bucket: std::env::var("LASH_S3_BUCKET").unwrap_or_else(|_| "lash-attachments".to_string()),
        prefix: Some(format!("cross-backend/{}", run_nonce())),
        access_key_id: Some(required("LASH_S3_ACCESS_KEY")),
        secret_access_key: Some(required("LASH_S3_SECRET_KEY").into()),
        path_style: true,
    })
    .unwrap();
    let operations = [
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::Put(vec![9, 8]),
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::DeleteFirst,
        BlobOperation::DeleteAbsent,
    ];
    eprintln!(
        "attachment blob differential coverage is bounded: operations={operations:?}; \
         backends=sqlite-memory,file,s3; omitted_operations=all other byte sequences and operation \
         sequences"
    );
    let mut first_id = None;
    for operation in &operations {
        match operation {
            BlobOperation::Put(bytes) => {
                let meta = || {
                    AttachmentCreateMeta::new(
                        MediaType::parse("application/octet-stream").unwrap(),
                        None,
                        Some("surface".to_string()),
                    )
                };
                let memory_ref = memory.put(bytes.clone(), meta()).await.unwrap();
                let file_ref = file.put(bytes.clone(), meta()).await.unwrap();
                let s3_ref = s3.put(bytes.clone(), meta()).await.unwrap();
                assert_eq!(memory_ref.id, file_ref.id);
                assert_eq!(file_ref.id, s3_ref.id);
                first_id.get_or_insert(memory_ref.id);
            }
            BlobOperation::DeleteFirst => {
                let id = first_id.as_ref().unwrap();
                memory.delete(id).await.unwrap();
                file.delete(id).await.unwrap();
                s3.delete(id).await.unwrap();
            }
            BlobOperation::DeleteAbsent => {
                let id = lash_core::AttachmentId::parse("absent").expect("valid attachment id");
                memory.delete(&id).await.unwrap();
                file.delete(&id).await.unwrap();
                s3.delete(&id).await.unwrap();
            }
        }
        let memory_rows = raw_sqlite_blobs(
            &memory_backend.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
        );
        let file_rows = raw_file_blobs(root.path());
        let s3_rows = s3.raw_blobs_for_testing().await.unwrap();
        assert_eq!(
            memory_rows, file_rows,
            "SQLite memory/file attachment blobs diverged after {operation:?}"
        );
        assert_eq!(
            file_rows, s3_rows,
            "file/S3 attachment blobs diverged after {operation:?}"
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn raw_sqlite_blobs(database_uri: &str) -> Vec<(lash_core::AttachmentId, Vec<u8>)> {
    let connection = rusqlite::Connection::open(database_uri).expect("open the blob reader");
    let mut statement = connection
        .prepare("SELECT attachment_id, content FROM attachment_blobs ORDER BY attachment_id")
        .expect("prepare the blob read");
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("read the attachment blobs")
        .map(|row| {
            let (id, bytes) = row.expect("decode an attachment blob row");
            (
                lash_core::AttachmentId::parse(id).expect("valid attachment id"),
                bytes,
            )
        })
        .collect()
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn raw_file_blobs(root: &Path) -> Vec<(lash_core::AttachmentId, Vec<u8>)> {
    let mut rows = Vec::new();
    let content_root = root.join("blake3");
    if !content_root.exists() {
        return rows;
    }
    for prefix in fs::read_dir(content_root).unwrap() {
        for entry in fs::read_dir(prefix.unwrap().path()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.contains(".staging.") {
                rows.push((
                    lash_core::AttachmentId::parse(name).expect("valid attachment id"),
                    fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows
}
