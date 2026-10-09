//! The generated cross-backend store differential: a fixed operation stream
//! is applied to SQLite memory, SQLite file and PostgreSQL stores, and their
//! durable storage rows are compared after every step.
//!
//! It compares storage surfaces only. Effect
//! records, tool-intent batches, await-event resolution and revocation,
//! runtime-operation journaling and retirement, and the historical effect-group
//! lifecycle — are not part of this surface.

use super::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::llm::attachment_delivery::{Delivery, DeliveryLimits, ProviderAccepts};

use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;

use lash_conformance::{
    StoreContractHandles, StoreContractOp, StoreContractScenario, sample_store_contract_operations,
};
use lash_core::{AttachmentCreateMeta, AttachmentStore, MediaType, RuntimeStore};
use lash_s3_store::{S3AttachmentStore, S3AttachmentStoreConfig};
use lash_sqlite_store::{SqliteStore, SqliteStoreSet, SqliteStoreSetOptions};

const DEFAULT_CASES: usize = 4;
const DEFAULT_SEED: u64 = 852;
const OPS_PER_CASE: usize = 55;
/// The session the scenario's runtime store is bound to: the runtime ops the
/// generated contract history executes all commit against it.
const SURFACE_RUNTIME_SESSION: &str = "prop-runtime-session";

const ALL_SURFACE_OPERATION_KINDS: &[&str] = &[
    "register",
    "first_start",
    "enter_wait",
    "clear_wait",
    "set_external_ref",
    "cancel_request",
    "terminal",
    "add_observer",
    "remove_observer",
    "prune",
    "compact_tombstones",
];

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "surface", content = "operation", rename_all = "snake_case")]
enum SurfaceOperation {
    StoreContract(StoreContractOp),
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
                StoreContractOp::CancelRequest { .. } => "cancel_request",
                StoreContractOp::Terminal { .. } => "terminal",
                StoreContractOp::AddObserver { .. } => "add_observer",
                StoreContractOp::RemoveObserver { .. } => "remove_observer",
                StoreContractOp::Prune { .. } => "prune",
                StoreContractOp::CompactTombstones { .. } => "compact_tombstones",
            },
        }
    }
}

#[path = "generated_surface/observation.rs"]
mod observation;
use observation::*;

struct SurfaceRunner {
    name: &'static str,
    scenario: StoreContractScenario,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    /// The durable store the registry commits through.
    reader: SurfaceReader,
}

fn generated_surface_operations(seed: u64) -> Vec<SurfaceOperation> {
    sample_store_contract_operations(seed, OPS_PER_CASE)
        .into_iter()
        .map(SurfaceOperation::StoreContract)
        .collect()
}

impl SurfaceRunner {
    async fn apply(&mut self, operation: &SurfaceOperation) -> Result<(), String> {
        match operation {
            SurfaceOperation::StoreContract(operation) => self.scenario.apply(operation).await,
        }
    }

    /// The raw rows, read once the process feed is sequenced. PostgreSQL
    /// stages a save and gives it its feed sequence on the next feed read
    /// (FIG-5276), where SQLite sequences in the save itself; reading the
    /// feed's bounds is that read on both.
    #[expect(
        clippy::expect_used,
        reason = "test support: a refused bounds read is a store defect the differential must surface"
    )]
    async fn observe(&self) -> SurfaceState {
        self.registry
            .process_change_bounds()
            .await
            .expect("the process feed's bounds read");
        self.reader.observe().await
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn reset_postgres_surface(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'lash\\_%' AND tablename NOT IN ('lash_schema_versions', 'lash_catalog_identity', 'lash_fleet_format') ORDER BY tablename").fetch_all(storage.pool()).await.unwrap();
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .unwrap();

    sqlx::query("INSERT INTO lash_process_change_clock (singleton, current_seq, tombstone_compaction_horizon) VALUES (TRUE, 0, 0) ON CONFLICT (singleton) DO UPDATE SET current_seq = 0, tombstone_compaction_horizon = 0").execute(storage.pool()).await.unwrap();
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
    // surfaces only — the process registry and the
    // session-bound runtime store. The SQL effect engines are not storage
    // (ADR 0104); FIG-3667 and FIG-3668 delete them.
    let sqlite_runtime_root = root.join("runtime");
    let sqlite_stores_path = root.join("stores.db");
    let session_request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(SURFACE_RUNTIME_SESSION),
        relation: SessionRelation::Root,
        config: lash_core::PersistedSessionConfig::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
            lash_core::SessionToolAccess::ambient(),
        ),
        head: SessionCreationHead::Config,
        retention: lash_core::Retention::UntilGc,
    };
    let sqlite_store = Arc::new(
        SqliteStore::open(
            &sqlite_runtime_root.join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .unwrap(),
    );
    sqlite_store.admit_session(&session_request).await.unwrap();
    let sqlite_runtime: Arc<dyn RuntimeStore> = sqlite_store;
    // The two registrars mint the same ids in the same order, so the
    // generated slots name the same process on both backends.
    let (sqlite_mint, postgres_mint) = super::paired_process_id_mints();
    let sqlite_stores = SqliteStoreSet::open_with_options_and_clock(
        &sqlite_stores_path,
        SqliteStoreSetOptions {
            process_id_mint: sqlite_mint,
            ..SqliteStoreSetOptions::standard(lash_sqlite_store::SqliteSynchronous::Normal)
        },
        Arc::clone(&clock),
    )
    .await
    .unwrap();
    lash_core::testing::process_execution_env_fixture(sqlite_stores.process_env_store().as_ref())
        .await;
    let sqlite_registry = sqlite_stores.process_registry();

    let postgres_store = Arc::new(
        storage
            .session_store_factory()
            .with_clock(Arc::clone(&clock)),
    );
    postgres_store
        .admit_session(&session_request)
        .await
        .unwrap();
    let postgres_runtime: Arc<dyn RuntimeStore> = postgres_store;
    // Every generated registration names the fixture execution env, and the
    // per-case reset truncates it, so each backend publishes it per case.
    lash_core::testing::process_execution_env_fixture(&storage.process_env_store()).await;
    let postgres_registry = Arc::new(
        storage
            .process_registry()
            .with_clock(Arc::clone(&clock))
            .with_process_id_mint_for_testing(postgres_mint),
    );

    vec![
        SurfaceRunner {
            name: "sqlite",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: sqlite_registry.clone(),
                runtime: Arc::clone(&sqlite_runtime),
            }),
            registry: sqlite_registry,
            reader: SurfaceReader::Sqlite {
                process_path: sqlite_stores_path.clone(),
            },
        },
        SurfaceRunner {
            name: "postgres",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: postgres_registry.clone(),
                runtime: Arc::clone(&postgres_runtime),
            }),
            registry: postgres_registry,
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
    let database_url = lash_postgres_store::testing::required_database_url();
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
    let storage = lash_postgres_store::testing::connect(&database_url)
        .await
        .unwrap();
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
    /// Deliver the last put ref as bytes, the one form every backend serves.
    DeliverLast,
    /// Reject the last delivery: a no-op for stores that cache no derivative.
    InvalidateLast,
    DeleteFirst,
    DeleteAbsent,
}

#[tokio::test]
#[ignore = "compares SQLite and S3 blob stores; requires a live S3 server (`scripts/ci/with-service.sh s3`, or LASH_REQUIRE_S3=1 with --include-ignored)"]
async fn attachment_blob_store_differential_agrees() {
    if std::env::var("LASH_REQUIRE_S3").as_deref() != Ok("1") {
        eprintln!("SKIPPED attachment blob-store differential: LASH_REQUIRE_S3 is not set");
        return;
    }
    let sqlite_memory_stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let memory = sqlite_memory_stores.attachment_store();
    let root = tempfile::tempdir().unwrap();
    let sqlite_file_stores = lash_sqlite_store::SqliteStoreSet::open(
        (root.path()).join("attachments.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("SQLite attachment store");
    let file = sqlite_file_stores.attachment_store();
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
        presigned_url_delivery: false,
    })
    .unwrap();
    let operations = [
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::Put(vec![9, 8]),
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::DeliverLast,
        BlobOperation::InvalidateLast,
        BlobOperation::DeleteFirst,
        BlobOperation::DeleteAbsent,
    ];
    eprintln!(
        "attachment blob differential coverage is bounded: operations={operations:?}; \
         backends=sqlite-memory,sqlite-file,s3; omitted_operations=all other byte sequences and operation \
         sequences"
    );
    let bytes_only = ProviderAccepts {
        bytes: true,
        ..ProviderAccepts::NONE
    };
    let limits = DeliveryLimits {
        max_bytes: 1024,
        max_upload_bytes: 1024,
        valid_through_ms: 0,
    };
    let mut first_id = None;
    let mut last = None;
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
                assert_eq!(memory_ref, file_ref);
                assert_eq!(file_ref, s3_ref);
                first_id.get_or_insert(memory_ref.id.clone());
                last = Some((memory_ref, bytes.clone()));
            }
            BlobOperation::DeliverLast => {
                let (reference, bytes) = last.as_ref().unwrap();
                let stores: [&dyn AttachmentStore; 3] = [memory.as_ref(), file.as_ref(), &s3];
                for store in stores {
                    let delivery = store
                        .deliver(reference, &bytes_only, &limits)
                        .await
                        .unwrap();
                    assert!(
                        matches!(&delivery, Delivery::Bytes(delivered) if delivered == bytes),
                        "a bytes-only acceptance delivered {delivery:?}"
                    );
                }
            }
            BlobOperation::InvalidateLast => {
                let (reference, bytes) = last.as_ref().unwrap();
                let rejected = Delivery::Bytes(bytes.clone());
                memory
                    .invalidate_delivery(reference, &rejected)
                    .await
                    .unwrap();
                file.invalidate_delivery(reference, &rejected)
                    .await
                    .unwrap();
                s3.invalidate_delivery(reference, &rejected).await.unwrap();
            }
            BlobOperation::DeleteFirst => {
                let id = first_id.as_ref().unwrap();
                memory.delete(id).await.unwrap();
                file.delete(id).await.unwrap();
                s3.delete(id).await.unwrap();
            }
            BlobOperation::DeleteAbsent => {
                let id =
                    lash_core::AttachmentId::parse("0".repeat(64)).expect("a digest-shaped id");
                memory.delete(&id).await.unwrap();
                file.delete(&id).await.unwrap();
                s3.delete(&id).await.unwrap();
            }
        }
        let memory_rows = raw_sqlite_blobs(&sqlite_memory_stores.database_uri());
        let file_rows = raw_sqlite_blobs(&sqlite_file_stores.database_uri());
        let s3_rows = s3.raw_blobs_for_testing().await.unwrap();
        assert_eq!(
            memory_rows, file_rows,
            "SQLite memory/file attachment blobs diverged after {operation:?}"
        );
        assert_eq!(
            file_rows, s3_rows,
            "SQLite file/S3 attachment blobs diverged after {operation:?}"
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
