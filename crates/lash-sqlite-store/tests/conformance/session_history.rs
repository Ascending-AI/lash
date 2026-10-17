use std::sync::{Arc, Mutex};

use lash_core_execution::store::ConformanceDeployment;

use crate::SUBSTRATE;
use crate::backend_fixture::TestBackend;

async fn catalog() -> (TestBackend, Arc<dyn ConformanceDeployment>) {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await as Arc<dyn ConformanceDeployment>;
    (backend, store)
}

#[tokio::test]
async fn window_is_frame_bounded() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_window_is_frame_bounded(store).await;
}

#[tokio::test]
async fn window_reads_only_appended_rows() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_window_reads_only_appended_rows(store).await;
}

#[tokio::test]
async fn pages_are_bounded_and_pinned() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_pages_are_bounded_and_pinned(store).await;
}

#[tokio::test]
async fn fork_respects_ceiling() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_fork_respects_ceiling(store).await;
}

#[tokio::test]
async fn inflated_fork_ceiling_cannot_expose_post_fork_source_nodes() {
    let (_backend, store) = catalog().await;
    lash_conformance::inflated_fork_ceiling_cannot_expose_post_fork_source_nodes(store).await;
}

#[tokio::test]
async fn admitted_and_current_reads_of_a_head_give_one_read_model() {
    let (_backend, store) = catalog().await;
    lash_conformance::admitted_and_current_reads_of_a_head_give_one_read_model(store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_selection_and_confirmation_share_one_snapshot() {
    let (_backend, store) = catalog().await;
    lash_conformance::history_selection_and_confirmation_share_one_snapshot(store).await;
}

#[tokio::test]
async fn graph_generation_overflow_rolls_back_every_write() {
    let (_backend, store) = catalog().await;
    lash_conformance::graph_generation_overflow_rolls_back_every_write(store).await;
}

#[tokio::test]
async fn a_later_frame_open_cannot_rescue_earlier_root_nodes() {
    let (_backend, store) = catalog().await;
    lash_conformance::a_later_frame_open_cannot_rescue_earlier_root_nodes(store).await;
}

#[tokio::test]
async fn window_rejects_corrupt_anchors() {
    let guards = Arc::new(Mutex::new(Vec::<TestBackend>::new()));
    lash_conformance::history_window_rejects_corrupt_anchors(|_| {
        let guards = Arc::clone(&guards);
        async move {
            let (backend, store) = catalog().await;
            guards
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(backend);
            store
        }
    })
    .await;
}

/// FIG-5961: preparation retains a window reference, so the encoded and
/// physical prelude sizes are bounded independently of transcript depth.
#[tokio::test]
async fn turn_prelude_bytes_do_not_grow_with_history_depth() {
    use lash_core_execution::EffectAddress;
    use lash_core_execution::ExecutionScope;
    use lash_core_execution::TurnPrelude;
    use lash_core_store::session_policy::SessionPolicy;
    use lash_core_store::session_state::RuntimeSessionState;
    use lash_core_store::store::RuntimeCommit;
    use lash_core_store::store::SessionCatalogStore;
    use lash_core_store::store::SessionCommitStore;
    use lash_execution_contracts::turn_prelude::TurnPreludeStore;
    use lash_protocol_types::MessageSequence;
    use lash_protocol_types::SessionId;
    use lash_protocol_types::session_model::Message;
    use lash_protocol_types::session_model::MessageRole;
    use lash_protocol_types::session_model::Part;
    let mut sizes = Vec::new();
    for depth in [8, 128] {
        let backend = TestBackend::open(SUBSTRATE).await;
        let store = backend.store().await;
        let session_id = SessionId::fixture("prelude-history");
        store
            .admit_session(
                &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
            )
            .await
            .expect("admit session");
        let mut state = RuntimeSessionState {
            session_id: session_id.clone(),
            ..RuntimeSessionState::fixture(SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_protocol_types::session_model::NoProgressBudget::bounded(12),
            ))
        };
        let message = |ordinal: usize| Message {
            id: format!("message-{ordinal:04}"),
            role: MessageRole::User,
            parts: Arc::new(vec![Part::text(
                format!("part-{ordinal:04}"),
                format!("history {ordinal:04}: {}", "old transcript ".repeat(40)),
                None,
            )]),
            origin: None,
            reply_marker: None,
        };
        state.ensure_agent_frame_initialized();
        for ordinal in 0..depth {
            state.session_graph.append_message(message(ordinal));
        }
        let commit = RuntimeCommit::persisted_state_for_test(&state);
        let receipt = store
            .commit_runtime_state(commit)
            .await
            .expect("commit history");
        state.apply_persisted_commit_result(receipt);
        let session_store = lash_core_store::store::SessionStore::new(store.clone(), session_id)
            .expect("session store");
        let loaded = lash_core_store::store::load_session_window_state(
            &session_store,
            lash_core_store::store::WindowSelector::Current,
        )
        .await
        .expect("window read")
        .expect("window");
        let mut state = loaded.state;
        let base = MessageSequence::from(state.read_model().messages.to_vec());
        let mut messages = base.to_vec();
        messages.push(message(9999));
        let history = MessageSequence::from(messages);
        let mut prompt = history.clone();
        prompt.make_mut()[0].parts = Arc::new(vec![Part::text(
            "omitted".into(),
            "recorded prompt omission".into(),
            None,
        )]);
        let scope = ExecutionScope::turn("prelude-history", "prelude-turn");
        let mut prelude = TurnPrelude::new(
            EffectAddress::new(scope.clone(), "turn-config").expect("address"),
            Vec::new(),
            lash_core_store::store::SessionHeadRef {
                generation: 0,
                revision: state.head_revision,
                leaf: state.session_graph.leaf_node_id.clone(),
                checkpoint: state.checkpoint_ref.clone(),
            },
            None,
        );
        prelude.capture_messages(&base, &history, &prompt);
        let reference = prelude
            .record(store.as_ref(), &scope)
            .await
            .expect("record prelude");
        let bytes = store
            .get_turn_prelude(&reference)
            .await
            .expect("read bytes")
            .expect("stored prelude");
        // Move the live head after recording. Restore must use the named
        // prefix, through a newly opened store handle, rather than Current.
        state.session_graph.append_message(message(8888));
        state.turn_index += 1;
        let later = RuntimeCommit::persisted_state_for_test(&state);
        store
            .commit_runtime_state(later)
            .await
            .expect("move the head");
        store
            .gc_unreachable()
            .await
            .expect("collect old revisions and checkpoints");
        let reopened = backend.reopen().await.store().await;
        let session_store =
            lash_core_store::store::SessionStore::new(reopened.clone(), state.session_id.clone())
                .expect("reopened session store");
        let restored = TurnPrelude::read(&reference, reopened.as_ref())
            .await
            .expect("restore prelude");
        let (_, restored_history, restored_context) = restored
            .restore(&session_store)
            .await
            .expect("restore views");
        assert_eq!(
            serde_json::to_vec(&restored_history).expect("history bytes"),
            serde_json::to_vec(&history).expect("original history")
        );
        assert_eq!(
            serde_json::to_vec(&restored_context.messages).expect("prompt bytes"),
            serde_json::to_vec(&prompt).expect("original prompt")
        );
        let physical: usize = backend.raw().query_row(
            "SELECT length(content) FROM blobs JOIN artifact_refs ON blob_ref = hash WHERE namespace = 'turn_prelude' AND artifact_ref = ?1",
            [reference.as_str()], |row| row.get(0)).expect("physical prelude bytes");
        sizes.push((bytes.len(), physical));
    }
    eprintln!("prelude bytes at history depths 8 and 128: {sizes:?}");
    assert_eq!(
        sizes[0].0, sizes[1].0,
        "prelude bytes must be independent of history depth"
    );
    assert!(
        sizes[0].1.abs_diff(sizes[1].1) <= 32,
        "compression of fixed-size references has a constant bound: {sizes:?}"
    );
}
