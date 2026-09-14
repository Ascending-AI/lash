//! The parent-end ledger row a terminal parent writes, and what retention may
//! do to the process row it was written for.

use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn terminal_completion_atomically_retains_parent_end_plan(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = ProcessId::from("process-parent-end-plan");
    let originator = SessionScope::new("parent-end-retention-session");
    let parent = registry
        .register_process(ProcessRegistration::new(
            process_id.clone(),
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
        .expect("register parent-end-plan process");
    let parent_scope = lash_core::ParentScope::Process {
        process_id: process_id.clone(),
        incarnation: parent.incarnation,
    };
    let child = ProcessRegistration::new(
        ProcessId::from("process-parent-end-child"),
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::session(originator.clone()),
        lash_core::ProcessLifecyclePolicy::new(
            parent_scope.clone(),
            lash_core::OnParentEnd::Cancel,
        ),
    );
    let child = registry
        .register_process(child)
        .await
        .expect("register cancel child under the live parent");
    let lease = registry
        .claim_process_lease(
            &process_id,
            &crate::LeaseOwnerIdentity::opaque("parent-end-owner", "parent-end-owner:i"),
            60_000,
        )
        .await
        .expect("claim parent-end-plan process")
        .acquired()
        .expect("parent-end-plan lease acquired");
    let completion = registry
        .complete_process_with_lease(
            &lease,
            settled_success(serde_json::json!({"parent": "done"})),
        )
        .await
        .expect("terminal write and ledger row commit atomically");
    assert!(matches!(
        completion,
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    let pending = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect("list pending parent-end ledger rows");
    assert_eq!(
        pending
            .iter()
            .map(|plan| plan.parent.clone())
            .collect::<Vec<_>>(),
        vec![parent_scope.clone()],
        "the terminal append writes exactly one ledger row for the ended scope"
    );
    assert!(
        pending[0].settled_at_ms.is_none(),
        "a freshly written ledger row is unsettled"
    );

    // The sweep's children query is index-served and returns exactly the
    // Cancel children that still owe a cancel.
    assert_eq!(
        registry
            .list_parent_end_children(&parent_scope, None, std::num::NonZeroUsize::MIN)
            .await
            .expect("page parent-end children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![child.id.clone()]
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
    assert_eq!(
        registry
            .list_parent_end_children(&parent_scope, None, std::num::NonZeroUsize::MIN)
            .await
            .expect("page parent-end children after the parent row is pruned")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![child.id.clone()],
        "the sweep reads children by their own parent scope, not through the parent row"
    );

    // A `Cancel` child registering after the ledger row exists is fenced.
    let late = ProcessRegistration::new(
        ProcessId::from("process-parent-end-late-child"),
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::session(originator.clone()),
        lash_core::ProcessLifecyclePolicy::new(
            parent_scope.clone(),
            lash_core::OnParentEnd::Cancel,
        ),
    );
    assert!(
        matches!(
            registry.register_process(late).await,
            Err(crate::PluginError::ParentEnded { .. })
        ),
        "a Cancel child that registers after the ledger row is refused"
    );

    registry
        .request_process_cancel(
            &crate::ProcessRef::from_record(&child),
            crate::CancelOrigin::ParentEnded,
            "conformance".to_string(),
            None,
        )
        .await
        .expect("request the child cancel the sweep would request");
    assert!(
        registry
            .list_parent_end_children(&parent_scope, None, std::num::NonZeroUsize::MIN)
            .await
            .expect("page parent-end children after the cancel request")
            .is_empty(),
        "a child already carrying a cancel request is settled by definition"
    );

    registry
        .settle_parent_end_plan(&parent_scope)
        .await
        .expect("settle parent-end ledger row");
    registry
        .settle_parent_end_plan(&parent_scope)
        .await
        .expect("settling a parent-end ledger row is idempotent");
    assert!(
        registry
            .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
            .await
            .expect("parent-end ledger row cleared")
            .is_empty()
    );
    assert!(
        registry
            .get_parent_end_plan(&parent_scope)
            .await
            .expect("read settled ledger row")
            .expect("a settled row is retained, not deleted")
            .settled_at_ms
            .is_some(),
        "settlement stamps the row rather than deleting the fence"
    );
    let settled_prune = registry
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
        .expect("prune again after the end plan settles");
    assert_eq!(
        settled_prune.pruned_processes, 0,
        "the parent was already reclaimed; settlement adds no new prune-eligible row"
    );
}

/// Retention reclaims a settled ledger row once no live child names its scope.
///
/// The row deliberately outlives the scope it records — it is what refuses a
/// late `Cancel` child — so nothing in the sweep may delete it. Retention is
/// what bounds it: past the same cutoff process rows are pruned under, a
/// settled scope with no live child can no longer parent anything lash will
/// act on. Without this every committed turn would leave one row behind
/// forever.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn settled_parent_end_plans_are_reclaimed_by_retention(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = ProcessId::from("process-parent-end-reclaim");
    let originator = SessionScope::new("parent-end-reclaim-session");
    let parent = registry
        .register_process(ProcessRegistration::new(
            process_id.clone(),
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
        .expect("register parent-end-reclaim process");
    let parent_scope = lash_core::ParentScope::Process {
        process_id: process_id.clone(),
        incarnation: parent.incarnation,
    };
    let child_id = ProcessId::from("process-parent-end-reclaim-child");
    let child = registry
        .register_process(ProcessRegistration::new(
            child_id.clone(),
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::Rerunnable,
            ProcessProvenance::session(originator.clone()),
            lash_core::ProcessLifecyclePolicy::new(
                parent_scope.clone(),
                lash_core::OnParentEnd::Cancel,
            ),
        ))
        .await
        .expect("register cancel child under the live parent");

    complete_process(&registry, &process_id, "parent-end-reclaim-parent").await;
    registry
        .settle_parent_end_plan(&parent_scope)
        .await
        .expect("settle the parent-end ledger row");

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
        "a settled row is retained while a live child still names its scope"
    );

    complete_process(&registry, &child.id, "parent-end-reclaim-child").await;
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
        "retention reclaims a settled row once no live child names its scope"
    );
}

/// Drive one registered process to a terminal outcome under its own lease.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn complete_process(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    owner: &str,
) {
    let lease = registry
        .claim_process_lease(
            process_id,
            &crate::LeaseOwnerIdentity::opaque(owner, format!("{owner}:i")),
            60_000,
        )
        .await
        .expect("claim lease")
        .acquired()
        .expect("lease acquired");
    registry
        .complete_process_with_lease(&lease, settled_success(serde_json::json!({"done": true})))
        .await
        .expect("complete process");
}
