//! A SessionTurn process cannot replay away a superseded child fence
//! (FIG-4524, ADR 0105 §2 and §9, ADR 0110 §2).

use super::process_root_recovery::{Harness, HeldModelCall, Storage, core, start_child};
use super::*;

async fn superseded_child_root_law(storage: Storage) {
    let (harness, _stores) = Harness::new(storage, false).await;
    let Harness::Double(double) = &harness else {
        unreachable!("the law runs on the server double");
    };
    let call = Arc::new(HeldModelCall::new());
    let core = core(&harness, Arc::clone(&call));
    let process_id = start_child(&harness, &core, None, "superseded-child-root").await;
    tokio::time::timeout(BOUND, call.started.notified())
        .await
        .expect("the child's admitted root reaches its model call");
    let session_id = lash_core::facade_support::process_child_session_id(&process_id);
    let sessions = harness.backend().session_store_factory();
    let epoch = sessions
        .drive_epoch(&session_id)
        .await
        .expect("read the epoch");
    assert_eq!(epoch.epoch, 1, "the process sealed its child root once");
    let successor = sessions
        .seal_drive_epoch(
            &session_id,
            &lash_core::store::AdmissionId::new("superseding-child-admission"),
            epoch.epoch,
            &lash_core::store::RootStartNonce::new("superseding-child-execution"),
            None,
        )
        .await
        .expect("the successor seals over the child's fence");
    assert!(
        matches!(successor, lash_core::store::DriveEpochSeal::Sealed(ref fence) if fence.epoch() == 2),
        "{successor:?}"
    );
    call.release.add_permits(1);

    // Fail as soon as the host requests a retry, rather than waiting for
    // the engine to exhaust its attempt budget on the permanent refusal.
    let target = format!("/{process_id}/run");
    let invocation = tokio::time::timeout(BOUND, async {
        loop {
            if let Some(invocation) = double
                .server()
                .invocations()
                .into_iter()
                .find(|invocation| invocation.target.ends_with(&target))
            {
                assert!(
                    invocation.last_failure.is_none(),
                    "a superseded child ends its process without retry: {invocation:?}"
                );
                if invocation.status == "completed" {
                    break invocation;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the superseded child ends its process step");
    assert_eq!(invocation.attempts, 1, "{invocation:?}");
    assert_eq!(invocation.retry_count, 0, "{invocation:?}");
    let output = core
        .processes()
        .await_output(&process_id)
        .await
        .expect("read the retained process terminal");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles its superseded child refusal: {output:?}");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("the superseded process fails: {output:?}");
    };
    assert_eq!(
        failure.code,
        lash_core::RuntimeErrorCode::StoreCommitSuperseded.as_str()
    );
    assert_eq!(failure.source, lash_core::ToolFailureSource::Runtime);
    assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    assert!(
        failure.message.starts_with("drive fence epoch 1"),
        "{failure:?}"
    );
    assert_eq!(
        sessions
            .root_terminal(&session_id, &lash_core::TurnId::from(process_id.as_str()))
            .await
            .expect("read the child's root terminal"),
        None,
        "the stale child writes no root terminal under its superseded fence"
    );
    assert_eq!(sessions.drive_epoch(&session_id).await.unwrap().epoch, 2);
    drop(core);
    harness.finish().await;
}

macro_rules! laws {
    ($module:ident, $storage:expr $(, $service:literal)?) => {
        mod $module {
            use super::*;
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_superseded_child_root_ends_its_session_turn_process_without_retry() {
                superseded_child_root_law($storage).await;
            }
        }
    };
}

laws!(sqlite_memory, Storage::Memory);
laws!(sqlite_file, Storage::File);
laws!(
    postgres,
    Storage::Postgres,
    "requires PostgreSQL; run through the pg16 service gate"
);
