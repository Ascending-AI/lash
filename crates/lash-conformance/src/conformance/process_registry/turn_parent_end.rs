//! The turn half of the parent-end ledger: the row a committed turn records
//! for itself, and the recovery read that re-derives it after a crash.
//!
//! The sibling laws in [`super::parent_end`] reach the ledger through a
//! *process* parent's terminal write, which is the one path that writes a row
//! as a side effect of another fact. A turn cannot end that way — a turn is
//! not a process row — so `record_parent_end` is its only writer, and
//! `list_unrecorded_turn_parents` is the only way a crash between the turn
//! commit and that write is ever noticed. Neither had a shared law: both were
//! asserted on one backend's own tests, so a tier could implement either one
//! differently, or inherit the empty-page default, and stay green.

use super::*;
use pretty_assertions::assert_eq;

const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(16).expect("page bound");

fn turn_scope(session: &SessionId, turn: &str) -> lash_core::ParentScope {
    lash_core::ParentScope::Turn {
        session_id: session.clone(),
        turn_id: crate::TurnId::from(turn),
    }
}

async fn register_child(
    registry: &Arc<dyn ProcessRegistry>,
    originator: &SessionScope,
    id: &str,
    parent: &lash_core::ParentScope,
    on_parent_end: lash_core::OnParentEnd,
) -> Result<ProcessRecord, crate::PluginError> {
    registry
        .register_process(ProcessRegistration::new(
            ProcessId::from(id),
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::Rerunnable,
            ProcessProvenance::session(originator.clone()),
            lash_core::ProcessLifecyclePolicy::new(parent.clone(), on_parent_end),
        ))
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

    for (id, parent, on_parent_end) in [
        (
            "turn-parent-end-cancel-a",
            &turn,
            lash_core::OnParentEnd::Cancel,
        ),
        (
            "turn-parent-end-cancel-b",
            &turn,
            lash_core::OnParentEnd::Cancel,
        ),
        (
            "turn-parent-end-abandon",
            &turn,
            lash_core::OnParentEnd::Abandon,
        ),
        (
            "turn-parent-end-other-turn-child",
            &other_turn,
            lash_core::OnParentEnd::Cancel,
        ),
    ] {
        register_child(&registry, &originator, id, parent, on_parent_end)
            .await
            .expect("register a child under a live turn scope");
    }

    assert!(
        registry
            .get_parent_end_plan(&turn)
            .await
            .expect("read the ledger row of a turn that has not ended")
            .is_none(),
        "a live turn owns no ledger row"
    );
    assert!(
        registry
            .record_parent_end(&lash_core::ParentScope::Host)
            .await
            .is_err(),
        "a host scope never ends, so it can never be recorded as ended"
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
        vec![
            ProcessId::from("turn-parent-end-cancel-a"),
            ProcessId::from("turn-parent-end-cancel-b"),
        ],
        "the sweep sees this turn's Cancel children only: not its Abandon child, \
         and not another turn's"
    );
    assert_eq!(
        registry
            .list_parent_end_children(
                &turn,
                Some(&ProcessId::from("turn-parent-end-cancel-a")),
                PAGE
            )
            .await
            .expect("resume the children page after the first child")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![ProcessId::from("turn-parent-end-cancel-b")],
        "the children page resumes strictly after the cursor"
    );

    assert!(
        matches!(
            register_child(
                &registry,
                &originator,
                "turn-parent-end-late-cancel",
                &turn,
                lash_core::OnParentEnd::Cancel,
            )
            .await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a Cancel child registering after the turn's row exists is refused, \
         because the sweep that would have cancelled it has already run"
    );
    register_child(
        &registry,
        &originator,
        "turn-parent-end-late-abandon",
        &turn,
        lash_core::OnParentEnd::Abandon,
    )
    .await
    .expect("an Abandon child owes the ended turn nothing and is admitted");

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

    let first_child = register_child(
        &registry,
        &originator,
        "unrecorded-turn-a-cancel",
        &first,
        lash_core::OnParentEnd::Cancel,
    )
    .await
    .expect("register the first turn's Cancel child");
    let second_child = register_child(
        &registry,
        &originator,
        "unrecorded-turn-b-cancel",
        &second,
        lash_core::OnParentEnd::Cancel,
    )
    .await
    .expect("register the second turn's Cancel child");
    register_child(
        &registry,
        &originator,
        "unrecorded-turn-b-cancel-sibling",
        &second,
        lash_core::OnParentEnd::Cancel,
    )
    .await
    .expect("register a second Cancel child under the same turn");
    register_child(
        &registry,
        &originator,
        "unrecorded-turn-c-abandon",
        &abandon_only,
        lash_core::OnParentEnd::Abandon,
    )
    .await
    .expect("register the third turn's Abandon child");

    // A Cancel child under a *process* scope is the same shape of row on every
    // column but `parent_scope_kind`. Recovery re-derives turn rows only: a
    // process scope's row rides the terminal write that ends it, so reporting
    // one here would hand the sweep a candidate it must never write.
    let process_parent = registry
        .register_process(ProcessRegistration::new(
            ProcessId::from("unrecorded-turn-process-parent"),
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::Rerunnable,
            ProcessProvenance::session(originator.clone()),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        ))
        .await
        .expect("register the process parent");
    register_child(
        &registry,
        &originator,
        "unrecorded-turn-process-child",
        &lash_core::ParentScope::Process {
            process_id: process_parent.id.clone(),
            incarnation: process_parent.incarnation,
        },
        lash_core::OnParentEnd::Cancel,
    )
    .await
    .expect("register a Cancel child under a live process scope");

    assert_eq!(
        registry
            .list_unrecorded_turn_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents"),
        vec![first.clone(), second.clone()],
        "each turn owing a row is reported once, in scope-id order: not the \
         turn whose only child abandons, and not a process scope"
    );

    assert_eq!(
        registry
            .list_unrecorded_turn_parents(None, std::num::NonZeroUsize::MIN)
            .await
            .expect("page unrecorded turn parents under a bound"),
        vec![first.clone()],
        "the page is bounded by the limit"
    );
    let first_id = first
        .storage_id()
        .expect("a turn scope has a storage identity");
    assert_eq!(
        registry
            .list_unrecorded_turn_parents(Some(first_id.as_str()), PAGE)
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
            .list_unrecorded_turn_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents after the row is written"),
        vec![second.clone()],
        "a scope stops being a candidate the moment its row exists"
    );

    for child in [&second_child, &first_child] {
        registry
            .request_process_cancel(
                &crate::ProcessRef::from_record(child),
                crate::CancelOrigin::TurnStopped,
                "conformance".to_string(),
                None,
            )
            .await
            .expect("request a cancel on a pending-cancel child");
    }
    assert_eq!(
        registry
            .list_unrecorded_turn_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents after one child is cancelled"),
        vec![second.clone()],
        "one settled child does not settle the scope while a sibling still owes a cancel"
    );
    registry
        .request_process_cancel(
            &crate::ProcessRef::from_record(
                &registry
                    .get_process(&ProcessId::from("unrecorded-turn-b-cancel-sibling"))
                    .await
                    .expect("read the remaining sibling")
                    .expect("the sibling is live"),
            ),
            crate::CancelOrigin::TurnStopped,
            "conformance".to_string(),
            None,
        )
        .await
        .expect("request a cancel on the remaining sibling");
    assert!(
        registry
            .list_unrecorded_turn_parents(None, PAGE)
            .await
            .expect("page unrecorded turn parents once every child is settled")
            .is_empty(),
        "a scope with no child left owing a cancel needs no row and is not reported"
    );
}
