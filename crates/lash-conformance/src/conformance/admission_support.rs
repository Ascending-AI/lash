//! Shared fixtures for laws that settle a session's ingress rows (FIG-3927):
//! a row is open until a run binds it, and only that run's commit (or its
//! terminal) answers it.

use super::*;

/// `commit` applying the command rows `completion` names: the command
/// lane's bindless settlement (design §2.7).
pub(crate) fn applying_commands(
    mut commit: crate::RuntimeCommit,
    completion: crate::QueuedWorkCompletion,
) -> crate::RuntimeCommit {
    commit.applied_commands = Some(completion);
    commit
}

/// A bare commit over the store's current head for `session_id`: the base a
/// settling or run-ending commit is built on.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own head read"
)]
pub(crate) async fn head_commit(
    store: &Arc<dyn crate::RuntimeStore>,
    session_id: &crate::SessionId,
) -> crate::RuntimeCommit {
    let revision = store
        .load_session_head_meta(session_id)
        .await
        .expect("load the head")
        .map_or(0, |meta| meta.head_revision);
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        head_revision: revision,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    crate::RuntimeCommit::persisted_state_for_test(&state)
}
