use lash_core_execution::{RuntimeStore, SessionCatalogStore};
use lash_sqlite_store::SqliteDatabase;
use std::sync::Arc;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;
#[path = "../../../lash-core/tests/support/queued_admission_atomicity.rs"]
mod law;

#[tokio::test]
async fn sqlite_a_partial_admission_rolls_back_through_both_entry_points() {
    for entry in law::ENTRIES {
        let backend = TestBackend::open(SUBSTRATE).await;
        let store = backend.store().await;
        store
            .admit_session(&lash_core_execution::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: "root".into(),
                relation: lash_core_execution::SessionRelation::Root,
                config: lash_core_execution::SessionPolicy::new(
                    lash_core_execution::TurnBudget::Unbounded,
                    lash_core_execution::MaxToolCalls::new(1024),
                )
                .into(),
                head: lash_core_execution::SessionCreationHead::Config,
            })
            .await
            .unwrap();
        let case = law::prepare(store as Arc<dyn RuntimeStore>, entry).await;
        let conn = backend.raw(SqliteDatabase::DurableCore);
        let second = case.ids[1].replace('\'', "''");
        conn.execute_batch(&format!("CREATE TRIGGER lose_second_bind BEFORE UPDATE OF admitted_run ON queued_work_batches WHEN OLD.batch_id = '{second}' BEGIN SELECT RAISE(IGNORE); END;")).unwrap();
        assert!(
            case.admit().await.is_err(),
            "{entry:?}: a partial admission is refused"
        );
        let bound: i64 = conn
            .query_row(
                "SELECT count(*) FROM queued_work_batches WHERE admitted_run IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound, 0, "{entry:?}: the first row's bind must roll back");
        let bindings: i64 = conn
            .query_row(
                "SELECT count(*) FROM turn_cancellation_bindings",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            bindings, 0,
            "{entry:?}: a refused admission must roll back its cancellation authority"
        );
        conn.execute_batch("DROP TRIGGER lose_second_bind").unwrap();
        assert_eq!(
            case.admit().await.unwrap().len(),
            2,
            "{entry:?}: both rows remain admissible"
        );
    }
}

#[tokio::test]
async fn sqlite_an_admission_holds_its_rows_across_a_displaced_fence() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    store
        .admit_session(&lash_core_execution::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: "root".into(),
            relation: lash_core_execution::SessionRelation::Root,
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
                lash_core_execution::MaxToolCalls::new(1024),
            )
            .into(),
            head: lash_core_execution::SessionCreationHead::Config,
        })
        .await
        .unwrap();
    law::an_admission_holds_its_rows_across_a_displaced_fence(
        store as Arc<dyn RuntimeStore>,
        "sqlite",
    )
    .await;
}

/// A final write cannot discard the cancellation authority admission retained.
#[tokio::test]
async fn a_final_commit_cannot_ignore_the_admitted_cancel_intent() {
    use lash_core_execution::testing::store_fixtures::seal_shift_fence_for_test;
    let backend = TestBackend::open(SUBSTRATE).await;
    let store: Arc<dyn RuntimeStore> = backend.store().await;
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&"root".into()),
        )
        .await
        .expect("admit session");
    let case = law::prepare(Arc::clone(&store), law::Entry::Run).await;
    case.admit().await.expect("admit queued run");
    let run = lash_core_execution::TurnId::from("atomicity-run");
    let fence = seal_shift_fence_for_test(&store, &"root".into(), "recorded-cancel-owner").await;
    let mut state =
        lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ));
    state.session_id = "root".into();
    let request = lash_core_execution::facade_support::TurnCancelRequest::new(
        lash_core_execution::facade_support::TurnAddress::new("root", &run),
        "accepted-before-admission",
        None,
    );
    let snapshot = lash_core_execution::TurnCancelIntentSnapshot::Present {
        request,
        revision: 1,
    };
    let connection = backend.raw(SqliteDatabase::DurableCore);
    let encoded: String = connection
        .query_row(
            "SELECT admission_json FROM session_runs WHERE session_id = 'root' AND run = ?1",
            [run.as_str()],
            |row| row.get(0),
        )
        .expect("recorded admission");
    let mut admission: serde_json::Value = serde_json::from_str(&encoded).expect("admission");
    admission["cancel_intent"] = serde_json::to_value(&snapshot).expect("intent");
    connection
        .execute(
            "UPDATE session_runs SET admission_json = ?2 WHERE session_id = 'root' AND run = ?1",
            rusqlite::params![run.as_str(), admission.to_string()],
        )
        .expect("retain the admission snapshot seam");
    let checkpoint_operation =
        lash_core_execution::OperationId::turn("root", &run, "checkpoint:after-work");
    let (mut checkpoint, _) = lash_core_execution::RuntimeCommit::persisted_state_with_operation(
        &mut state,
        checkpoint_operation,
    )
    .expect("assemble intermediate checkpoint");
    checkpoint.shift_fence = Some(Box::new(fence.clone()));
    checkpoint.park_run = Some(run.clone());
    let receipt = store
        .commit_runtime_state(checkpoint)
        .await
        .expect("an intermediate checkpoint does not consume admission cancellation");
    state.head_revision = receipt.head_revision;
    let operation = lash_core_execution::OperationId::turn("root", &run, "final");
    let (mut commit, _) =
        lash_core_execution::RuntimeCommit::persisted_state_with_operation(&mut state, operation)
            .expect("assemble final write");
    commit.shift_fence = Some(Box::new(fence));
    commit.park_run = Some(run);
    assert!(
        matches!(
            store.commit_runtime_state(commit).await,
            Err(lash_core_execution::StoreError::TurnCancelIntentChanged { .. })
        ),
        "a final commit without cancellation closure must refuse an admitted request"
    );
}
