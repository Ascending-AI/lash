//! The turn half of the parent-end ledger: the row a turn or a session
//! records for its own scope, and the fence that row is.
//!
//! The sibling laws in [`super::parent_end`] reach the ledger through a
//! *process* parent's terminal write, which is the one path that writes a row
//! as a side effect of another fact. A turn cannot end that way — a turn is
//! not a process row — so `record_parent_end` is its only writer.

use super::*;
use pretty_assertions::assert_eq;

fn turn_scope(session: &SessionId, turn: &str) -> lash_core::ScopeId {
    lash_core::ScopeId::turn(session.clone(), crate::TurnId::fixture(turn))
}

/// How a child started under a scope lives relative to it.
#[derive(Clone, Copy)]
enum Lives {
    /// `Until` the scope that started it: its end cancels the child.
    Until,
    /// `Detached`: started there, owed nothing when it ends.
    Detached,
}

async fn register_child(
    registry: &Arc<dyn ProcessRegistry>,
    originator: &SessionScope,
    starter: &lash_core::ScopeId,
    lives: Lives,
) -> Result<ProcessRecord, crate::PluginError> {
    let registration = lash_core::testing::held_engine_registration(
        serde_json::Value::Null,
        ProcessProvenance::session(originator.clone()),
        lash_core::Lifetime::Detached,
    );
    registry
        .register_process(match lives {
            Lives::Until => crate::started_until_starter(registration, starter.clone()),
            Lives::Detached => crate::started_detached(registration, starter.clone()),
        })
        .await
}

/// A turn scope ends through the row it records for itself, and that row
/// fences and scopes exactly as a process scope's does.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_turn_scope_ends_through_its_recorded_ledger_row(
    registry: Arc<dyn ProcessRegistry>,
) {
    let session = SessionId::from("turn-parent-end-session");
    let originator = SessionScope::new(session.clone());
    let turn = turn_scope(&session, "turn-parent-end-turn");
    let other_turn = turn_scope(&session, "turn-parent-end-other-turn");

    let mut children = Vec::new();
    for (parent, lives) in [
        (&turn, Lives::Until),
        (&turn, Lives::Until),
        (&turn, Lives::Detached),
        (&other_turn, Lives::Until),
    ] {
        children.push(
            register_child(&registry, &originator, parent, lives)
                .await
                .expect("register a child under a live turn scope")
                .id,
        );
    }
    assert_eq!(children.len(), 4, "four registered children");

    assert!(
        registry
            .get_parent_end_plan(&turn)
            .await
            .expect("read the ledger row of a turn that has not ended")
            .is_none(),
        "a live turn owns no ledger row"
    );

    registry
        .record_parent_end(&turn)
        .await
        .expect("record the turn's end");
    let recorded = registry
        .get_parent_end_plan(&turn)
        .await
        .expect("read the recorded ledger row")
        .expect("recording the end writes the row");
    assert_eq!(
        recorded.parent, turn,
        "the row is keyed by the scope it ends"
    );
    registry
        .record_parent_end(&turn)
        .await
        .expect("recording the same end again is a no-op, not a conflict");
    assert_eq!(
        registry
            .get_parent_end_plan(&turn)
            .await
            .expect("re-read the ledger row after a repeated record")
            .expect("the row survives a repeated record"),
        recorded,
        "repetition preserves the first ending rather than restamping it"
    );
    assert!(
        registry
            .get_parent_end_plan(&other_turn)
            .await
            .expect("read the other turn's ledger row")
            .is_none(),
        "the turn that never ended has none"
    );

    assert!(
        matches!(
            register_child(&registry, &originator, &turn, Lives::Until,).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a Until child registering after the turn's row exists is refused"
    );
    assert!(
        matches!(
            register_child(&registry, &originator, &turn, Lives::Detached).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a Detached child owes the ended turn nothing, but an ended scope \
         starts nothing: its starter's close row refuses it too"
    );

    register_child(&registry, &originator, &other_turn, Lives::Until)
        .await
        .expect("a turn that has not ended still admits its children");
}

/// Two scopes whose components rendered to the same stored id under the
/// retired `{session}/{turn}` codec must share nothing: not a ledger key, not
/// a fence. `("collision-session/a", "c")` and `("collision-session", "a/c")`
/// were one `parent_id` before FIG-3418 — one scope's end would have fenced
/// the other's starts. The canonical
/// projection is injective, so they are independent scopes end to end.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn scopes_that_collide_in_rendering_share_no_ledger_key(
    registry: Arc<dyn ProcessRegistry>,
) {
    let first_originator = SessionScope::new("collision-session/a");
    let second_originator = SessionScope::new("collision-session");
    let first = lash_core::ScopeId::turn(
        SessionId::from("collision-session/a"),
        crate::TurnId::from("c"),
    );
    let second = lash_core::ScopeId::turn(
        SessionId::from("collision-session"),
        crate::TurnId::from("a/c"),
    );
    assert_ne!(
        first.storage_id(),
        second.storage_id(),
        "the index projection is injective where the rendered id was not"
    );

    registry
        .record_parent_end(&first)
        .await
        .expect("end only the first scope");
    let recorded = registry
        .get_parent_end_plan(&first)
        .await
        .expect("read the first scope's ledger row")
        .expect("recording the end writes the row");
    assert_eq!(
        recorded.parent, first,
        "the row decodes to the typed scope it ended, not a rendered id"
    );
    assert!(
        registry
            .get_parent_end_plan(&second)
            .await
            .expect("read the second scope's ledger row")
            .is_none(),
        "a scope that renders identically must not alias the ended scope's row"
    );

    assert!(
        matches!(
            register_child(&registry, &first_originator, &first, Lives::Until).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "the ended scope refuses a late start"
    );
    register_child(&registry, &second_originator, &second, Lives::Until)
        .await
        .expect("a rendering-identical scope is not fenced by the other's end");
}

/// A turn scope that never became a run — the turn id of an input that
/// joined an earlier run — has no terminal, so no run close ever records
/// its row. Its session's close is the proof that it can no longer become a
/// run (FIG-3948): from the session's row on, a start naming any scope
/// inside the session is refused.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_session_close_fences_the_turn_scopes_that_never_became_runs(
    registry: Arc<dyn ProcessRegistry>,
) {
    let session = SessionId::from("never-run-session");
    let originator = SessionScope::new(session.clone());
    let session_scope = lash_core::ScopeId::session(session.clone());
    let run = turn_scope(&session, "never-run-admitted");
    let joined = turn_scope(&session, "never-run-joined");
    let drain = lash_core::ScopeId::session_operation(session.clone(), "never-run-drain");
    let other_session = SessionId::from("never-run-session-other");
    let foreign = turn_scope(&other_session, "never-run-joined");

    register_child(&registry, &originator, &joined, Lives::Until)
        .await
        .expect("an open session admits a start under a turn it may still admit");
    register_child(&registry, &originator, &drain, Lives::Until)
        .await
        .expect("register a child under a drain that recorded no end");

    // What the session's close records: the admitted runs' rows, then the
    // session's own.
    registry
        .record_parent_end(&run)
        .await
        .expect("close the admitted run's scope");
    registry
        .record_parent_end(&session_scope)
        .await
        .expect("close the session's scope");
    assert!(
        registry
            .get_parent_end_plan(&joined)
            .await
            .expect("read the never-run turn's ledger row")
            .is_none(),
        "no run close ever records a row for a turn that never became a run"
    );

    for (scope, lives) in [
        (&joined, Lives::Until),
        (&joined, Lives::Detached),
        (&turn_scope(&session, "never-run-late"), Lives::Until),
    ] {
        match register_child(&registry, &originator, scope, lives).await {
            Err(crate::PluginError::ParentEnded { parent, .. }) => assert_eq!(
                parent, session_scope,
                "the session's row is what refuses a start under `{scope}`"
            ),
            other => {
                panic!("a start under `{scope}` of a closed session must be refused, got {other:?}")
            }
        }
    }
    register_child(
        &registry,
        &SessionScope::new(other_session.clone()),
        &foreign,
        Lives::Until,
    )
    .await
    .expect("another session's turns are not fenced by this session's close");
}
