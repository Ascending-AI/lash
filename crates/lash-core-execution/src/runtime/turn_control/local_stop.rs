//! A host-local stop, delivered as a durable request (FIG-3672 P9).
//!
//! A host that holds a turn in process — a shutdown lever, the facade's
//! `cancel` token, a process runner stopping its child turn — asks it to stop
//! through a [`LocalTurnStop`]. The drive never reads the handle. For as long
//! as a physical turn runs, its forwarding turns a fired stop into a
//! request on that turn's durable gate pair, with lash's internal evidence,
//! exactly as a routed [`TurnWorkDriver::request_cancel`](super::TurnWorkDriver::request_cancel)
//! would. The turn then honours it where it honours any request: at a
//! journaled peek, or through a recorded step whose body observed it. Replay
//! reads the same record, whatever the process-local handle did.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

use super::{ActiveTurnControl, TurnCancelMode};
use crate::runtime::EffectHost;

/// A host-local request to stop the turn it was handed to.
///
/// `Immediate` fires the handle's token; `AfterStep` leaves the token alone
/// and asks the turn to stop at its next step boundary. The first origin
/// recorded with a request wins; a token installed with an origin supplies the
/// origin when nothing else recorded one.
#[derive(Clone, Default)]
pub struct LocalTurnStop {
    immediate: CancellationToken,
    after_step: CancellationToken,
    origin: Arc<Mutex<LocalStopOrigin>>,
}

#[derive(Default)]
struct LocalStopOrigin {
    configured: Option<Option<String>>,
    observed: Option<Option<String>>,
}

impl LocalTurnStop {
    /// A stop nothing has requested yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A stop requested `Immediate` when `token` fires, with `origin` as the
    /// origin to record if the token fires on its own.
    pub fn from_token(token: CancellationToken, origin: Option<String>) -> Self {
        let stop = Self {
            immediate: token,
            ..Self::default()
        };
        stop.origin.lock_recover().configured = Some(origin);
        stop
    }

    /// Request the stop in `mode`, recording `origin` unless an earlier request
    /// already recorded one.
    pub fn request(&self, mode: TurnCancelMode, origin: Option<String>) {
        {
            let mut state = self.origin.lock_recover();
            if state.observed.is_none() {
                state.observed = Some(origin);
            }
        }
        match mode {
            TurnCancelMode::Immediate => self.immediate.cancel(),
            TurnCancelMode::AfterStep => self.after_step.cancel(),
        }
    }

    /// The origin a forwarded request carries.
    pub fn origin(&self) -> Option<String> {
        let state = self.origin.lock_recover();
        state
            .observed
            .clone()
            .or_else(|| state.configured.clone())
            .flatten()
    }

    /// The `Immediate` lever's token, for a host-level wait that runs before
    /// or outside a turn's drive (a queued drain waiting for its lane). Drive
    /// code never reads it.
    pub fn immediate_token(&self) -> CancellationToken {
        self.immediate.clone()
    }

    /// The strongest mode requested so far, if any.
    pub fn requested(&self) -> Option<TurnCancelMode> {
        if self.immediate.is_cancelled() {
            Some(TurnCancelMode::Immediate)
        } else if self.after_step.is_cancelled() {
            Some(TurnCancelMode::AfterStep)
        } else {
            None
        }
    }

    /// Forward this stop onto `control`'s gate pair, over `host`'s deployment
    /// resolver, until the returned guard drops. A stop already requested is
    /// delivered before this returns, so a turn handed a fired lever meets
    /// the request at its first journaled peek.
    pub async fn forward_to(
        &self,
        control: Arc<ActiveTurnControl>,
        host: Arc<dyn EffectHost>,
    ) -> StopDeliveryGuard {
        if let Some(mode) = self.requested() {
            deliver(&control, host.as_ref(), mode, self.origin()).await;
        }
        let stop = self.clone();
        let done = CancellationToken::new();
        let finished = done.clone();
        let task = crate::task::spawn(async move {
            tokio::select! {
                biased;
                () = finished.cancelled() => {}
                () = stop.forward(&control, host.as_ref()) => {}
            }
        });
        StopDeliveryGuard {
            done,
            task: Some(task),
        }
    }

    async fn forward(&self, control: &ActiveTurnControl, host: &dyn EffectHost) {
        tokio::select! {
            biased;
            () = self.immediate.cancelled() => {}
            () = self.after_step.cancelled() => {
                deliver(control, host, TurnCancelMode::AfterStep, self.origin()).await;
                self.immediate.cancelled().await;
            }
        }
        deliver(control, host, TurnCancelMode::Immediate, self.origin()).await;
    }
}

const LOCAL_STOP_ATTEMPTS: usize = 8;
const LOCAL_STOP_RETRY_INITIAL: Duration = Duration::from_millis(25);
const LOCAL_STOP_RETRY_MAX: Duration = Duration::from_secs(1);

/// Resolve the gate pair with the stop, retrying a transient failure: a lost
/// local stop leaves the turn running, which is what an unforwarded stop would
/// have done too, so exhaustion is logged rather than raised.
async fn deliver(
    control: &ActiveTurnControl,
    host: &dyn EffectHost,
    mode: TurnCancelMode,
    origin: Option<String>,
) {
    let mut backoff = LOCAL_STOP_RETRY_INITIAL;
    for attempt in 1..=LOCAL_STOP_ATTEMPTS {
        match control
            .request_local_stop(host.await_event_resolver(), mode, origin.clone())
            .await
        {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!(
                    %error,
                    attempt,
                    ?mode,
                    session_id = %control.address().session_id,
                    turn_id = %control.address().turn_id,
                    "forwarding a local turn stop to its durable gate failed"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(LOCAL_STOP_RETRY_MAX);
            }
        }
    }
}

/// Keeps one execution-side stop delivery running — a forwarded
/// [`LocalTurnStop`], or a stop lent to tool children — for as long as it
/// lives; dropping it ends the delivery.
pub struct StopDeliveryGuard {
    done: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for StopDeliveryGuard {
    fn drop(&mut self) {
        self.done.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl ActiveTurnControl {
    /// Execution-side only: run one recorded step body under a cooperative
    /// cancel that fires when the turn's gate pair asks it to stop now.
    ///
    /// This is how a race against a step keeps its loser on an engine that
    /// cannot select a running step away (ADR 0105 §3, FIG-3672 P9): the
    /// engine's recorded body watches the gate itself, over the deployment
    /// resolver `host`, and whatever the body returns — finished, or stopped
    /// by the token — is the step's recorded outcome. A replay serves that
    /// outcome and never runs this. `honoured` starts the token fired: the
    /// turn had already recorded its cancellation when it issued the step.
    /// A watch that fails also fires the token, closed rather than open.
    pub async fn run_step_body<T, F, Fut>(
        &self,
        host: &dyn EffectHost,
        honoured: bool,
        body: F,
    ) -> T
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let stop = CancellationToken::new();
        if honoured {
            stop.cancel();
        }
        let body = body(stop.clone());
        tokio::pin!(body);
        if stop.is_cancelled() {
            return body.await;
        }
        let watch_done = CancellationToken::new();
        let watch = self.watch_immediate(host.await_event_resolver(), watch_done.clone());
        tokio::pin!(watch);
        let output = tokio::select! {
            biased;
            output = &mut body => Some(output),
            observed = &mut watch => {
                if let Err(error) = observed.as_ref() {
                    tracing::warn!(
                        %error,
                        session_id = %self.address().session_id,
                        turn_id = %self.address().turn_id,
                        "a step body could not watch its turn's cancellation gate; stopping it"
                    );
                }
                if !matches!(observed, Ok(None)) {
                    stop.cancel();
                }
                None
            }
        };
        watch_done.cancel();
        match output {
            Some(output) => output,
            None => body.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_recorded_origin_wins() {
        let stop = LocalTurnStop::new();
        stop.request(TurnCancelMode::AfterStep, Some("shutdown".to_string()));
        stop.request(TurnCancelMode::Immediate, Some("user".to_string()));
        assert_eq!(stop.origin().as_deref(), Some("shutdown"));
        assert_eq!(stop.requested(), Some(TurnCancelMode::Immediate));
    }

    #[test]
    fn an_explicitly_absent_origin_is_kept() {
        let stop = LocalTurnStop::new();
        stop.request(TurnCancelMode::Immediate, None);
        stop.request(TurnCancelMode::Immediate, Some("user".to_string()));
        assert_eq!(stop.origin(), None);
    }

    #[test]
    fn a_token_origin_yields_to_a_recorded_request_origin() {
        let token = CancellationToken::new();
        let stop = LocalTurnStop::from_token(token.clone(), Some("shutdown".to_string()));
        assert_eq!(stop.origin().as_deref(), Some("shutdown"));
        assert_eq!(stop.requested(), None);
        token.cancel();
        assert_eq!(stop.requested(), Some(TurnCancelMode::Immediate));
        stop.request(TurnCancelMode::Immediate, Some("user".to_string()));
        assert_eq!(stop.origin().as_deref(), Some("user"));
    }

    #[test]
    fn an_originless_token_does_not_block_a_later_origin() {
        let stop = LocalTurnStop::from_token(CancellationToken::new(), None);
        stop.request(TurnCancelMode::AfterStep, Some("user".to_string()));
        assert_eq!(stop.origin().as_deref(), Some("user"));
        assert_eq!(stop.requested(), Some(TurnCancelMode::AfterStep));
    }
}
