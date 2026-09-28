//! An engine attempt the engine drops mid-flight leaves nothing on the
//! resident runtime for the next reader (FIG-3984).
//!
//! Restate stops polling a handler that suspends, so an attempt of a root
//! ends where it awaited. Whoever locks the resident runtime next, a host's
//! read as much as the engine's next attempt, must find it as a redrive in a
//! fresh process would: no sealed root, no journaled claims, no admitted turn
//! index, no attempt flag, and a resident session that reloads from the
//! durable session instead of serving what the dropped attempt did to it.

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use crate::runtime::tests::helpers::{TestRuntime, recording_session_store};
use crate::testing::{TestProvider, TestTurnDrive as _};

const SESSION_ID: &str = "dropped-engine-attempt";

/// An attempt dropped while its model call is in flight, then a host read of
/// the resident runtime before any new attempt: the read sees none of the
/// attempt's residue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_read_after_a_dropped_attempt_sees_no_residue() {
    let double =
        crate::testing::kernel_double(0x3984_0001, lash_restate_test::ServerConfig::default())
            .await;
    let backend = double.lash_backend();
    let store = recording_session_store(&backend, SESSION_ID).await;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let started_tx = Arc::clone(&started_tx);
            async move {
                if let Some(tx) = started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<crate::LlmResponse, _>>().await
            }
        })
        .build();
    let runtime = TestRuntime::new(&backend, provider)
        .with_session_id(SESSION_ID)
        .store(store)
        .build()
        .await;
    let session_id = runtime.state.session_id.clone();
    let resident = crate::runtime::RuntimeHandle::new(runtime);
    let handler = double
        .open_handler(crate::AdmittedScope::turn(&session_id, "dropped-attempt"))
        .await
        .expect("open the attempt's handler");

    let attempt = async {
        let writer = resident.writer();
        let mut runtime = writer.lock().await;
        runtime
            .drive_turn(
                crate::TurnInput::text("stay in flight until dropped"),
                crate::runtime::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    handler.scoped(),
                ),
            )
            .await
            .map(|_| ())
    };
    tokio::select! {
        run = attempt => panic!("the attempt ended before its model call: {run:?}"),
        started = started_rx => started.expect("the attempt reaches its model call"),
    }

    let writer = resident.writer();
    let runtime = writer.lock().await;
    assert!(
        !runtime.engine_retries_root,
        "the dropped attempt's flag is down"
    );
    assert!(
        runtime.drive_root.is_none(),
        "the dropped attempt's sealed root is gone"
    );
    assert!(
        runtime.journaled_drive_claims.is_empty(),
        "the dropped attempt's journaled claims are gone: {:?}",
        runtime.journaled_drive_claims
    );
    assert_eq!(
        runtime.admitted_turn_index, None,
        "the dropped attempt's admitted turn index is gone"
    );
    assert!(
        !runtime.resident_session.is_valid(),
        "the resident session reloads from the durable session instead of serving the dropped attempt's state"
    );
}
