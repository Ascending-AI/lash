use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core::store::GraphAppend;
use lash_core::{
    AttachmentId, CommitBudget, CommitBudgetLimit, PersistedSessionConfig, ProtocolEvent,
    RuntimeCommit, RuntimePersistence, RuntimeSessionState, SessionHistoryRecord,
    SessionNodePayload, SessionNodeRecord, SessionPolicy, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory,
};
use lash_postgres_store::PostgresStorage;
use lash_sqlite_store::SqliteSessionStoreFactory;
use rusqlite::{Connection, params};
use sqlx::QueryBuilder;

const RECOMMENDED_BYTES: usize = 1024 * 1024;
const RECOMMENDED_ROWS: usize = 512;
const BYTE_TARGETS: &[usize] = &[
    256 * 1024,
    512 * 1024,
    768 * 1024,
    RECOMMENDED_BYTES,
    1280 * 1024,
];
const ROW_SHAPES: &[RowShape] = &[
    RowShape::new(16, 48),
    RowShape::new(64, 192),
    RowShape::new(128, 384),
    RowShape::new(160, 480),
];
const BYTE_CURVE_ROWS: RowShape = RowShape::new(96, 32);
const ROW_CURVE_BYTES: usize = 512 * 1024;
const SMALL_CHECKPOINT_COMPONENTS: usize = 32;
const LARGE_CHECKPOINT_COMPONENTS: usize = 3;
const WARMUP_SAMPLES: usize = 3;
const SAMPLES: usize = 21;

#[derive(Clone, Copy)]
struct RowShape {
    graph: usize,
    adoption: usize,
}

impl RowShape {
    const fn new(graph: usize, adoption: usize) -> Self {
        Self { graph, adoption }
    }

    const fn total(self) -> usize {
        self.graph + self.adoption
    }
}

#[derive(Clone, Copy)]
struct BenchmarkCase {
    axis: &'static str,
    target: usize,
    logical_bytes: usize,
    rows: RowShape,
}

fn seeded_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut bytes = vec![0; len];
    rng.fill(&mut bytes);
    bytes
}

fn seeded_ascii(len: usize, seed: u64) -> String {
    seeded_bytes(len, seed)
        .into_iter()
        .map(|byte| b'!' + (byte % 94))
        .map(char::from)
        .collect()
}

fn realistic_commit(
    session_id: &str,
    row_shape: RowShape,
    logical_bytes: usize,
    sample: usize,
) -> RuntimeCommit {
    let frame_key = lash_core::FrameKey::from_caller_material("benchmark-frame")
        .expect("non-empty frame material");
    let frame_node_id = lash_core::facade_support::frame_node_id(session_id, frame_key.as_str());
    let nodes = (0..row_shape.graph)
        .map(|index| {
            let node_id = if index == 0 {
                frame_node_id.to_string()
            } else {
                format!("{session_id}:node:{index}")
            };
            SessionNodeRecord {
                node_id: node_id.clone(),
                parent_node_id: (index > 0).then(|| {
                    if index == 1 {
                        frame_node_id.to_string()
                    } else {
                        format!("{session_id}:node:{}", index - 1)
                    }
                }),
                timestamp: "2026-08-20T12:00:00Z".to_string(),
                payload: if index == 0 {
                    SessionNodePayload::FrameOpen {
                        frame_key: frame_key.clone(),
                        reason: lash_core::AgentFrameReason::initial(),
                        assignment: lash_core::AgentFrameAssignment::from_policy(
                            SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                        ),
                        protocol_turn_options: Default::default(),
                    }
                } else {
                    SessionNodePayload::Event {
                        event: SessionHistoryRecord::Protocol(
                            ProtocolEvent::typed(
                                "commit-size-benchmark",
                                serde_json::json!({
                                    "role": if index % 4 == 0 { "assistant" } else { "tool" },
                                    "content": seeded_ascii(
                                        96,
                                        ((sample as u64) << 32) | index as u64,
                                    ),
                                    "ordinal": index,
                                    "status": "complete",
                                    "retryable": false,
                                }),
                            )
                            .expect("benchmark protocol event"),
                        ),
                    }
                },
            }
        })
        .collect::<Vec<_>>();
    let attachment_ids = (0..row_shape.adoption)
        .map(|index| {
            AttachmentId::parse(format!("{session_id}:attachment:{index:08}"))
                .expect("valid attachment id")
        })
        .step_by(2)
        .collect::<Vec<_>>();
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        policy: SessionPolicy {
            model: lash_core::ModelSpec::builder("benchmark-model")
                .context_window_tokens(200_000)
                .build()
                .expect("benchmark model"),
            ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(
        &state,
        &[],
        CommitBudget::new(CommitBudgetLimit::Unbounded, CommitBudgetLimit::Unbounded),
    );
    commit.current_frame_node_id = Some(frame_node_id);
    commit.config = PersistedSessionConfig::from(&state.policy);
    commit.config.provider_id = "benchmark".to_string();
    commit.graph = GraphAppend {
        leaf_node_id: nodes.last().map(|node| node.node_id.clone()),
        nodes,
    };
    for index in 0..SMALL_CHECKPOINT_COMPONENTS {
        let len = 64 + (index % 5) * 32;
        commit.checkpoint.components.insert(
            format!("benchmark/small/{index:02}"),
            lash_core::HydratedCheckpointComponent::changed(seeded_bytes(
                len,
                0x51_4d_41_4c_4c ^ ((sample as u64) << 16) ^ index as u64,
            )),
        );
    }
    for index in 0..LARGE_CHECKPOINT_COMPONENTS {
        commit.checkpoint.components.insert(
            format!("benchmark/body/{index}"),
            lash_core::HydratedCheckpointComponent::changed(Vec::new()),
        );
    }
    commit.committed_attachment_ids = attachment_ids;
    commit.adopted_intent_rows = row_shape.adoption as u64;
    let turn_id = format!("commit-size-benchmark-{sample}");
    let (mut commit, _) = commit
        .with_operation(lash_core::store::OperationId::new(
            lash_core::ExecutionScope::turn(session_id, turn_id),
            "commit",
        ))
        .expect("derive benchmark graph node ids");

    let baseline = lash_core::testing::measure_runtime_commit_budget(&commit)
        .expect("measure benchmark baseline")
        .total_bytes;
    assert!(
        baseline <= logical_bytes,
        "benchmark shape baseline {baseline} exceeds requested {logical_bytes} logical bytes"
    );
    let remaining = logical_bytes - baseline;
    let per_large_body = remaining / LARGE_CHECKPOINT_COMPONENTS;
    let remainder = remaining % LARGE_CHECKPOINT_COMPONENTS;
    for index in 0..LARGE_CHECKPOINT_COMPONENTS {
        let len = per_large_body + usize::from(index < remainder);
        commit.checkpoint.components.insert(
            format!("benchmark/body/{index}"),
            lash_core::HydratedCheckpointComponent::changed(seeded_bytes(
                len,
                0x42_4f_44_59 ^ ((sample as u64) << 16) ^ index as u64,
            )),
        );
    }
    commit
}

fn adopted_attachment_ids(commit: &RuntimeCommit) -> impl Iterator<Item = AttachmentId> + '_ {
    let count = usize::try_from(commit.adopted_intent_rows).unwrap_or(usize::MAX);
    (0..count).map(|index| {
        AttachmentId::parse(format!("{}:attachment:{index:08}", commit.session_id))
            .expect("valid benchmark attachment id")
    })
}

fn sqlite_seed_attachment_intents(database_path: &std::path::Path, commit: &RuntimeCommit) {
    let turn_id = commit
        .turn_commit
        .operation
        .turn_id()
        .expect("benchmark commit has a turn owner");
    let mut connection = Connection::open(database_path).expect("open SQLite benchmark fixture");
    let transaction = connection
        .transaction()
        .expect("begin SQLite benchmark fixture transaction");
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO attachment_manifest
                    (attachment_id, session_id, canonical_uri, intent_at_ms,
                     committed_at_ms, owner_kind, owner_id)
                 VALUES (?1, ?2, ?3, 1, NULL, 'turn', ?4)",
            )
            .expect("prepare SQLite benchmark intent insert");
        for attachment_id in adopted_attachment_ids(commit) {
            statement
                .execute(params![
                    attachment_id.as_str(),
                    commit.session_id,
                    format!("lash-attachment://blake3/{attachment_id}"),
                    turn_id,
                ])
                .expect("insert SQLite benchmark attachment intent");
        }
    }
    transaction
        .commit()
        .expect("commit SQLite benchmark fixture transaction");
}

async fn postgres_seed_attachment_intents(pool: &sqlx::PgPool, commit: &RuntimeCommit) {
    if commit.adopted_intent_rows == 0 {
        return;
    }
    let turn_id = commit
        .turn_commit
        .operation
        .turn_id()
        .expect("benchmark commit has a turn owner");
    let mut query = QueryBuilder::<sqlx::Postgres>::new(
        "INSERT INTO lash_attachment_manifest (
            attachment_id, session_id, canonical_uri, intent_at_ms, committed_at_ms,
            owner_kind, owner_id
         ) ",
    );
    query.push_values(adopted_attachment_ids(commit), |mut row, attachment_id| {
        row.push_bind(attachment_id.to_string())
            .push_bind(&commit.session_id)
            .push_bind(format!("lash-attachment://blake3/{attachment_id}"))
            .push_bind(1_i64)
            .push_bind(None::<i64>)
            .push_bind("turn")
            .push_bind(turn_id);
    });
    query
        .build()
        .execute(pool)
        .await
        .expect("insert PostgreSQL benchmark attachment intents");
}

async fn time_commit(store: Arc<dyn RuntimePersistence>, commit: RuntimeCommit) -> Duration {
    let started = Instant::now();
    store
        .commit_runtime_state(commit)
        .await
        .expect("benchmark commit");
    started.elapsed()
}

fn percentile(samples: &mut [Duration], percentile: f64) -> Duration {
    samples.sort_unstable();
    let nearest_rank = ((samples.len() as f64 * percentile).ceil() as usize).saturating_sub(1);
    samples[nearest_rank.min(samples.len() - 1)]
}

fn assert_reference_admission(commit: &RuntimeCommit) {
    let measured = lash_core::testing::measure_runtime_commit_budget(commit)
        .expect("measure benchmark commit with production accounting");
    let mut bounded = commit.clone();
    bounded.commit_budget = CommitBudget::bounded(RECOMMENDED_BYTES, RECOMMENDED_ROWS);
    assert_eq!(
        bounded.validate_budget().is_ok(),
        measured.total_bytes <= RECOMMENDED_BYTES && measured.total_rows <= RECOMMENDED_ROWS,
        "reference admission must agree with measured logical bytes and rows"
    );
}

#[test]
fn measured_budget_matches_seeded_checkpoint_and_adoption_rows() {
    let row_shape = RowShape::new(8, 3);
    let logical_bytes = RuntimeCommit::MAX_COMMIT_BUDGET_BYTES + 1;
    let commit = realistic_commit("benchmark-budget-accounting", row_shape, logical_bytes, 0);
    let expected = lash_core::testing::measure_runtime_commit_budget(&commit)
        .expect("measure benchmark commit with production accounting");

    assert_eq!(expected.total_bytes, logical_bytes);
    assert_eq!(expected.graph_rows, row_shape.graph);
    assert_eq!(expected.adopted_intent_rows, row_shape.adoption);
    assert_eq!(expected.total_rows, row_shape.total());
    let mut bounded = commit;
    bounded.commit_budget = CommitBudget::bounded(
        RuntimeCommit::MAX_COMMIT_BUDGET_BYTES,
        RuntimeCommit::MAX_COMMIT_NODE_COUNT,
    );
    assert!(matches!(
        bounded.validate_budget(),
        Err(lash_core::StoreError::CommitByteBudgetExceeded {
            session_config_bytes,
            graph_delta_bytes,
            checkpoint_bytes,
            attachment_manifest_bytes,
            queue_batch_bytes,
            agent_frame_bytes,
            usage_delta_bytes,
            turn_result_bytes,
            total_bytes,
            max_bytes,
        }) if session_config_bytes == expected.session_config_bytes
            && graph_delta_bytes == expected.graph_delta_bytes
            && checkpoint_bytes == expected.checkpoint_bytes
            && attachment_manifest_bytes == expected.attachment_manifest_bytes
            && queue_batch_bytes == expected.queue_batch_bytes
            && agent_frame_bytes == expected.agent_frame_bytes
            && usage_delta_bytes == expected.usage_delta_bytes
            && turn_result_bytes == expected.turn_result_bytes
            && total_bytes == expected.total_bytes
            && max_bytes == RuntimeCommit::MAX_COMMIT_BUDGET_BYTES
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement benchmark; requires LASH_POSTGRES_DATABASE_URL"]
async fn measured_commit_size_curve() {
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .expect("set LASH_POSTGRES_DATABASE_URL to run the benchmark");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect benchmark Postgres");
    let postgres_fixture_pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect PostgreSQL benchmark fixture pool");
    let sqlite_dir = tempfile::tempdir().expect("SQLite benchmark directory");
    let sqlite_factory = SqliteSessionStoreFactory::new(sqlite_dir.path());
    let sqlite_database_path = sqlite_dir.path().join("durable-core.db");
    let cases = BYTE_TARGETS
        .iter()
        .copied()
        .map(|logical_bytes| BenchmarkCase {
            axis: "logical_bytes",
            target: logical_bytes,
            logical_bytes,
            rows: BYTE_CURVE_ROWS,
        })
        .chain(ROW_SHAPES.iter().copied().map(|rows| BenchmarkCase {
            axis: "rows_written",
            target: rows.total(),
            logical_bytes: ROW_CURVE_BYTES,
            rows,
        }))
        .collect::<Vec<_>>();

    println!(
        "| axis | target | backend | logical bytes | graph rows | adoption rows | total rows | logical checkpoint bytes | p50 commit ms | p95 commit ms | samples |"
    );
    println!("|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for case in cases {
        for backend in ["sqlite", "postgres"] {
            let mut elapsed = Vec::with_capacity(SAMPLES);
            let mut measured = None;
            for sample in 0..(WARMUP_SAMPLES + SAMPLES) {
                let session_id = format!(
                    "bench-{}-{}-{}-{backend}-{sample}",
                    case.axis,
                    case.target,
                    case.rows.total()
                );
                let store = match backend {
                    "sqlite" => sqlite_factory
                        .create_store(&SessionStoreCreateRequest {
                            pending_observer_intents: Vec::new(),
                            session_id: session_id.clone(),
                            relation: SessionRelation::Root,
                            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                        })
                        .await
                        .expect("create SQLite benchmark store"),
                    "postgres" => {
                        Arc::new(postgres.session_store(&session_id)) as Arc<dyn RuntimePersistence>
                    }
                    _ => unreachable!(),
                };
                let commit = realistic_commit(&session_id, case.rows, case.logical_bytes, sample);
                let sample_measurement = lash_core::testing::measure_runtime_commit_budget(&commit)
                    .expect("measure benchmark commit with production accounting");
                assert_eq!(sample_measurement.total_bytes, case.logical_bytes);
                assert_eq!(sample_measurement.graph_rows, case.rows.graph);
                assert_eq!(sample_measurement.adopted_intent_rows, case.rows.adoption);
                assert_eq!(sample_measurement.total_rows, case.rows.total());
                assert_reference_admission(&commit);
                store
                    .admit_and_bind_session(&lash_core::SessionBinding::root(session_id))
                    .await
                    .expect("bind benchmark session to store");
                match backend {
                    "sqlite" => sqlite_seed_attachment_intents(&sqlite_database_path, &commit),
                    "postgres" => {
                        postgres_seed_attachment_intents(&postgres_fixture_pool, &commit).await
                    }
                    _ => unreachable!(),
                }
                let duration = time_commit(store, commit).await;
                if sample >= WARMUP_SAMPLES {
                    elapsed.push(duration);
                }
                measured = Some(sample_measurement);
            }
            let p50 = percentile(&mut elapsed, 0.50);
            let p95 = percentile(&mut elapsed, 0.95);
            let measured = measured.expect("benchmark records at least one sample");
            println!(
                "| {} | {} | {backend} | {} | {} | {} | {} | {} | {:.3} | {:.3} | {SAMPLES} |",
                case.axis,
                case.target,
                measured.total_bytes,
                measured.graph_rows,
                measured.adopted_intent_rows,
                measured.total_rows,
                measured.checkpoint_bytes,
                p50.as_secs_f64() * 1_000.0,
                p95.as_secs_f64() * 1_000.0,
            );
        }
    }
}
