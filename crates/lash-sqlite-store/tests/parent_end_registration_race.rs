//! SQLite proof that an `Until` child registering while its parent scope ends
//! is either refused or swept, never left live under an ended scope.
//!
//! SQLite serializes registration and the ledger write through one write
//! flow, so this cannot interleave the way the PostgreSQL race does — but the
//! same invariant must hold on this backend, and this test is what keeps the
//! scope-keyed ledger honest against a regression that reads "no row" outside
//! the serialized write path (for example through a stale read of the index
//! projection rather than the fence the write flow holds).

use std::sync::Arc;

use lash_core_execution::ProcessRegistry;
use lash_sqlite_store::SqliteProcessRegistry;

/// One race per scope, enough of them that the ordering is exercised rather
/// than hoped for.
const SCOPES: usize = 24;

fn session_name(index: usize) -> String {
    format!("sqlite-parent-end-race-session-{index:02}")
}

fn turn_scope(index: usize) -> lash_core_execution::ScopeId {
    lash_core_execution::ScopeId::turn(
        lash_sansio::SessionId::from(session_name(index)),
        lash_core_execution::TurnId::from(format!("sqlite-parent-end-race-turn-{index:02}")),
    )
}

/// A child one turn scope started, living `Until` it. The child's originator
/// session is the turn's own, so both come from `index`.
fn cancel_child(
    index: usize,
    parent: lash_core_execution::ScopeId,
) -> lash_core_execution::ProcessRegistration {
    let mut registration = lash_core_execution::ProcessRegistration::new(
        lash_core_execution::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core_execution::RecoveryContract::Rerunnable,
        lash_core_execution::ProcessProvenance::session(lash_core_execution::SessionScope::new(
            session_name(index),
        )),
        lash_core_execution::Lifetime::Detached,
    );
    // Started by the turn and living until it.
    registration.ancestry = lash_core_execution::Ancestry::from_scopes([parent.clone()]);
    registration.lifetime = lash_core_execution::LifetimeDecision::Until {
        scope: parent,
        grant: lash_core_execution::ScopeGrant::Ancestor,
    };
    registration
}

/// The worker's settle, written out here so the race runs against the same
/// registry calls the sweep makes.
async fn settle(registry: &Arc<dyn ProcessRegistry>, parent: &lash_core_execution::ScopeId) {
    let page = std::num::NonZeroUsize::new(64).expect("page bound");
    let mut after: Option<lash_sansio::ProcessId> = None;
    loop {
        let children = registry
            .list_parent_end_children(parent, after.as_ref(), page)
            .await
            .expect("page parent-end children");
        let Some(last) = children.last() else { break };
        after = Some(last.id.clone());
        for child in &children {
            registry
                .request_process_cancel(
                    &child.id,
                    lash_core_execution::CancelOrigin::ParentEnded,
                    "sqlite-parent-end-race".to_string(),
                    None,
                )
                .await
                .expect("request the cancel the sweep requests");
        }
    }
    registry
        .settle_parent_end_plan(parent)
        .await
        .expect("settle the ledger row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_registering_as_its_parent_scope_ends_is_refused_or_swept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_path = dir.path().join("processes.db");
    let sessions = dir.path().join("sessions");
    let registry = Arc::new(
        SqliteProcessRegistry::open(&process_path, &sessions)
            .await
            .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;
    let barrier = Arc::new(tokio::sync::Barrier::new(SCOPES * 2));

    let mut races = Vec::new();
    for index in 0..SCOPES {
        let parent = turn_scope(index);
        let registering = {
            let registry = Arc::clone(&registry);
            let parent = parent.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                registry.register_process(cancel_child(index, parent)).await
            })
        };
        let ending = {
            let registry = Arc::clone(&registry);
            let parent = parent.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                registry
                    .record_parent_end(&parent)
                    .await
                    .expect("end the parent scope");
                settle(&registry, &parent).await;
            })
        };
        races.push((index, parent, registering, ending));
    }

    for (index, parent, registering, ending) in races {
        let registration = registering.await.expect("registration task");
        ending.await.expect("parent-end task");
        match registration {
            Err(error) => assert!(
                matches!(error, lash_core_execution::PluginError::ParentEnded { .. }),
                "a child racing its parent's end is refused with ParentEnded, not {error:?}"
            ),
            Ok(record) => {
                let observed = registry
                    .get_process(&record.id)
                    .await
                    .expect("read the committed child")
                    .expect("the committed child exists");
                assert!(
                    observed.cancel_request.is_some(),
                    "child {index} committed before the ledger row, so the sweep must have \
                     cancelled it; a live Until child of an ended scope is never revisited"
                );
                assert_eq!(
                    observed.cancel_request.map(|request| request.origin),
                    Some(lash_core_execution::CancelOrigin::ParentEnded)
                );
            }
        }
        assert!(
            registry
                .get_parent_end_plan(&parent)
                .await
                .expect("read the settled ledger row")
                .expect("the ledger row exists")
                .settled_at_ms
                .is_some(),
            "scope {index} settled"
        );
    }
}
