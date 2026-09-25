//! FIG-3682: the head a turn was admitted on stays readable until the
//! session's next admission.
//!
//! A replay of an admitted turn rebuilds the turn's input state from the head
//! it was admitted on (`load_session_at`). The turn's own commit supersedes
//! that head, and garbage collection reclaims superseded checkpoints, so the
//! admission retains its base: collection keeps the base checkpoint while it
//! is the session's latest admission, and releases it once the next admission
//! retains another.

use super::*;
use pretty_assertions::assert_eq;

/// Commit a checkpoint whose tool state carries `generation`, returning its
/// receipt.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_generation(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    head_revision: u64,
    generation: u64,
    owner: &str,
) -> crate::store::RuntimeCommitReceipt {
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(generation),
    ));
    commit_runtime_state_for_test(
        store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        owner,
    )
    .await
    .expect("commit the checkpoint")
}

/// The head a commit's receipt names, as an admission records it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn admitted_on(
    store: &Arc<dyn RuntimePersistence>,
    receipt: &crate::store::RuntimeCommitReceipt,
) -> crate::store::SessionHeadRef {
    crate::store::SessionHeadRef {
        generation: store
            .read_session_state_version()
            .await
            .expect("read the session-state generation"),
        revision: receipt.head_revision,
        leaf: receipt.committed_leaf_node_id.clone(),
        checkpoint: Some(receipt.checkpoint_ref.clone()),
    }
}

/// Retain `base` as the session's latest admission, under a lease of its own.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn admit_on(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    base: &crate::store::SessionHeadRef,
) {
    let lease =
        claim_session_execution_lease_for_test(store, session_id, "admission-base-retention").await;
    store
        .retain_admission_base(&lease.fence(), base)
        .await
        .expect("retain the admission's base");
    store
        .release_session_execution_lease(&lease.completion())
        .await
        .expect("release the admission's lease");
}

/// The tool-state generation the checkpoint of `read` carries.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn generation_of(read: crate::store::PersistedSessionRead) -> Option<u64> {
    read.checkpoint
        .and_then(|checkpoint| {
            checkpoint
                .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
                .expect("decode the checkpoint's tool state")
        })
        .map(|tool_state| tool_state.generation())
}

/// An admission's base survives garbage collection after the turn's commit
/// superseded it, and is released once the next admission retains another.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_admission_base_survives_collection_until_the_next_admission(
    factory: ReopenableRuntimePersistence,
) {
    let store = factory.open;
    let session_id = SessionId::from("admission-base-retention");
    let first = commit_generation(&store, &session_id, 0, 1, "admission-base-v1").await;
    let base = admitted_on(&store, &first).await;

    // The turn is admitted on the first head; its commit supersedes it.
    admit_on(&store, &session_id, &base).await;
    let second = commit_generation(
        &store,
        &session_id,
        first.head_revision,
        2,
        "admission-base-v2",
    )
    .await;
    assert_ne!(
        second.checkpoint_ref, first.checkpoint_ref,
        "the turn's commit wrote another checkpoint"
    );
    for sweep in ["first", "second"] {
        store
            .gc_unreachable()
            .await
            .unwrap_or_else(|error| panic!("{sweep} collection: {error}"));
    }
    let replayed = store
        .load_session_at(&base)
        .await
        .expect("the admitted turn's base survives collection");
    assert_eq!(replayed.head_revision, first.head_revision);
    assert_eq!(replayed.checkpoint_ref, Some(first.checkpoint_ref.clone()));
    assert_eq!(
        generation_of(replayed),
        Some(1),
        "the base reads as the head the turn was admitted on"
    );
    assert_eq!(
        generation_of(
            store
                .load_session()
                .await
                .expect("load the live head")
                .expect("the session exists")
        ),
        Some(2),
        "the live head is the turn's commit"
    );

    // The next turn is admitted on the second head: the first base is
    // released, and collection reclaims it.
    let next_base = admitted_on(&store, &second).await;
    admit_on(&store, &session_id, &next_base).await;
    let report = store
        .gc_unreachable()
        .await
        .expect("collect after the next admission");
    assert!(
        report.deleted_blob_count >= 1,
        "the released base is reclaimed: {report:?}"
    );
    let error = store
        .load_session_at(&base)
        .await
        .expect_err("a released base is no longer retained");
    assert!(
        matches!(error, crate::StoreError::TurnBaseNotRetained { revision } if revision == first.head_revision),
        "a base the store no longer holds is refused typed: {error:?}"
    );
    assert_eq!(
        generation_of(
            store
                .load_session_at(&next_base)
                .await
                .expect("the next admission's base is retained")
        ),
        Some(2)
    );
}
