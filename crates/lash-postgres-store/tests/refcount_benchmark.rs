use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core::store::load_persisted_session_state;
use lash_core::{
    ForkSessionRequest, InMemorySessionStoreFactory, OperationId, RuntimeCommit,
    RuntimePersistence, RuntimeSessionState, SessionRelation, SessionStoreCreateRequest,
    SessionStoreFactory,
};
use lash_postgres_store::PostgresStorage;
use lash_sqlite_store::SqliteSessionStoreFactory;

const DEEP_CHAIN_DEPTH: usize = 256;
const SAMPLES: usize = 7;
const WIDE_SIBLING_COUNT: usize = 64;

fn request(session_id: impl Into<String>) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        session_id: session_id.into(),
        relation: SessionRelation::Root,
        policy: Default::default(),
    }
}

fn operation(session_id: &str, key: &str) -> OperationId {
    OperationId::turn(session_id, key, "refcount-benchmark")
}

async fn create_state(
    factory: &Arc<dyn SessionStoreFactory>,
    session_id: &str,
) -> (Arc<dyn RuntimePersistence>, RuntimeSessionState) {
    let store = factory
        .create_store(&request(session_id))
        .await
        .expect("create benchmark store");
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..Default::default()
    };
    (store, state)
}

async fn commit_state(
    store: &Arc<dyn RuntimePersistence>,
    state: &RuntimeSessionState,
    key: &str,
) -> (String, String) {
    let (commit, _) = RuntimeCommit::persisted_state_for_test(state, &[])
        .with_operation(operation(&state.session_id, key))
        .expect("stamp benchmark commit");
    let root_node_id = commit
        .graph
        .nodes
        .first()
        .expect("benchmark commit has a root")
        .node_id
        .clone();
    let leaf_node_id = commit
        .graph
        .leaf_node_id
        .clone()
        .expect("benchmark commit has a leaf");
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit benchmark graph");
    (root_node_id, leaf_node_id)
}

async fn fork_store(
    factory: &Arc<dyn SessionStoreFactory>,
    node_id: &str,
    session_id: &str,
) -> Arc<dyn RuntimePersistence> {
    let fork_request = ForkSessionRequest {
        session_id: session_id.to_string(),
        node_id: node_id.to_string(),
        relation: SessionRelation::Root,
        policy: Default::default(),
    };
    factory
        .fork_at(&fork_request)
        .await
        .expect("fork benchmark session");
    factory
        .open_existing_store(&request(session_id))
        .await
        .expect("open benchmark fork")
        .expect("benchmark fork exists")
}

async fn append_child(store: &Arc<dyn RuntimePersistence>, key: &str) {
    let mut state = load_persisted_session_state(store.as_ref())
        .await
        .expect("load benchmark fork")
        .expect("benchmark fork state exists");
    state
        .session_graph
        .append_plugin("refcount-benchmark", serde_json::json!({ "key": key }));
    let (commit, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation(&state.session_id, key))
        .expect("stamp benchmark child commit");
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit benchmark child");
}

async fn create_chain(
    factory: &Arc<dyn SessionStoreFactory>,
    session_id: &str,
    depth: usize,
) -> (String, String) {
    let (store, mut state) = create_state(factory, session_id).await;
    state.ensure_agent_frame_initialized();
    for ordinal in 1..depth {
        state.session_graph.append_plugin(
            "refcount-benchmark",
            serde_json::json!({ "ordinal": ordinal }),
        );
    }
    commit_state(&store, &state, "seed-chain").await
}

fn percentile(samples: &mut [Duration], percentile: f64) -> Duration {
    samples.sort_unstable();
    let index = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[index]
}

fn print_samples(
    backend: &str,
    shape: &str,
    operation: &str,
    scale: usize,
    samples: &mut [Duration],
) {
    let median = percentile(samples, 0.5);
    let p95 = percentile(samples, 0.95);
    println!(
        "{backend},{shape},{operation},{scale},{:.3},{:.3},{}",
        median.as_secs_f64() * 1_000.0,
        p95.as_secs_f64() * 1_000.0,
        samples.len(),
    );
}

async fn benchmark_backend(backend: &str, factory: Arc<dyn SessionStoreFactory>, run_id: &str) {
    let prefix = format!("refcount-bench-{run_id}-{backend}");
    let wide_source_id = format!("{prefix}-wide-source");
    let (wide_source, mut wide_state) = create_state(&factory, &wide_source_id).await;
    wide_state.ensure_agent_frame_initialized();
    let (wide_root, _) = commit_state(&wide_source, &wide_state, "seed-wide").await;
    factory.pin(&wide_root).await.expect("pin wide root");
    for ordinal in 0..WIDE_SIBLING_COUNT {
        let branch_id = format!("{prefix}-wide-sibling-{ordinal}");
        let branch = fork_store(&factory, &wide_root, &branch_id).await;
        append_child(&branch, &format!("wide-sibling-{ordinal}")).await;
    }

    let deep_source_id = format!("{prefix}-deep-source");
    let (_, deep_leaf) = create_chain(&factory, &deep_source_id, DEEP_CHAIN_DEPTH).await;

    let mut wide_fork = Vec::with_capacity(SAMPLES);
    let mut wide_head_move = Vec::with_capacity(SAMPLES);
    let mut wide_delete = Vec::with_capacity(SAMPLES);
    let mut deep_fork = Vec::with_capacity(SAMPLES);
    let mut deep_head_move = Vec::with_capacity(SAMPLES);
    let mut deep_delete = Vec::with_capacity(SAMPLES);

    for sample in 0..SAMPLES {
        let fork_id = format!("{prefix}-wide-fork-{sample}");
        let started = Instant::now();
        fork_store(&factory, &wide_root, &fork_id).await;
        wide_fork.push(started.elapsed());

        let mover_id = format!("{prefix}-wide-mover-{sample}");
        let mover = fork_store(&factory, &wide_root, &mover_id).await;
        let mut mover_state = load_persisted_session_state(mover.as_ref())
            .await
            .expect("load wide mover")
            .expect("wide mover state exists");
        mover_state.session_graph.append_plugin(
            "refcount-benchmark",
            serde_json::json!({ "sample": sample }),
        );
        let (commit, _) = RuntimeCommit::persisted_state_for_test(&mover_state, &[])
            .with_operation(operation(&mover_id, "head-move"))
            .expect("stamp wide head move");
        let started = Instant::now();
        mover
            .commit_runtime_state(commit)
            .await
            .expect("commit wide head move");
        wide_head_move.push(started.elapsed());

        let victim_id = format!("{prefix}-wide-victim-{sample}");
        let victim = fork_store(&factory, &wide_root, &victim_id).await;
        append_child(&victim, "wide-delete-child").await;
        let started = Instant::now();
        factory
            .delete_session(&victim_id)
            .await
            .expect("delete wide victim");
        wide_delete.push(started.elapsed());

        let deep_fork_id = format!("{prefix}-deep-fork-{sample}");
        let started = Instant::now();
        fork_store(&factory, &deep_leaf, &deep_fork_id).await;
        deep_fork.push(started.elapsed());

        let deep_mover_id = format!("{prefix}-deep-mover-{sample}");
        let deep_mover = fork_store(&factory, &deep_leaf, &deep_mover_id).await;
        let mut deep_mover_state = load_persisted_session_state(deep_mover.as_ref())
            .await
            .expect("load deep mover")
            .expect("deep mover state exists");
        deep_mover_state.session_graph.append_plugin(
            "refcount-benchmark",
            serde_json::json!({ "sample": sample }),
        );
        let (commit, _) = RuntimeCommit::persisted_state_for_test(&deep_mover_state, &[])
            .with_operation(operation(&deep_mover_id, "head-move"))
            .expect("stamp deep head move");
        let started = Instant::now();
        deep_mover
            .commit_runtime_state(commit)
            .await
            .expect("commit deep head move");
        deep_head_move.push(started.elapsed());

        let deep_victim_id = format!("{prefix}-deep-victim-{sample}");
        create_chain(&factory, &deep_victim_id, DEEP_CHAIN_DEPTH).await;
        let started = Instant::now();
        factory
            .delete_session(&deep_victim_id)
            .await
            .expect("delete deep victim");
        deep_delete.push(started.elapsed());
    }

    print_samples(backend, "wide", "fork", WIDE_SIBLING_COUNT, &mut wide_fork);
    print_samples(
        backend,
        "wide",
        "head_move",
        WIDE_SIBLING_COUNT,
        &mut wide_head_move,
    );
    print_samples(
        backend,
        "wide",
        "delete",
        WIDE_SIBLING_COUNT,
        &mut wide_delete,
    );
    print_samples(backend, "deep", "fork", DEEP_CHAIN_DEPTH, &mut deep_fork);
    print_samples(
        backend,
        "deep",
        "head_move",
        DEEP_CHAIN_DEPTH,
        &mut deep_head_move,
    );
    print_samples(
        backend,
        "deep",
        "delete",
        DEEP_CHAIN_DEPTH,
        &mut deep_delete,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement benchmark; requires LASH_POSTGRES_DATABASE_URL"]
async fn measured_refcount_replacement_operations() {
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .expect("set LASH_POSTGRES_DATABASE_URL to run the benchmark");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect benchmark Postgres");
    let sqlite_dir = tempfile::tempdir().expect("SQLite benchmark directory");
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let backends: Vec<(&str, Arc<dyn SessionStoreFactory>)> = vec![
        ("in_memory", Arc::new(InMemorySessionStoreFactory::new())),
        (
            "sqlite",
            Arc::new(SqliteSessionStoreFactory::new(sqlite_dir.path())),
        ),
        ("postgres", Arc::new(postgres.session_store_factory())),
    ];

    println!("backend,shape,operation,scale,median_ms,p95_ms,samples");
    for (backend, factory) in backends {
        benchmark_backend(backend, factory, &run_id).await;
    }
}
