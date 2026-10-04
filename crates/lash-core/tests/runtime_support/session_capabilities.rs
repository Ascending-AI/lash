//! Construct capabilities through the Run that publishes their plugin view.

use crate::runtime::tests::*;
use lash_core::testing::TestTurnExecution as _;

pub(crate) fn activate<'a>(
    runtime: &'a mut LashRuntime,
    double: &'a lash_restate_test::RestateTestBackend,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        if runtime.plugin_session().is_some() {
            return;
        }
        let request = "fixture-capabilities";
        let handler = double
            .open_handler(AdmittedScope::turn(
                runtime.session_id().clone(),
                TurnId::fixture(request),
            ))
            .await
            .expect("open the capability Run's handler");
        let store = runtime
            .services
            .store
            .clone()
            .expect("a Run-owned fixture has a store");
        if runtime.state().head_revision == 0 {
            // These fixtures supply the creator's state by hand. Record it
            // before admission, including the authority chosen by its plugins.
            let commit = lash_core::RuntimeCommit::persisted_state_for_test(runtime.state());
            store
                .commit_runtime_state_verified(commit, runtime.host.core.tracing.metrics())
                .await
                .expect("record the fixture's creation state");
        }
        store
            .store()
            .enqueue_queued_work(QueuedWorkBatchDraft::new(
                runtime.session_id().clone(),
                lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                SessionCommand::RefreshToolCatalog {
                    reason: request.into(),
                },
            ))
            .await
            .expect("send the fixture's catalog command");
        Box::pin(runtime.execute_next_run(
            request,
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        ))
        .await
        .expect("execute the capability command Run");
        handler.close().await.expect("close the capability handler");
        assert!(
            runtime.plugin_session().is_some(),
            "Run published capabilities"
        );
    })
}
