//! The turn half of the parent-end ledger: the row a committed turn records
//! for itself, and the recovery read that re-derives it after a crash.
//!
//! The sibling laws in [`super::parent_end`] reach the ledger through a
//! *process* parent's terminal write, which is the one path that writes a row
//! as a side effect of another fact. A turn cannot end that way — a turn is
//! not a process row — so `record_parent_end` is its only writer, and
//! `list_unrecorded_opener_parents` is the only way a crash between the turn
//! commit and that write is ever noticed. Neither had a shared law: both were
//! asserted on one backend's own tests, so a tier could implement either one
//! differently, or inherit the empty-page default, and stay green.

use super::*;
use pretty_assertions::assert_eq;

const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(16).expect("page bound");

fn turn_scope(session: &SessionId, turn: &str) -> lash_core::ScopeId {
    lash_core::ScopeId::turn(session.clone(), crate::TurnId::from(turn))
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
    let registration = ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
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
/// fences, scopes and settles exactly as a process scope's does.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_turn_scope_ends_through_its_recorded_ledger_row(
    registry: Arc<dyn ProcessRegistry>,
) {
    let session = SessionId::from("turn-parent-end-session");
    let originator = SessionScope::new(session.as_str());
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
    let [cancel_a, cancel_b, _abandon, _other_turn_child] =
        <[crate::ProcessId; 4]>::try_from(children).expect("four registered children");

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
    assert!(
        recorded.settled_at_ms.is_none(),
        "a freshly recorded row is unsettled"
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
    let pending = registry
        .get_parent_end_plan(&turn)
        .await
        .expect("read the ledger row after the repeated record")
        .expect("the repeat left the one row");
    assert!(
        pending.settled_at_ms.is_none()
            && pending.obligation_state == Some(lash_core::store::ObligationState::Due),
        "the repeat kept the first row unsettled with its obligation still due"
    );
    assert!(
        registry
            .get_parent_end_plan(&other_turn)
            .await
            .expect("read the other turn's ledger row")
            .is_none(),
        "the turn that never ended has none"
    );

    assert_eq!(
        registry
            .list_parent_end_children(&turn, None, PAGE)
            .await
            .expect("page the turn's pending-cancel children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![cancel_a.clone(), cancel_b.clone()],
        "the sweep sees this turn's Until children only: not its Detached child, \
         and not another turn's"
    );
    assert_eq!(
        registry
            .list_parent_end_children(&turn, Some(&cancel_a), PAGE)
            .await
            .expect("resume the children page after the first child")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![cancel_b],
        "the children page resumes strictly after the cursor"
    );

    assert!(
        matches!(
            register_child(&registry, &originator, &turn, Lives::Until,).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a Until child registering after the turn's row exists is refused, \
         because the sweep that would have cancelled it has already run"
    );
    assert!(
        matches!(
            register_child(&registry, &originator, &turn, Lives::Detached).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a Detached child owes the ended turn nothing, but an ended scope \
         starts nothing: its starter's close row refuses it too"
    );

    registry
        .settle_parent_end_plan(&turn)
        .await
        .expect("settle the turn's ledger row");
    let settled = registry
        .get_parent_end_plan(&turn)
        .await
        .expect("read the settled row")
        .expect("settlement stamps the row rather than deleting the fence");
    assert!(
        settled.settled_at_ms.is_some(),
        "the row records its settlement"
    );
    registry
        .record_parent_end(&turn)
        .await
        .expect("recording an already-settled end is a no-op, not a reopening");
    assert_eq!(
        registry
            .get_parent_end_plan(&turn)
            .await
            .expect("re-read the settled row after a repeated record")
            .expect("the settled row survives"),
        settled,
        "a repeat on a settled row neither reopens it nor restamps its ending"
    );
    assert_eq!(
        registry
            .get_parent_end_plan(&turn)
            .await
            .expect("read the settled row after the repeated record")
            .expect("the settled row survives")
            .obligation_state,
        Some(lash_core::store::ObligationState::Delivered),
        "the settle delivered the obligation the row owed, and a repeated \
         record does not re-arm it"
    );
}

/// Two scopes whose components rendered to the same stored id under the
/// retired `{session}/{turn}` codec must share nothing: not a ledger key, not
/// a children page, not a fence. `("collision-session/a", "c")` and
/// `("collision-session", "a/c")` were one `parent_id` before FIG-3418 — one
/// scope's end would have swept the other's children. The canonical
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

    let first_child = register_child(&registry, &first_originator, &first, Lives::Until)
        .await
        .expect("register a Until child under the first colliding scope");
    let second_child = register_child(&registry, &second_originator, &second, Lives::Until)
        .await
        .expect("register a Until child under the second colliding scope");

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

    assert_eq!(
        registry
            .list_parent_end_children(&first, None, PAGE)
            .await
            .expect("page the ended scope's children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![first_child.id.clone()],
        "the sweep sees only the ended scope's own children"
    );
    assert_eq!(
        registry
            .list_parent_end_children(&second, None, PAGE)
            .await
            .expect("page the surviving scope's children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![second_child.id.clone()],
        "children of a rendering-identical scope are not swept by the other's end"
    );
}

/// A turn scope that never became a root — the turn id of an input that
/// joined an earlier root — has no terminal, so no root close ever records
/// its row. Its session's close is the proof that it can no longer become a
/// root (FIG-3948): from the session's row on, a start naming any scope
/// inside the session is refused, and the session's plan owes a cancel to
/// every live `Until` child of a scope inside it that has no row of its own.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_session_close_reaps_the_turn_scopes_that_never_became_roots(
    registry: Arc<dyn ProcessRegistry>,
) {
    let session = SessionId::from("never-root-session");
    let originator = SessionScope::new(session.as_str());
    let session_scope = lash_core::ScopeId::session(session.clone());
    let root = turn_scope(&session, "never-root-admitted");
    let joined = turn_scope(&session, "never-root-joined");
    let drain = lash_core::ScopeId::queue_drain(session.clone(), "never-root-drain");
    let other_session = SessionId::from("never-root-session-other");
    let foreign = turn_scope(&other_session, "never-root-joined");

    let root_child = register_child(&registry, &originator, &root, Lives::Until)
        .await
        .expect("register a child under the admitted root");
    let joined_child = register_child(&registry, &originator, &joined, Lives::Until)
        .await
        .expect("an open session admits a start under a turn it may still admit");
    let joined_detached = register_child(&registry, &originator, &joined, Lives::Detached)
        .await
        .expect("register a detached child the never-root turn started");
    let drain_child = register_child(&registry, &originator, &drain, Lives::Until)
        .await
        .expect("register a child under a drain that recorded no end");
    let foreign_child = register_child(
        &registry,
        &SessionScope::new(other_session.as_str()),
        &foreign,
        Lives::Until,
    )
    .await
    .expect("register a child under another session's turn");

    // What the session's close intent does through a record-only scope
    // owner: the admitted roots' rows, then the session's own.
    lash_core::engine::ScopeCloseSink::close_session_scope(
        &crate::RegistryScopeClose::new(
            Arc::clone(&registry),
            Arc::new(crate::facade_support::SystemClock),
        ),
        &session,
        lash_core::store::ControlIntentId::from_sequence(1),
        &[crate::TurnId::from("never-root-admitted")],
    )
    .await
    .expect("close the session's scope");

    assert!(
        registry
            .get_parent_end_plan(&joined)
            .await
            .expect("read the never-root turn's ledger row")
            .is_none(),
        "no root close ever records a row for a turn that never became a root"
    );
    let owed = registry
        .get_parent_end_plan(&session_scope)
        .await
        .expect("read the session's close row")
        .expect("the close records the session's row");
    assert!(
        owed.settled_at_ms.is_none()
            && owed.obligation_state == Some(lash_core::store::ObligationState::Due),
        "the session's plan owes the never-root turn's child its cancel, so it is \
         not settled as childless: {owed:?}"
    );
    let mut expected = vec![joined_child.id.clone(), drain_child.id.clone()];
    expected.sort();
    assert_eq!(
        registry
            .list_parent_end_children(&session_scope, None, PAGE)
            .await
            .expect("page the closed session's children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        expected,
        "the session's plan owes every live Until child of a scope inside it with no \
         row of its own: not the root's child, which the root's own row owes; not the \
         detached child; not another session's"
    );

    for (scope, lives) in [
        (&joined, Lives::Until),
        (&joined, Lives::Detached),
        (&turn_scope(&session, "never-root-late"), Lives::Until),
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
        &SessionScope::new(other_session.as_str()),
        &foreign,
        Lives::Until,
    )
    .await
    .expect("another session's turns are not fenced by this session's close");

    // The relay's delivery of the session's `ParentEnd` obligation.
    let work = crate::NoProcessWork::for_registry(Arc::clone(&registry));
    let applied = crate::apply_parent_end_plan(registry.as_ref(), &work, &session_scope, 1)
        .await
        .expect("apply the session's plan");
    assert_eq!(applied.delivered, 2, "one cancel per owed child");
    for child in [&joined_child, &drain_child] {
        let cancel = registry
            .get_process(&child.id)
            .await
            .expect("read a reaped child")
            .expect("the reaped child is retained")
            .cancel_request
            .expect("the session's plan requested the child's cancel");
        assert_eq!(
            (cancel.origin, cancel.requester),
            (
                crate::CancelOrigin::ParentEnded,
                crate::parent_end_requester(&session_scope)
            ),
            "the session's end is what cancels a child of a scope inside it"
        );
    }
    for untouched in [&root_child, &joined_detached, &foreign_child] {
        assert!(
            registry
                .get_process(&untouched.id)
                .await
                .expect("read an untouched child")
                .expect("the untouched child is retained")
                .cancel_request
                .is_none(),
            "the session's plan leaves `{}` alone",
            untouched.id
        );
    }
    assert!(
        registry
            .get_parent_end_plan(&session_scope)
            .await
            .expect("re-read the session's close row")
            .expect("the row survives its application")
            .settled_at_ms
            .is_some(),
        "the applied session plan settles"
    );
    assert!(
        !registry
            .list_unrecorded_opener_parents(None, PAGE)
            .await
            .expect("page unrecorded opener parents")
            .iter()
            .any(|scope| scope == &joined || scope == &drain),
        "a reaped never-root scope owes recovery nothing"
    );
}

/// A turn whose commit outran its ledger row is reported as a recovery
/// candidate until the row exists — and nothing else is.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn an_unrecorded_turn_parent_is_reported_until_its_row_is_written(
    registry: Arc<dyn ProcessRegistry>,
) {
    let session = SessionId::from("unrecorded-turn-session");
    let originator = SessionScope::new(session.as_str());
    let first = turn_scope(&session, "unrecorded-turn-a");
    let second = turn_scope(&session, "unrecorded-turn-b");
    let abandon_only = turn_scope(&session, "unrecorded-turn-c");

    let first_child = register_child(&registry, &originator, &first, Lives::Until)
        .await
        .expect("register the first turn's Until child");
    let second_child = register_child(&registry, &originator, &second, Lives::Until)
        .await
        .expect("register the second turn's Until child");
    let sibling = register_child(&registry, &originator, &second, Lives::Until)
        .await
        .expect("register a second Until child under the same turn");
    register_child(&registry, &originator, &abandon_only, Lives::Detached)
        .await
        .expect("register the third turn's Detached child");

    // A Until child under a *process* scope is the same shape of row on every
    // column but `lifetime_scope_kind`. Recovery re-derives turn rows only: a
    // process scope's row rides the terminal write that ends it, so reporting
    // one here would hand the sweep a candidate it must never write.
    let process_parent = registry
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            ProcessProvenance::session(originator.clone()),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register the process parent");
    register_child(
        &registry,
        &originator,
        &lash_core::ScopeId::process(process_parent.id.clone()),
        Lives::Until,
    )
    .await
    .expect("register a Until child under a live process scope");

    assert_eq!(
        registry
            .list_unrecorded_opener_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents"),
        vec![first.clone(), second.clone()],
        "each turn owing a row is reported once, in scope-id order: not the \
         turn whose only child abandons, and not a process scope"
    );

    assert_eq!(
        registry
            .list_unrecorded_opener_parents(None, std::num::NonZeroUsize::MIN)
            .await
            .expect("page unrecorded turn parents under a bound"),
        vec![first.clone()],
        "the page is bounded by the limit"
    );
    let first_id = first.storage_id();
    assert_eq!(
        registry
            .list_unrecorded_opener_parents(Some(first_id.as_str()), PAGE)
            .await
            .expect("resume the candidate page after the first scope"),
        vec![second.clone()],
        "the candidate page resumes strictly after the cursor, so a scope that \
         stays unresolvable cannot block every later turn forever"
    );

    registry
        .record_parent_end(&first)
        .await
        .expect("recovery writes the row the crash skipped");
    assert_eq!(
        registry
            .list_unrecorded_opener_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents after the row is written"),
        vec![second.clone()],
        "a scope stops being a candidate the moment its row exists"
    );

    for child in [&second_child, &first_child] {
        registry
            .request_process_cancel(
                &child.id.clone(),
                crate::CancelOrigin::TurnStopped,
                "conformance".to_string(),
                None,
            )
            .await
            .expect("request a cancel on a pending-cancel child");
    }
    assert_eq!(
        registry
            .list_unrecorded_opener_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents after one child is cancelled"),
        vec![second.clone()],
        "one settled child does not settle the scope while a sibling still owes a cancel"
    );
    registry
        .request_process_cancel(
            &sibling.id,
            crate::CancelOrigin::TurnStopped,
            "conformance".to_string(),
            None,
        )
        .await
        .expect("request a cancel on the remaining sibling");
    assert!(
        registry
            .list_unrecorded_opener_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents once every child is settled")
            .is_empty(),
        "a scope with no child left owing a cancel needs no row and is not reported"
    );
}
