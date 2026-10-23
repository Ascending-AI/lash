//! A failed process-state cleanup resumes at the retained Tombstone step.

use super::*;
use lash_core_execution::testing::ProcessRegistryFaults;
use lash_protocol_types::sync::MutexExt as _;

/// CLOSE-PROCESS-RETRY: the process-registry step retains its typed failure;
/// the next claim resumes there after transactional Artifacts deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_process_state_delete_resumes_at_tombstone() {
    let failing = SessionCloseStep::Tombstone;
    let faults = Arc::new(Mutex::new(None));
    let world = {
        let faults = Arc::clone(&faults);
        World::layered(
            "faulted-close",
            DurableSettings {
                max_ingress_bytes: Some(const { std::num::NonZeroU64::new(64 * 1024).unwrap() }),
                owner_retention: Some(lash_durable::OwnerRetentionSettings {
                    idle: std::time::Duration::from_secs(60),
                    max_resident_owners: const { std::num::NonZeroUsize::new(256).unwrap() },
                }),
                ..DurableSettings::default()
            },
            Vec::new(),
            move |stores| {
                stores.map_process_registry(move |inner| {
                    let faulted = Arc::new(ProcessRegistryFaults::new(inner));
                    *faults.lock_recover() = Some(Arc::clone(&faulted));
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

    let injected = lash_execution_contracts::plugin_error::PluginError::Registration(
        "injected process-state deletion failure".into(),
    );
    faults
        .lock_recover()
        .clone()
        .expect("registry fault")
        .fail_next_session_delete(injected.clone());
    let error = run_session_close(&cx, &world.session, None)
        .await
        .expect_err("the process-state delete fails");
    match error {
        super::super::session_close::SessionCloseError::Process(source) => {
            assert_eq!(format!("{source:?}"), format!("{injected:?}"));
        }
        other => panic!("unexpected close failure: {other:?}"),
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
