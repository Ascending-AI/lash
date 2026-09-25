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

use super::{
    ActiveTurnControl, AwaitEventKey, Resolution, RuntimeError, TurnCancelGatePair, TurnCancelMode,
    TurnCancellationEvidence,
};
use crate::RuntimeEffectControllerError;
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
    /// delivered before this returns when the gate answers, so a turn handed a
    /// fired lever meets the request at its first journaled peek; a delivery
    /// that keeps failing goes on retrying behind the guard.
    pub async fn forward_to(
        &self,
        control: Arc<ActiveTurnControl>,
        host: Arc<dyn EffectHost>,
    ) -> StopDeliveryGuard {
        if let Some(mode) = self.requested() {
            deliver_bounded(&control, host.as_ref(), mode, self.origin()).await;
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

/// The retry ladder every execution-side gate operation shares: a watch on
/// the gate pair, and the inline delivery of a stop already requested when a
/// turn starts. Eight attempts, from 25ms doubling to 1s (about 2.6s).
pub(crate) const GATE_RETRY_ATTEMPTS: usize = 8;
const GATE_RETRY_INITIAL: Duration = Duration::from_millis(25);
const GATE_RETRY_MAX: Duration = Duration::from_secs(1);

/// Resolve the gate pair with the stop, retrying for as long as the forwarding
/// runs — until the turn ends and drops its [`StopDeliveryGuard`]. A stop is
/// never given up on while the turn it addresses can still honour it.
async fn deliver(
    control: &ActiveTurnControl,
    host: &dyn EffectHost,
    mode: TurnCancelMode,
    origin: Option<String>,
) {
    deliver_within(control, host, mode, origin, None).await;
}

/// [`deliver`] on the retry ladder alone, for the inline delivery before a
/// turn starts: the forwarding task behind the guard keeps retrying after it.
async fn deliver_bounded(
    control: &ActiveTurnControl,
    host: &dyn EffectHost,
    mode: TurnCancelMode,
    origin: Option<String>,
) {
    deliver_within(control, host, mode, origin, Some(GATE_RETRY_ATTEMPTS)).await;
}

async fn deliver_within(
    control: &ActiveTurnControl,
    host: &dyn EffectHost,
    mode: TurnCancelMode,
    origin: Option<String>,
    attempts: Option<usize>,
) {
    let mut backoff = GATE_RETRY_INITIAL;
    let mut attempt: usize = 1;
    loop {
        let Err(error) = control
            .request_local_stop(host.await_event_resolver(), mode, origin.clone())
            .await
        else {
            return;
        };
        if attempt.is_power_of_two() {
            tracing::warn!(
                %error,
                attempt,
                ?mode,
                session_id = %control.address().session_id,
                turn_id = %control.address().turn_id,
                "forwarding a local turn stop to its durable gate failed; retrying"
            );
        }
        if attempts.is_some_and(|attempts| attempt >= attempts) {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(GATE_RETRY_MAX);
        attempt += 1;
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

impl TurnCancelGatePair {
    /// [`Self::await_stop`] on the shared retry ladder: a transient fault
    /// watching the gate is retried, and a watch that keeps failing ends as
    /// the typed live fault [`RuntimeEffectControllerError::turn_cancel_watch_lost`]
    /// — never as a stop. Whoever raced the watch then fails its attempt
    /// unrecorded, so the engine runs the step again (FIG-3672 P9).
    pub async fn await_stop_retrying<F, Fut>(
        &self,
        await_key: F,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeEffectControllerError>
    where
        F: Fn(AwaitEventKey) -> Fut,
        Fut: std::future::Future<Output = Result<Resolution, RuntimeError>>,
    {
        let mut backoff = GATE_RETRY_INITIAL;
        let mut attempt = 1;
        loop {
            match self.await_stop(&await_key).await {
                Ok(stop) => return Ok(stop),
                Err(error) if attempt < GATE_RETRY_ATTEMPTS && !error.code.is_terminal() => {
                    tracing::warn!(
                        %error,
                        attempt,
                        gate = %self.cancel.key_id,
                        "watching a turn's cancellation gate failed; retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2).min(GATE_RETRY_MAX);
                    attempt += 1;
                }
                Err(error) => {
                    return Err(RuntimeEffectControllerError::turn_cancel_watch_lost(
                        format!(
                            "watching turn cancellation gate `{}` failed after {attempt} attempt(s): {error}",
                            self.cancel.key_id
                        ),
                    ));
                }
            }
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
    /// resolver `host`, and whatever the body returns, finished or stopped by
    /// the token, is the step's recorded outcome. A replay serves that outcome and never runs this. `honoured`
    /// starts the token fired: the turn had already recorded its cancellation
    /// when it issued the step.
    ///
    /// A watch that gives up never stops the body: the body is dropped and
    /// this returns the typed live fault
    /// [`RuntimeEffectControllerError::turn_cancel_watch_lost`], which the
    /// engine never records, so the attempt ends and the step runs again.
    pub async fn run_step_body<T, F, Fut>(
        &self,
        host: &Arc<dyn EffectHost>,
        honoured: bool,
        body: F,
    ) -> Result<T, RuntimeEffectControllerError>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        self.run_step_body_with(host, honoured, WatchLoss::EndAttempt, body)
            .await
    }

    /// [`Self::run_step_body`] for a step whose engine records every
    /// outcome its body returns (a tool attempt): a watch that gives up
    /// leaves the body running to its own end under a stop that never fires,
    /// so the fault is neither recorded nor a cancellation. The turn still
    /// honours a request at its next journaled peek.
    pub async fn run_recorded_step_body<T, F, Fut>(
        &self,
        host: &Arc<dyn EffectHost>,
        honoured: bool,
        body: F,
    ) -> T
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        match self
            .run_step_body_with(host, honoured, WatchLoss::RunToEnd, body)
            .await
        {
            Ok(output) => output,
            Err(_) => unreachable!("a run-to-end step body never ends on a lost watch"),
        }
    }

    async fn run_step_body_with<T, F, Fut>(
        &self,
        host: &Arc<dyn EffectHost>,
        honoured: bool,
        on_loss: WatchLoss,
        body: F,
    ) -> Result<T, RuntimeEffectControllerError>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let stop = CancellationToken::new();
        if honoured {
            stop.cancel();
            return Ok(body(stop).await);
        }
        let resolver = host.await_event_resolver();
        let pair = self.gate_pair();
        let watch = pair.await_stop_retrying(|key| async move {
            // Never a fired token: firing the waiter's token would resolve
            // the turn's gate itself `Cancelled`. The watch ends by being
            // dropped when the body ends.
            resolver
                .await_await_event(&key, CancellationToken::new(), None)
                .await
        });
        let body = body(stop.clone());
        tokio::pin!(body);
        tokio::pin!(watch);
        let watched = tokio::select! {
            biased;
            output = &mut body => return Ok(output),
            watched = &mut watch => watched,
        };
        match watched {
            Ok(Some(_)) => {
                stop.cancel();
                Ok(body.await)
            }
            Ok(None) => Ok(body.await),
            Err(lost) if on_loss == WatchLoss::RunToEnd => {
                tracing::warn!(
                    session_id = %self.address().session_id,
                    turn_id = %self.address().turn_id,
                    error = %lost,
                    "a step body lost its turn's cancellation watch; it runs to its own end"
                );
                Ok(body.await)
            }
            Err(lost) => {
                tracing::warn!(
                    session_id = %self.address().session_id,
                    turn_id = %self.address().turn_id,
                    error = %lost,
                    "a step body lost its turn's cancellation watch; ending the attempt"
                );
                Err(lost)
            }
        }
    }
}

/// What a step body does when its gate watch gives up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchLoss {
    /// The body is dropped and the attempt ends with the unrecorded live
    /// fault (a model call, whose engine never records it).
    EndAttempt,
    /// The body runs to its own end (a step whose engine records every
    /// outcome).
    RunToEnd,
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
