//! FIG-869: stable boundary names do not imply semantic request replay.
//! FIG-2480: operations that adopt the `SemanticBoundary` receipt identity
//! replay rebuilt same-request retries and refuse differing canonical content.
//! Exercise real SQLite receipt adjudication with an intervening committed head.

use lash_core::{
    ExecutionScope, OperationId, RuntimeCommit, RuntimeSessionState, SessionCommitStore,
    SessionPolicy, StoreError, TurnBudget,
};
use lash_sqlite_store::Store;

fn commit(boundary: &str, key: &str, revision: u64) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: "root".into(),
        head_revision: revision,
        ..RuntimeSessionState::new(SessionPolicy::new(TurnBudget::Unbounded))
    };
    commit_state(boundary, key, &state)
}

fn commit_state(boundary: &str, key: &str, state: &RuntimeSessionState) -> RuntimeCommit {
    RuntimeCommit::persisted_state_for_test(state, &[])
        .with_operation(OperationId::new(
            ExecutionScope::runtime_operation(format!("session:root:boundary:{boundary}")),
            key,
        ))
        .expect("boundary operation")
        .0
}

fn semantic_commit_state(boundary: &str, key: &str, state: &RuntimeSessionState) -> RuntimeCommit {
    let mut commit = commit_state(boundary, key, state);
    commit
        .stamp_semantic_boundary()
        .expect("semantic-boundary stamp");
    commit
}

async fn semantic_boundary_retry_after_head_advance(boundary: &str, key: &str) {
    let directory = tempfile::tempdir().expect("database directory");
    let path = directory.path().join("session.db");
    let store = Store::open(&path).await.expect("SQLite store");
    let state = RuntimeSessionState {
        session_id: "root".into(),
        ..RuntimeSessionState::new(SessionPolicy::new(TurnBudget::Unbounded))
    };
    let first = semantic_commit_state(boundary, key, &state);
    let original = store
        .commit_runtime_state(first.clone())
        .await
        .expect("first commit");
    let mut advanced_state = lash_core::store::load_persisted_session_state(&store)
        .await
        .expect("load initial state")
        .expect("initial state");
    advanced_state.turn_index += 1;
    let advanced = store
        .commit_runtime_state(commit_state("intervening", "advance", &advanced_state))
        .await
        .expect("advance head");
    drop(store);
    let store = Store::open(&path).await.expect("reopen durable receipts");
    let replay = store
        .commit_runtime_state(first)
        .await
        .expect("exact retry");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, original.head_revision);
    assert_eq!(replay.checkpoint_ref, original.checkpoint_ref);
    // FIG-2480: a rebuilt same-request retry at the advanced head is answered
    // from durable receipt evidence, not refused for its moved commit hash.
    let loaded = lash_core::store::load_persisted_session_state(&store)
        .await
        .expect("load advanced state")
        .expect("advanced state");
    let rebuilt = store
        .commit_runtime_state(semantic_commit_state(boundary, key, &loaded))
        .await
        .expect("rebuilt same-request retry replays");
    assert!(rebuilt.receipt_replayed);
    assert_eq!(rebuilt.head_revision, original.head_revision);
    assert_eq!(rebuilt.checkpoint_ref, original.checkpoint_ref);
    // A non-retry with differing canonical content is refused, never silently
    // deduplicated into the stored receipt.
    let mut changed = commit_state(boundary, key, &loaded);
    changed.config.provider_id = "changed-provider".into();
    changed
        .stamp_semantic_boundary()
        .expect("semantic-boundary stamp");
    let refused = store.commit_runtime_state(changed).await;
    assert!(
        matches!(
            refused,
            Err(StoreError::SemanticBoundaryIdentityConflict { ref operation_key, .. })
                if operation_key == key
        ),
        "differing canonical encoding must be refused: {refused:?}"
    );
    assert_eq!(
        store
            .load_session()
            .await
            .expect("read head")
            .expect("session")
            .head_revision,
        advanced.head_revision,
        "neither retry nor refusal may advance or rewind the durable head"
    );
}

#[tokio::test]
async fn record_config_retry_after_head_advance() {
    semantic_boundary_retry_after_head_advance("protocol-materialization", "record-config").await;
}

#[tokio::test]
async fn create_session_retry_after_head_advance() {
    semantic_boundary_retry_after_head_advance("root", "create-session").await;
}

#[tokio::test]
async fn usage_ledger_retry_after_head_advance() {
    semantic_boundary_retry_after_head_advance("child-turn", "usage-ledger").await;
}

#[tokio::test]
async fn usage_ledger_retry_with_staged_usage_after_head_advance() {
    let directory = tempfile::tempdir().expect("database directory");
    let path = directory.path().join("session.db");
    let store = Store::open(&path).await.expect("SQLite store");
    let entry = |source: &str| lash_core::TokenLedgerEntry {
        source: source.into(),
        model: "ledger-model".into(),
        usage: lash_core::TokenUsage::default(),
        usage_disposition: Default::default(),
    };
    let usage_commit = |state: &RuntimeSessionState, source: &str| {
        let mut commit = commit_state("child-turn", "usage-ledger", state);
        commit.usage_deltas = lash_core::store::RuntimeUsageDelta::for_operation(
            &commit.turn_commit.operation,
            &[entry(source)],
        )
        .expect("usage delta identities");
        commit
            .stamp_semantic_boundary()
            .expect("semantic-boundary stamp");
        commit
    };
    let state = RuntimeSessionState {
        session_id: "root".into(),
        ..RuntimeSessionState::new(SessionPolicy::new(TurnBudget::Unbounded))
    };
    let first = usage_commit(&state, "child-turn-usage");
    let original = store
        .commit_runtime_state(first.clone())
        .await
        .expect("first usage flush");
    let mut advanced_state = lash_core::store::load_persisted_session_state(&store)
        .await
        .expect("load initial state")
        .expect("initial state");
    advanced_state.turn_index += 1;
    let advanced = store
        .commit_runtime_state(commit_state("intervening", "advance", &advanced_state))
        .await
        .expect("advance head");
    // FIG-2480: a rebuilt retry carrying the same staged usage is answered from
    // durable receipt evidence and publishes no second ledger row.
    let loaded = lash_core::store::load_persisted_session_state(&store)
        .await
        .expect("load advanced state")
        .expect("advanced state");
    let rebuilt = store
        .commit_runtime_state(usage_commit(&loaded, "child-turn-usage"))
        .await
        .expect("rebuilt same-usage retry replays");
    assert!(rebuilt.receipt_replayed);
    assert_eq!(rebuilt.head_revision, original.head_revision);
    assert_eq!(
        rebuilt.committed_usage_delta_identities, original.committed_usage_delta_identities,
        "replay must confirm the originally committed usage identities"
    );
    // Differing staged usage under the same boundary is a different request:
    // refused, never deduplicated into the stored receipt.
    let refused = store
        .commit_runtime_state(usage_commit(&loaded, "child-turn-usage-changed"))
        .await;
    assert!(
        matches!(
            refused,
            Err(StoreError::SemanticBoundaryIdentityConflict { ref operation_key, .. })
                if operation_key == "usage-ledger"
        ),
        "differing staged usage must be refused: {refused:?}"
    );
    assert_eq!(
        store
            .load_session()
            .await
            .expect("read head")
            .expect("session")
            .head_revision,
        advanced.head_revision,
        "neither retry nor refusal may advance or rewind the durable head"
    );
}

#[tokio::test]
async fn initial_park_exact_commit_retry_after_head_advance() {
    let directory = tempfile::tempdir().expect("database directory");
    let store = Store::open(&directory.path().join("session.db"))
        .await
        .expect("SQLite store");
    let mut state = RuntimeSessionState {
        session_id: "root".into(),
        ..RuntimeSessionState::new(SessionPolicy::new(TurnBudget::Unbounded))
    };
    // Mirror lifecycle's content-addressed operation for an empty pending graph.
    // Its private unit test also pins derivation for a nonempty graph.
    let park_commit = |state: &RuntimeSessionState| {
        let preview = commit_state("initial-park-preview", "preview", state);
        let content_hash = preview.turn_commit_hash().expect("preview content hash");
        commit_state(&format!("content:{content_hash}"), "initial-park", state)
    };
    let first = park_commit(&state);
    assert!(
        store
            .load_session()
            .await
            .expect("preview has no store effects")
            .is_none()
    );
    let original = store
        .commit_runtime_state(first.clone())
        .await
        .expect("park");
    state.head_revision = original.head_revision;
    state.turn_index += 1;
    let advanced = store
        .commit_runtime_state(commit_state("intervening", "advance", &state))
        .await
        .expect("advance");
    let replay = store
        .commit_runtime_state(first.clone())
        .await
        .expect("exact park retry");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, original.head_revision);
    state.head_revision = advanced.head_revision;
    state.turn_index = 0;
    let head_only = park_commit(&state);
    assert_eq!(head_only.turn_commit.operation, first.turn_commit.operation);
    assert!(
        store
            .commit_runtime_state(head_only)
            .await
            .expect("head-only retry")
            .receipt_replayed
    );
    state.turn_index = 2;
    let changed = park_commit(&state);
    assert_ne!(changed.turn_commit.operation, first.turn_commit.operation);
    let fresh = store
        .commit_runtime_state(changed)
        .await
        .expect("different content is a new park");
    assert!(!fresh.receipt_replayed);
    assert_eq!(fresh.head_revision, advanced.head_revision + 1);
}

#[tokio::test]
async fn append_identity_replays_after_head_advance() {
    let directory = tempfile::tempdir().expect("database directory");
    let store = Store::open(&directory.path().join("session.db"))
        .await
        .expect("SQLite store");
    let mut state = RuntimeSessionState {
        session_id: "root".into(),
        ..RuntimeSessionState::new(SessionPolicy::new(TurnBudget::Unbounded))
    };
    let nodes = vec![lash_core::SessionAppendNode::plugin(
        "audit",
        serde_json::json!({"value": 1}),
    )];
    let first = lash_core::store::append_request_commit_with_clock_for_testing(
        &mut state,
        "append-audit",
        &nodes,
        None,
        &lash_core::facade_support::SystemClock,
    )
    .expect("append identity");
    let original = store.commit_runtime_state(first).await.expect("append");
    let mut advanced = lash_core::store::load_persisted_session_state(&store)
        .await
        .expect("load")
        .expect("state");
    advanced.turn_index += 1;
    let advanced_receipt = store
        .commit_runtime_state(commit_state("intervening", "advance", &advanced))
        .await
        .expect("advance head");
    let mut loaded = lash_core::store::load_persisted_session_state(&store)
        .await
        .expect("reload")
        .expect("state");
    let retry = lash_core::store::append_request_commit_with_clock_for_testing(
        &mut loaded,
        "append-audit",
        &nodes,
        None,
        &lash_core::facade_support::SystemClock,
    )
    .expect("retry identity");
    let replay = store
        .commit_runtime_state(retry)
        .await
        .expect("semantic append retry");
    assert!(replay.receipt_replayed);
    assert_eq!(
        replay.committed_leaf_node_id,
        original.committed_leaf_node_id
    );
    assert_eq!(replay.head_revision, original.head_revision);
    assert_eq!(
        store
            .load_session()
            .await
            .expect("head")
            .expect("session")
            .head_revision,
        advanced_receipt.head_revision
    );
}

#[tokio::test]
async fn non_append_operations_refuse_append_identity_metadata() {
    let directory = tempfile::tempdir().expect("database directory");
    let store = Store::open(&directory.path().join("session.db"))
        .await
        .expect("SQLite store");
    for key in [
        "initial-park",
        "record-config",
        "create-session",
        "usage-ledger",
    ] {
        let mut attempted = commit("identity-adoption", key, 0);
        attempted.turn_commit.append_request_identity = lash_core::AppendRequestIdentity::Append {
            encoding_version: 1,
            request_hash: "non-append-semantic-request".into(),
            requested_node_count: 0,
            requested_ancestor_node_id: None,
        };
        let error = store
            .commit_runtime_state(attempted)
            .await
            .expect_err("append identity is operation-specific");
        assert!(
            matches!(error, StoreError::Backend(ref message)
            if message == &format!("append receipt identity metadata is invalid for operation `{key}`")),
            "unexpected adoption refusal: {error:?}"
        );
    }
    assert!(
        store
            .load_session()
            .await
            .expect("unchanged store")
            .is_none()
    );
}
