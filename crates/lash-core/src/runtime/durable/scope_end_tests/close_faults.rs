//! A session close's store-facing steps fail typed and resume (FIG-5307):
//! the law the facade's session-delete failure laws owed once a deletion
//! became the session actor's own close (L6b, FIG-5176).

use super::*;
use crate::store::{MaintenanceFailure, MaintenanceStop, SessionBlobReclaimReport};
use lash_core_execution::testing::ProcessRegistryFaults;
use lash_sansio::sync::MutexExt as _;

/// The fault decorators one law arms.
#[derive(Default)]
struct Faults {
    storage: Mutex<Option<Arc<crate::testing::runtime_helpers::RecordingDeploymentStore>>>,
    registry: Mutex<Option<Arc<ProcessRegistryFaults>>>,
}

/// The injected refusal of the process-state step.
fn injected(step: SessionCloseStep) -> crate::PluginError {
    crate::PluginError::Registration(format!("injected {step:?} failure"))
}

/// The partial report the injected storage failure witnessed.
fn partial() -> SessionBlobReclaimReport {
    SessionBlobReclaimReport {
        enumerated_blob_count: 4,
        retained_blob_count: 1,
        deleted_blob_count: 2,
    }
}

/// A close step that calls out of the durable store and meets a failure
/// answers that step's typed error with its source (and, for the storage
/// delete, the partial report the store witnessed); the steps before it
/// stand, nothing after it runs, and the next claim resumes at that step
/// and closes the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_close_step_answers_its_typed_cause_and_the_next_claim_resumes_there() {
    for failing in [SessionCloseStep::Artifacts, SessionCloseStep::Tombstone] {
        let faults = Arc::new(Faults::default());
        let world = {
            let faults = Arc::clone(&faults);
            World::layered(
                "faulted-close",
                DurableSettings::default(),
                Vec::new(),
                move |stores| {
                    let (storage, registry) = (Arc::clone(&faults), faults);
                    stores
                        .map_session_store_factory(move |inner| {
                            let recording = Arc::new(
                                crate::testing::runtime_helpers::RecordingDeploymentStore::over(
                                    inner,
                                ),
                            );
                            *storage.storage.lock_recover() = Some(Arc::clone(&recording));
                            recording
                        })
                        .map_process_registry(move |inner| {
                            let faulted = Arc::new(ProcessRegistryFaults::new(inner));
                            *registry.registry.lock_recover() = Some(Arc::clone(&faulted));
                            faulted
                        })
                },
            )
            .await
        };
        world
            .backend
            .session_store_factory()
            .admit_session(
                &lash_core_store::testing::store_fixtures::root_session_request(&world.session),
            )
            .await
            .expect("materialize the session");
        let cx = world.claim().await;
        request_session_close(&world.backend, &world.session)
            .await
            .expect("request the close");
        let mut tx = cx.begin().await.expect("begin");
        begin_session_close(&mut tx, &world.session);
        tx.ack_seen();
        cx.commit(tx, CommitLabel::SESSION_CLOSE_BEGIN)
            .await
            .expect("drain the close request");

        match failing {
            SessionCloseStep::Artifacts => {
                faults
                    .storage
                    .lock_recover()
                    .clone()
                    .expect("storage fault")
                    .fail_next_delete(MaintenanceFailure::failed(
                        crate::StoreError::Backend("injected storage failure".to_owned()),
                        partial(),
                    ));
            }
            _ => faults
                .registry
                .lock_recover()
                .clone()
                .expect("registry fault")
                .fail_next_session_delete(injected(failing)),
        }

        let error = run_session_close(&cx, &world.session, None)
            .await
            .expect_err("the faulted step fails the close's pass");
        match (failing, &error) {
            (
                SessionCloseStep::Tombstone,
                super::super::session_close::SessionCloseError::Process(source),
            ) => {
                assert_eq!(
                    format!("{source:?}"),
                    format!("{:?}", injected(failing)),
                    "{failing:?}: the step keeps its source"
                );
            }
            (
                SessionCloseStep::Artifacts,
                super::super::session_close::SessionCloseError::Storage(failure),
            ) => {
                assert!(
                    matches!(
                        &failure.stop,
                        MaintenanceStop::Failed(crate::StoreError::Backend(message))
                            if message == "injected storage failure"
                    ),
                    "the typed maintenance stop is kept: {failure:?}"
                );
                assert_eq!(failure.partial, partial(), "the partial report is kept");
            }
            other => panic!("{failing:?}: the close answered another step's error: {other:?}"),
        }
        let row = world
            .backend
            .durable()
            .session_close(&world.session)
            .await
            .expect("read the close")
            .expect("the close row");
        assert_eq!(
            row.next(),
            Some(failing),
            "{failing:?}: the steps before the failed one stand, and it is next"
        );
        if failing != SessionCloseStep::Tombstone {
            assert!(
                !matches!(
                    world
                        .backend
                        .session_store_factory()
                        .lookup_session(&world.session)
                        .await
                        .expect("look the session up"),
                    SessionLookup::Deleted
                ),
                "{failing:?}: nothing after the failed step ran"
            );
        }

        let cx = world.claim().await;
        assert_eq!(
            run_session_close(&cx, &world.session, None)
                .await
                .expect("the next claim resumes the close"),
            Some(SessionCloseExit::Closed),
            "{failing:?}"
        );
        assert!(
            world
                .backend
                .durable()
                .session_close(&world.session)
                .await
                .expect("read the close")
                .expect("the tombstone")
                .is_tombstone(),
            "{failing:?}: the resumed close tombstoned the session"
        );
    }
}
