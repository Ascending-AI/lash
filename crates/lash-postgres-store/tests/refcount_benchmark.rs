// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::SessionId;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core_execution::store::{
    HistoryAnchor, HistoryBudget, SessionStore, WindowSelector, load_session_window_state,
};
use lash_core_execution::{
    DeploymentStore, ForkSessionRequest, OperationId, RuntimeCommit, RuntimeSessionState,
    RuntimeStore, SessionCreationHead, SessionRelation, SessionStoreCreateRequest,
};
use lash_postgres_store::PostgresStorage;

const DEEP_CHAIN_DEPTH: usize = 256;
const DEEP_FORK_CHAIN_DEPTH: usize = 64;
const SAMPLES: usize = 7;
const WIDE_SIBLING_COUNT: usize = 64;

fn request(session_id: impl Into<SessionId>) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.into(),
        relation: SessionRelation::Root,
        config: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded)
            .into(),
        head: SessionCreationHead::CommittedByCreator,
    }
}

fn operation(session_id: &SessionId, key: &str) -> OperationId {
    OperationId::turn(session_id, key, "refcount-benchmark")
}

async fn create_state(
    factory: &Arc<dyn DeploymentStore>,
    session_id: &SessionId,
) -> (Arc<dyn RuntimeStore>, RuntimeSessionState) {
    factory
        .admit_session(&request(session_id))
        .await
        .expect("admit benchmark session");
    let store = Arc::clone(factory) as Arc<dyn RuntimeStore>;
    let state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    (store, state)
}

async fn commit_state(
    store: &Arc<dyn RuntimeStore>,
    state: &RuntimeSessionState,
    key: &str,
) -> (String, String) {
    let (commit, _) = RuntimeCommit::persisted_state_for_test(state, &[])
        .with_operation(operation(&state.session_id, key))
        .expect("stamp benchmark commit");
    let root_node_id = commit
        .graph
        .nodes()
        .first()
        .expect("benchmark commit has a root")
        .node_id
        .clone();
    let leaf_node_id = commit
        .graph
        .leaf_node_id()
        .cloned()
        .expect("benchmark commit has a leaf");
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit benchmark graph");
    (root_node_id.to_string(), leaf_node_id.to_string())
}

async fn fork_store(
    factory: &Arc<dyn DeploymentStore>,
    node_id: &str,
    session_id: &SessionId,
) -> Arc<dyn RuntimeStore> {
    let fork_request = ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        node_id: node_id.to_string().into(),
        relation: SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    factory
        .fork_session(&fork_request)
        .await
        .expect("fork benchmark session");
    Arc::clone(factory) as Arc<dyn RuntimeStore>
}

async fn append_child(store: &Arc<dyn RuntimeStore>, session_id: &SessionId, key: &str) {
    let mut state = load_state(store, session_id).await;
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

async fn load_state(store: &Arc<dyn RuntimeStore>, session_id: &SessionId) -> RuntimeSessionState {
    let view = SessionStore::new(Arc::clone(store), session_id.clone())
        .expect("valid benchmark session id");
    load_session_window_state(&view, WindowSelector::Current)
        .await
        .expect("load benchmark fork")
        .expect("benchmark fork state exists")
        .state
}

async fn create_chain(
    factory: &Arc<dyn DeploymentStore>,
    session_id: &SessionId,
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

async fn create_fork_chain(
    factory: &Arc<dyn DeploymentStore>,
    prefix: &str,
) -> (String, String, Arc<dyn RuntimeStore>) {
    let source_id = format!("{prefix}-fork-chain-source");
    let (source, mut state) = create_state(factory, &SessionId::from(source_id)).await;
    state.ensure_agent_frame_initialized();
    let (root_node_id, mut leaf_node_id) = commit_state(&source, &state, "seed-fork-chain").await;
    let mut terminal = source;
    for depth in 0..DEEP_FORK_CHAIN_DEPTH {
        let session_id = SessionId::from(format!("{prefix}-fork-chain-{depth}"));
        terminal = fork_store(factory, &leaf_node_id, &session_id).await;
        append_child(&terminal, &session_id, &format!("fork-chain-{depth}")).await;
        leaf_node_id = terminal
            .load_session_window(&session_id, WindowSelector::Current)
            .await
            .expect("load fork-chain session")
            .expect("fork-chain session exists")
            .window
            .leaf_node_id
            .clone()
            .expect("fork-chain leaf")
            .to_string();
    }
    (root_node_id, leaf_node_id, terminal)
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

async fn benchmark_backend(backend: &str, factory: Arc<dyn DeploymentStore>, run_id: &str) {
    let prefix = format!("refcount-bench-{run_id}-{backend}");
    let wide_source_id = format!("{prefix}-wide-source");
    let (wide_source, mut wide_state) =
        create_state(&factory, &SessionId::from(wide_source_id)).await;
    wide_state.ensure_agent_frame_initialized();
    let (wide_root, _) = commit_state(&wide_source, &wide_state, "seed-wide").await;
    factory
        .pin(&wide_root.clone().into())
        .await
        .expect("pin wide root");
    for ordinal in 0..WIDE_SIBLING_COUNT {
        let branch_id = format!("{prefix}-wide-sibling-{ordinal}");
        let branch_id = SessionId::from(branch_id);
        let branch = fork_store(&factory, &wide_root, &branch_id).await;
        append_child(&branch, &branch_id, &format!("wide-sibling-{ordinal}")).await;
    }

    let deep_source_id = format!("{prefix}-deep-source");
    let (_, deep_leaf) =
        create_chain(&factory, &SessionId::from(deep_source_id), DEEP_CHAIN_DEPTH).await;
    let (fork_chain_root, fork_chain_leaf, fork_chain_terminal) =
        create_fork_chain(&factory, &prefix).await;

    let mut wide_fork = Vec::with_capacity(SAMPLES);
    let mut wide_head_move = Vec::with_capacity(SAMPLES);
    let mut wide_delete = Vec::with_capacity(SAMPLES);
    let mut deep_fork = Vec::with_capacity(SAMPLES);
    let mut deep_head_move = Vec::with_capacity(SAMPLES);
    let mut deep_delete = Vec::with_capacity(SAMPLES);
    let mut fork_chain_load_node = Vec::with_capacity(SAMPLES);
    let mut fork_chain_load_session = Vec::with_capacity(SAMPLES);
    let mut fork_chain_fork = Vec::with_capacity(SAMPLES);

    for sample in 0..SAMPLES {
        let fork_id = format!("{prefix}-wide-fork-{sample}");
        let started = Instant::now();
        fork_store(&factory, &wide_root, &SessionId::from(fork_id)).await;
        wide_fork.push(started.elapsed());

        let mover_id = format!("{prefix}-wide-mover-{sample}");
        let mover = fork_store(&factory, &wide_root, &SessionId::from(mover_id.clone())).await;
        let mut mover_state = load_state(&mover, &SessionId::from(mover_id.clone())).await;
        mover_state.session_graph.append_plugin(
            "refcount-benchmark",
            serde_json::json!({ "sample": sample }),
        );
        let (commit, _) = RuntimeCommit::persisted_state_for_test(&mover_state, &[])
            .with_operation(operation(&SessionId::from(mover_id), "head-move"))
            .expect("stamp wide head move");
        let started = Instant::now();
        mover
            .commit_runtime_state(commit)
            .await
            .expect("commit wide head move");
        wide_head_move.push(started.elapsed());

        let victim_id = format!("{prefix}-wide-victim-{sample}");
        let victim = fork_store(&factory, &wide_root, &SessionId::from(victim_id.clone())).await;
        append_child(
            &victim,
            &SessionId::from(victim_id.clone()),
            "wide-delete-child",
        )
        .await;
        let started = Instant::now();
        factory
            .delete_session(&SessionId::from(victim_id))
            .await
            .expect("delete wide victim");
        wide_delete.push(started.elapsed());

        let deep_fork_id = format!("{prefix}-deep-fork-{sample}");
        let started = Instant::now();
        fork_store(&factory, &deep_leaf, &SessionId::from(deep_fork_id)).await;
        deep_fork.push(started.elapsed());

        let deep_mover_id = format!("{prefix}-deep-mover-{sample}");
        let deep_mover = fork_store(
            &factory,
            &deep_leaf,
            &SessionId::from(deep_mover_id.clone()),
        )
        .await;
        let mut deep_mover_state =
            load_state(&deep_mover, &SessionId::from(deep_mover_id.clone())).await;
        deep_mover_state.session_graph.append_plugin(
            "refcount-benchmark",
            serde_json::json!({ "sample": sample }),
        );
        let (commit, _) = RuntimeCommit::persisted_state_for_test(&deep_mover_state, &[])
            .with_operation(operation(&SessionId::from(deep_mover_id), "head-move"))
            .expect("stamp deep head move");
        let started = Instant::now();
        deep_mover
            .commit_runtime_state(commit)
            .await
            .expect("commit deep head move");
        deep_head_move.push(started.elapsed());

        let deep_victim_id = format!("{prefix}-deep-victim-{sample}");
        create_chain(
            &factory,
            &SessionId::from(deep_victim_id.clone()),
            DEEP_CHAIN_DEPTH,
        )
        .await;
        let started = Instant::now();
        factory
            .delete_session(&SessionId::from(deep_victim_id))
            .await
            .expect("delete deep victim");
        deep_delete.push(started.elapsed());

        let started = Instant::now();
        let terminal_id =
            SessionId::from(format!("{prefix}-fork-chain-{}", DEEP_FORK_CHAIN_DEPTH - 1));
        let page = fork_chain_terminal
            .load_ancestors(
                &terminal_id,
                HistoryAnchor::Node(fork_chain_root.clone().into()),
                HistoryBudget {
                    max_nodes: std::num::NonZeroU32::new(1).expect("one is nonzero"),
                    max_bytes: std::num::NonZeroU64::new(u64::MAX).expect("max is nonzero"),
                },
            )
            .await
            .expect("load fork-chain root");
        assert_eq!(page.nodes.len(), 1);
        fork_chain_load_node.push(started.elapsed());

        let started = Instant::now();
        fork_chain_terminal
            .load_session_window(&terminal_id, WindowSelector::Current)
            .await
            .expect("load terminal fork-chain session")
            .expect("terminal fork-chain session exists");
        fork_chain_load_session.push(started.elapsed());

        let fork_id = format!("{prefix}-fork-chain-probe-{sample}");
        let started = Instant::now();
        fork_store(&factory, &fork_chain_leaf, &SessionId::from(fork_id)).await;
        fork_chain_fork.push(started.elapsed());
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
    print_samples(
        backend,
        "fork_chain",
        "load_root_node",
        DEEP_FORK_CHAIN_DEPTH,
        &mut fork_chain_load_node,
    );
    print_samples(
        backend,
        "fork_chain",
        "load_session",
        DEEP_FORK_CHAIN_DEPTH,
        &mut fork_chain_load_session,
    );
    print_samples(
        backend,
        "fork_chain",
        "fork",
        DEEP_FORK_CHAIN_DEPTH,
        &mut fork_chain_fork,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "noisy measurement benchmark; repeat runs (fork-chain medians can swing by 10x); requires LASH_POSTGRES_DATABASE_URL"]
async fn measured_refcount_replacement_operations() {
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .expect("set LASH_POSTGRES_DATABASE_URL to run the benchmark");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect benchmark Postgres");
    let sqlite_dir = tempfile::tempdir().expect("SQLite benchmark directory");
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let sqlite_memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory store set");
    let sqlite_file = lash_sqlite_store::SqliteStoreSet::open(sqlite_dir.path())
        .await
        .expect("open SQLite file store set");
    let backends: Vec<(&str, Arc<dyn DeploymentStore>)> = vec![
        (
            "sqlite_memory",
            sqlite_memory
                .open_store()
                .await
                .expect("open SQLite memory store") as Arc<dyn DeploymentStore>,
        ),
        (
            "sqlite",
            sqlite_file
                .open_store()
                .await
                .expect("open SQLite file store") as Arc<dyn DeploymentStore>,
        ),
        ("postgres", Arc::new(postgres.session_store_factory())),
    ];

    println!("backend,shape,operation,scale,median_ms,p95_ms,samples");
    for (backend, factory) in backends {
        benchmark_backend(backend, factory, &run_id).await;
    }
}
