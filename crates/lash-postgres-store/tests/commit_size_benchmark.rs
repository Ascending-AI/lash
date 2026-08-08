use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core::store::GraphAppend;
use lash_core::{
    AttachmentId, AttachmentIntent, PersistedSessionConfig, ProtocolEvent, RuntimeCommit,
    RuntimePersistence, RuntimeSessionState, SessionHistoryRecord, SessionNodePayload,
    SessionNodeRecord, SessionPolicy, SessionRelation, SessionStoreCreateRequest,
    SessionStoreFactory,
};
use lash_postgres_store::PostgresStorage;
use lash_sqlite_store::SqliteSessionStoreFactory;

const NODE_COUNTS: &[usize] = &[8, 32, 128, 512];
const CHECKPOINT_PAYLOAD_BYTES: &[usize] = &[0, 64 * 1024, 256 * 1024];
const SAMPLES: usize = 7;

fn realistic_commit(
    session_id: &str,
    node_count: usize,
    checkpoint_payload_bytes: usize,
    sample: usize,
) -> RuntimeCommit {
    let payload = "turn-tail-token ".repeat(48);
    let nodes = (0..node_count)
        .map(|index| {
            let node_id = format!("{session_id}:node:{index}");
            SessionNodeRecord {
                node_id: node_id.clone(),
                parent_node_id: (index > 0).then(|| format!("{session_id}:node:{}", index - 1)),
                timestamp: "2026-07-26T12:00:00Z".to_string(),
                payload: if index == 0 {
                    SessionNodePayload::FrameOpen {
                        frame_key: "benchmark-frame".to_string(),
                        reason: lash_core::AgentFrameReason::initial(),
                        assignment: lash_core::AgentFrameAssignment::from_policy(
                            SessionPolicy::default(),
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
                                    "content": payload,
                                    "ordinal": index,
                                }),
                            )
                            .expect("benchmark protocol event"),
                        ),
                    }
                },
            }
        })
        .collect::<Vec<_>>();
    let attachment_count = node_count.div_ceil(512).clamp(1, 16);
    let attachment_ids = (0..attachment_count)
        .map(|index| AttachmentId::new(format!("{session_id}:attachment:{index:08}")))
        .collect::<Vec<_>>();
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        policy: SessionPolicy {
            model: lash_core::ModelSpec::builder("benchmark-model")
                .context_window_tokens(200_000)
                .build()
                .expect("benchmark model"),
            ..SessionPolicy::default()
        },
        ..RuntimeSessionState::default()
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.current_frame_node_id = Some(format!("{session_id}:node:0"));
    commit.config = PersistedSessionConfig {
        provider_id: "benchmark".to_string(),
        model: state.policy.model,
    };
    commit.graph = GraphAppend {
        leaf_node_id: nodes.last().map(|node| node.node_id.clone()),
        nodes,
    };
    commit.checkpoint.execution_state = Some(vec![sample as u8; checkpoint_payload_bytes]);
    commit.committed_attachment_ids = attachment_ids;
    commit
        .with_operation(lash_core::store::OperationId::new(
            lash_core::ExecutionScope::runtime_operation(format!(
                "commit-size-benchmark:{session_id}"
            )),
            "commit",
        ))
        .expect("derive benchmark graph node ids")
        .0
}

fn record_attachment_intents(store: &dyn RuntimePersistence, commit: &RuntimeCommit) {
    for attachment_id in &commit.committed_attachment_ids {
        store
            .record_intent(AttachmentIntent {
                attachment_id: attachment_id.clone(),
                session_id: commit.session_id.clone(),
                canonical_uri: format!("lash-attachment://sha256/{attachment_id}"),
                intent_at_epoch_ms: 1,
                owner_kind: None,
                owner_id: None,
            })
            .expect("record benchmark attachment intent");
    }
}

async fn time_commit(store: Arc<dyn RuntimePersistence>, commit: RuntimeCommit) -> Duration {
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(
            commit.session_id.clone(),
            &SessionPolicy::default(),
        ))
        .await
        .expect("bind benchmark session to store");
    record_attachment_intents(store.as_ref(), &commit);
    let started = Instant::now();
    store
        .commit_runtime_state(commit)
        .await
        .expect("benchmark commit");
    started.elapsed()
}

fn percentile(samples: &mut [Duration], percentile: f64) -> Duration {
    samples.sort_unstable();
    let index = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[index]
}

#[test]
fn measured_bytes_match_validate_budget_components() {
    let commit = realistic_commit(
        "benchmark-budget-accounting",
        8,
        RuntimeCommit::MAX_COMMIT_BUDGET_BYTES + 1,
        0,
    );
    let expected = lash_core::testing::measure_runtime_commit_budget(&commit)
        .expect("measure benchmark commit with production accounting");

    assert!(matches!(
        commit.validate_budget(),
        Err(lash_core::StoreError::CommitByteBudgetExceeded {
            graph_delta_bytes,
            checkpoint_bytes,
            attachment_manifest_bytes,
            total_bytes,
            max_bytes,
        }) if graph_delta_bytes == expected.graph_delta_bytes
            && checkpoint_bytes == expected.checkpoint_bytes
            && attachment_manifest_bytes == expected.attachment_manifest_bytes
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
    let sqlite_dir = tempfile::tempdir().expect("SQLite benchmark directory");
    let sqlite_factory = SqliteSessionStoreFactory::new(sqlite_dir.path());

    println!(
        "backend,node_count,checkpoint_payload_bytes,graph_delta_bytes,checkpoint_bytes,attachment_manifest_bytes,budget_bytes,median_ms,p95_ms,samples"
    );
    for &node_count in NODE_COUNTS {
        for &checkpoint_payload_bytes in CHECKPOINT_PAYLOAD_BYTES {
            for backend in ["sqlite", "postgres"] {
                let mut elapsed = Vec::with_capacity(SAMPLES);
                let mut measured_bytes = None;
                for sample in 0..SAMPLES {
                    let session_id =
                        format!("bench-{backend}-{node_count}-{checkpoint_payload_bytes}-{sample}");
                    let store = match backend {
                        "sqlite" => sqlite_factory
                            .create_store(&SessionStoreCreateRequest {
                                session_id: session_id.clone(),
                                relation: SessionRelation::Root,
                                policy: SessionPolicy::default(),
                            })
                            .await
                            .expect("create SQLite benchmark store"),
                        "postgres" => Arc::new(postgres.session_store(&session_id))
                            as Arc<dyn RuntimePersistence>,
                        _ => unreachable!(),
                    };
                    let commit =
                        realistic_commit(&session_id, node_count, checkpoint_payload_bytes, sample);
                    measured_bytes = Some(
                        lash_core::testing::measure_runtime_commit_budget(&commit)
                            .expect("measure benchmark commit with production accounting"),
                    );
                    commit
                        .validate_budget()
                        .expect("benchmark case must fit the production commit budget");
                    elapsed.push(time_commit(store, commit).await);
                }
                let median = percentile(&mut elapsed, 0.5);
                let p95 = percentile(&mut elapsed, 0.95);
                let measured_bytes = measured_bytes.expect("benchmark records at least one sample");
                println!(
                    "{backend},{node_count},{checkpoint_payload_bytes},{},{},{},{},{:.3},{:.3},{SAMPLES}",
                    measured_bytes.graph_delta_bytes,
                    measured_bytes.checkpoint_bytes,
                    measured_bytes.attachment_manifest_bytes,
                    measured_bytes.total_bytes,
                    median.as_secs_f64() * 1_000.0,
                    p95.as_secs_f64() * 1_000.0,
                );
            }
        }
    }
}
