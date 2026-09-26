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
        RecoveryContract::Rerunnable,
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
    assert_eq!(
        registry
            .list_pending_parent_end_plans(PAGE)
            .await
            .expect("page pending ledger rows")
            .into_iter()
            .map(|plan| plan.parent)
            .collect::<Vec<_>>(),
        vec![turn.clone()],
        "the repeat left one row, and the turn that never ended has none"
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
    assert!(
        registry
            .list_pending_parent_end_plans(PAGE)
            .await
            .expect("page pending ledger rows after settlement")
            .is_empty(),
        "a settled row is not pending, including after a repeated record"
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
            RecoveryContract::Rerunnable,
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
