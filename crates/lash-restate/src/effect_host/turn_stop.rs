//! The ingress host's race arm against a turn's cancellation gate
//! (FIG-3672 P9).

use lash_core::{AwaitEventResolver, ExecutionScope, RuntimeEffectControllerError};

use super::RestateEffectHostController;

impl RestateEffectHostController {
    /// Resolves once the gate pair of `scope`'s turn asks the turn to stop
    /// now, over this host's ingress; never for a wait that observes no turn.
    pub(super) async fn turn_stop(
        &self,
        scope: Option<&ExecutionScope>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Some(scope) = scope else {
            return std::future::pending().await;
        };
        let pair = lash_core::TurnCancelGatePair::for_scope(self, scope).await?;
        // A watch that keeps failing ends as the typed live fault, never as
        // a stop (FIG-3672 P9).
        match pair
            .await_stop_retrying(|key| async move {
                self.await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
                    .await
            })
            .await?
        {
            Some(_) => Ok(()),
            None => std::future::pending().await,
        }
    }
}
