use super::*;

use lash::TurnEvent;

use lash::provider::LlmTransportError;
use lash::{TurnActivity, TurnInput};
use lash_core::facade_support::{SessionObservationSubscription, SessionResume, TurnOutcome};

use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;

use lash_facade_tests::streaming::{
    AppTools, FailingAppTools, PendingAppTools, QueuedWorkHydrationProbeFactory,
    app_tool_definition, assistant_prose, checkpoint_gated_provider, retry_once_provider,
    run_async_test_on_stack_budget, semantic_group_provider, tool_roundtrip_provider,
};
use tokio::sync::{Mutex as TokioMutex, oneshot};

mod builders_and_queue;
mod control_and_cancel;
mod observations;
mod rlm_streaming;
mod tool_live;

/// Observe the scheduling boundary even when its existing deadline panics.
/// This distinguishes startup, ready-but-unclaimed work and a claimed turn
/// that has not reached the provider/publication named by the law.
async fn streaming_boundary<T: Send>(
    backend: &lash_execution_contracts::backend::Backend,
    session: &SessionId,
    boundary: &str,
    future: impl std::future::Future<Output = T> + Send,
) -> T {
    use futures_util::FutureExt as _;
    let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
    let durable = backend.durable();
    let actor =
        lash_core::durable_port::ActorKey::session(session.as_str()).expect("session actor");
    eprintln!(
        "streaming boundary={boundary} reached={} registered_nodes={:?} readiness={:?} actor={:?}",
        result.is_ok(),
        durable.live_nodes().await.map(|nodes| nodes.len()),
        durable.readiness().await,
        durable.actor(&actor).await,
    );
    match result {
        Ok(answer) => answer,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Package installation and registration are asynchronous setup, outside
/// the provider/publication interleaving's unchanged one/two-second budget.
async fn streaming_registered(backend: &lash_execution_contracts::backend::Backend) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if !backend
                .durable()
                .live_nodes()
                .await
                .expect("live nodes")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the streaming fixture's node registered");
}
