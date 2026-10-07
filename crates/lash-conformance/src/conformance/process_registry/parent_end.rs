//! The parent-end ledger row a terminal parent writes: the late-start fence
//! of its scope, and what retention may do to it and to the process row it
//! was written for.

use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn terminal_completion_atomically_retains_parent_end_plan(
    registry: Arc<dyn ProcessRegistry>,
) {
    let originator = SessionScope::new("parent-end-retention-session");
    let parent = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::session(originator.clone()),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register parent-end-plan process");
    let process_id = parent.id.clone();
    let parent_scope = lash_core::ScopeId::process(parent.id.clone());
    let child = crate::started_until_starter(
        lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::session(originator.clone()),
            lash_core::Lifetime::Detached,
        ),
        parent_scope.clone(),
    );
    // A live child keeps the ended scope's row through the parent's prune.
    let _live_child = registry
        .register_process(child)
        .await
        .expect("register cancel child under the live parent");
    let completion = registry
        .complete_process(
            &process_id,
            settled_success(serde_json::json!({"parent": "done"})),
            crate::ProcessCompletionAuthority::workflow_key(&process_id),
        )
        .await
        .expect("terminal write and ledger row commit atomically");
    assert!(matches!(
        completion,
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    let pending = registry
        .get_parent_end_plan(&parent_scope)
        .await
        .expect("read the parent-end ledger row")
        .expect("the terminal append writes one ledger row for the ended scope");
    assert_eq!(
        pending.parent,
        parent_scope.clone(),
        "the row is keyed by the scope it ends"
    );
    let pending_prune = registry
        .prune_terminal_processes(
            u64::MAX,
            Some(ProcessListFilter {
                status: ProcessStatusFilter::Any,
                originator: Some(ProcessOriginatorFilter::session(
                    originator.session_id.clone(),
                )),
                ..ProcessListFilter::default()
            }),
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the terminal parent while its end plan is pending");
    assert_eq!(
        pending_prune.pruned_processes, 1,
        "the ledger is keyed by scope, so retention never has to hold the parent row back"
    );
    let parent_after_prune = match registry.get_process(&process_id).await {
        Ok(record) => record.is_some(),
        // A tier that tombstones pruned rows answers the read with a refusal
        // rather than an absence; both mean the parent row is gone.
        Err(crate::PluginError::ProcessNoLongerRetained { .. }) => false,
        Err(error) => panic!("read parent after retention prune: {error:?}"),
    };
    assert!(!parent_after_prune, "the pruned parent row is gone");
    assert!(
        registry
            .get_parent_end_plan(&parent_scope)
            .await
            .expect("read parent-end ledger row after retention prune")
            .is_some(),
        "the ledger row outlives the process row it was written for"
    );
    // A `Cancel` child registering after the ledger row exists is fenced.
    let late = crate::started_until_starter(
        lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::session(originator.clone()),
            lash_core::Lifetime::Detached,
        ),
        parent_scope.clone(),
    );
    assert!(
        matches!(
            registry.register_process(late).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a Cancel child that registers after the ledger row is refused"
    );
}

/// Retention reclaims a ledger row once no live child names its scope.
///
/// The row deliberately outlives the scope it records — it is what refuses a
/// late `Cancel` child. Retention is what bounds it: past the same cutoff
/// process rows are pruned under, an ended scope with no live child can no
/// longer parent anything lash will act on. Without this every ended scope
/// would leave one row behind forever.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn parent_end_plans_are_reclaimed_by_retention(
    registry: Arc<dyn ProcessRegistry>,
) {
    let originator = SessionScope::new("parent-end-reclaim-session");
    let parent = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::session(originator.clone()),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register parent-end-reclaim process");
    let parent_scope = lash_core::ScopeId::process(parent.id.clone());
    let child = registry
        .register_process(crate::started_until_starter(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                ProcessProvenance::session(originator.clone()),
                lash_core::Lifetime::Detached,
            ),
            parent_scope.clone(),
        ))
        .await
        .expect("register cancel child under the live parent");

    complete_process(&registry, &parent.id).await;

    let filter = ProcessListFilter {
        status: ProcessStatusFilter::Any,
        originator: Some(ProcessOriginatorFilter::session(
            originator.session_id.clone(),
        )),
        ..ProcessListFilter::default()
    };
    registry
        .prune_terminal_processes(
            u64::MAX,
            Some(filter.clone()),
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune while the child is still live");
    assert!(
        registry
            .get_parent_end_plan(&parent_scope)
            .await
            .expect("read the ledger row while a live child names the scope")
            .is_some(),
        "a row is retained while a live child still names its scope"
    );

    complete_process(&registry, &child.id).await;
    registry
        .prune_terminal_processes(
            u64::MAX,
            Some(filter),
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune once no live child names the scope");
    assert_eq!(
        registry
            .get_parent_end_plan(&parent_scope)
            .await
            .expect("read the ledger row after retention"),
        None,
        "retention reclaims a row once no live child names its scope"
    );
}

/// Execute one held process to a terminal outcome.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn complete_process(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) {
    registry
        .complete_process(
            process_id,
            settled_success(serde_json::json!({"done": true})),
            crate::ProcessCompletionAuthority::workflow_key(process_id),
        )
        .await
        .expect("complete process");
}

/// A session's `Session` scope closes only through its close row (FIG-3607
/// R10, ADR 0108 §5). Deleting the session's process state writes none: the
/// session's close is the one owner of that row. Once the row is there, a
/// start that names the closed session — as its lifetime or its starter — is
/// refused (R11).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_session_scope_closes_only_through_its_close_row(
    registry: Arc<dyn ProcessRegistry>,
) {
    let session = SessionId::from("session-close-session");
    let originator = SessionScope::new(session.clone());
    let turn = lash_core::ScopeId::turn(session.clone(), crate::TurnId::from("session-close-turn"));
    let session_scope = lash_core::ScopeId::session(session.clone());
    let registration = || {
        lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::session(originator.clone()),
            lash_core::Lifetime::Detached,
        )
    };
    let until_session = registry
        .register_process(crate::started_until(
            registration(),
            turn.clone(),
            session_scope.clone(),
        ))
        .await
        .expect("register a child living until the session");
    let detached = registry
        .register_process(crate::started_detached(registration(), turn.clone()))
        .await
        .expect("register a detached child the session's turn started");
    assert!(
        registry
            .get_parent_end_plan(&session_scope)
            .await
            .expect("read the live session's close row")
            .is_none(),
        "a live session has no close row"
    );

    registry
        .delete_session_process_state(&session)
        .await
        .expect("delete the session's process state");
    assert!(
        registry
            .get_parent_end_plan(&session_scope)
            .await
            .expect("read the session's close row after its process state went")
            .is_none(),
        "deleting a session's process state writes no close row: its close intent owns it"
    );
    let until_unclosed = registry
        .register_process(crate::started_until(
            registration(),
            turn.clone(),
            session_scope.clone(),
        ))
        .await
        .expect("a session that is not closed still admits a start living until it");

    registry
        .record_parent_end(&session_scope)
        .await
        .expect("close the session's scope");
    let closed = registry
        .get_parent_end_plan(&session_scope)
        .await
        .expect("read the session's close row")
        .expect("the scope close writes the session scope's close row");
    assert_eq!(closed.parent, session_scope);
    for survivor in [&until_session, &until_unclosed, &detached] {
        assert!(
            registry
                .get_process(&survivor.id)
                .await
                .expect("read a child of the session")
                .is_some(),
            "the close row deletes no process"
        );
    }
    assert!(
        matches!(
            registry
                .register_process(crate::started_until(
                    registration(),
                    turn.clone(),
                    session_scope.clone(),
                ))
                .await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a start living until the closed session is refused"
    );
    let session_started = {
        let mut registration = registration();
        registration.ancestry = crate::Ancestry::from_scopes([session_scope.clone()]);
        registration
    };
    assert!(
        matches!(
            registry.register_process(session_started).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a start whose starter is the closed session is refused"
    );
}
