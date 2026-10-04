//! A process segment's waits race the segment's own promises: its cancel
//! promise (FIG-3673) and, for a signal wait, its hand-over promise, which a
//! drain's wake resolves with the build generation it drains (FIG-3799).

use std::future::Future;

use lash_core::Resolution;
use restate_sdk::context::macro_support::SealedDurableFuture;
use restate_sdk::errors::TerminalError;

use super::{GateWait, RestateControllerContext};
use crate::durable_wait::{RestateDurableWaitAwaitRequest, RestateTurnCancelRaceOutcome};

/// Whether a wait that observes no turn races its process segment's durable
/// cancel promise (FIG-3673).
///
/// Only a process segment's own controller asks for the race, and only a
/// context with a workflow promise surface can answer it: every other wait
/// keeps the command shape it had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessCancelRace {
    /// The wait belongs to no process drive.
    NotRaced,
    /// The wait is a process drive's: its journal records whether it or the
    /// segment's cancel promise completed first.
    Raced,
}

/// How a process segment's signal wait ended when its cancel promise did
/// not win (FIG-3799): a cancel is the race's own
/// [`ProcessCancelled`](RestateTurnCancelRaceOutcome::ProcessCancelled).
#[derive(Clone, Debug, PartialEq)]
pub enum SignalWaitOutcome {
    /// The wait's event resolved first.
    Resolved(Resolution),
    /// The drain's wake won for the generation that admitted the segment:
    /// the wait is left open for a successor to wait on again.
    HandedOver,
}

/// How a turn's transferable wait ended when neither its turn's cancel nor
/// its session's revocation won (FIG-4739).
#[derive(Clone, Debug, PartialEq)]
pub enum TurnWaitOutcome {
    /// The wait's event resolved first.
    Resolved(Resolution),
    /// The drain's wake won for the generation the turn runs on: the wait is
    /// left open for the Run's successor segment to wait on again.
    HandedOver,
}

/// How a transferable turn sleep ended without cancellation or revocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnSleepOutcome {
    Resolved,
    HandedOver,
}

/// `await_event_or_turn_end` on a context whose turn waits take no drain
/// wake (a recording test context): the turn's gate alone races the event.
pub(super) async fn turn_cancel_only<'ctx, 'run, C>(
    context: &'run C,
    namespace: &'run crate::RestateNamespace,
    request: RestateDurableWaitAwaitRequest,
    replay_key: String,
    turn_cancel: RestateDurableWaitAwaitRequest,
) -> Result<RestateTurnCancelRaceOutcome<TurnWaitOutcome>, TerminalError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Ok(context
        .await_event_or_turn_cancel(
            namespace,
            request,
            replay_key,
            Some(turn_cancel),
            ProcessCancelRace::NotRaced,
        )
        .await?
        .map(TurnWaitOutcome::Resolved))
}

/// The index of whichever of `waits` the journal completed first:
/// [`first_of_gate_race`](super::first_of_gate_race) over any number of durable futures, with the same
/// non-consuming semantics — every other future stays awaitable.
fn first_completed(
    waits: &[&dyn SealedDurableFuture],
) -> impl Future<Output = Result<usize, TerminalError>> + Send + use<> {
    let inner = waits.first().map(|wait| wait.inner_context());
    let handles = waits.iter().map(|wait| wait.handle()).collect::<Vec<_>>();
    async move {
        let Some(inner) = inner else {
            return Err(TerminalError::new("a durable race needs at least one wait"));
        };
        let index = inner.select(handles.clone()).await?;
        if index < handles.len() {
            Ok(index)
        } else {
            Err(TerminalError::new(format!(
                "durable race completed out-of-range branch {index}"
            )))
        }
    }
}

/// The owner controls raced by a segment's removable observers.
#[derive(Default)]
pub(super) struct WaitControl<'run> {
    pub turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    pub generation: Option<lash_core::engine::BuildGeneration>,
    pub process_cancel: Option<GateWait<'run, String>>,
    pub process_hand_over: Option<GateWait<'run, String>>,
}

/// A segment races its short subscriptions against cancellation and its
/// generation's drain wake. Retirement and another generation's wake drop out.
pub(super) async fn race_segment_wait<'run, T>(
    mut events: Vec<GateWait<'run, T>>,
    mut cancel: Option<GateWait<'run, String>>,
    mut hand_over: Option<GateWait<'run, String>>,
    generation: Option<&lash_core::engine::BuildGeneration>,
) -> Result<super::TurnGateRace<T>, TerminalError> {
    if events.is_empty() {
        return Err(TerminalError::new("a segment race needs an event"));
    }
    loop {
        let race = {
            let mut waits: Vec<&dyn SealedDurableFuture> = events
                .iter()
                .map(|event| &**event as &dyn SealedDurableFuture)
                .collect();
            if let Some(cancel) = &cancel {
                waits.push(&**cancel);
            }
            if let Some(hand_over) = &hand_over {
                waits.push(&**hand_over);
            }
            first_completed(&waits)
        };
        let winner = race.await?;
        if winner < events.len() {
            return Ok(super::TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::Completed(events.remove(winner).await?),
            ));
        }
        if winner == events.len() && cancel.is_some() {
            let payload = cancel
                .take()
                .ok_or_else(|| TerminalError::new("invalid cancel race branch"))?
                .await?;
            if crate::process::process_cancel_promise_verdict(Some(payload)) {
                return Ok(super::TurnGateRace::Ended(
                    RestateTurnCancelRaceOutcome::ProcessCancelled,
                ));
            }
        } else {
            let payload = hand_over
                .take()
                .ok_or_else(|| TerminalError::new("invalid segment race branch"))?
                .await?;
            if generation.is_some_and(|generation| {
                crate::process::process_hand_over_verdict(&payload, generation)
            }) {
                return Ok(super::TurnGateRace::HandedOver);
            }
        }
    }
}

/// `await_signal_or_segment_end` on a context without the hand-over promise
/// (a recording test context): the cancel promise alone races the event, exactly as
/// [`await_event_or_turn_cancel`](RestateControllerContext::await_event_or_turn_cancel)
/// does.
pub(super) async fn cancel_only<'ctx, 'run, C>(
    context: &'run C,
    namespace: &'run crate::RestateNamespace,
    request: RestateDurableWaitAwaitRequest,
    replay_key: String,
) -> Result<RestateTurnCancelRaceOutcome<SignalWaitOutcome>, TerminalError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Ok(
        match context
            .await_event_or_turn_cancel(
                namespace,
                request,
                replay_key,
                None,
                ProcessCancelRace::Raced,
            )
            .await?
        {
            RestateTurnCancelRaceOutcome::Completed(resolution) => {
                RestateTurnCancelRaceOutcome::Completed(SignalWaitOutcome::Resolved(resolution))
            }
            RestateTurnCancelRaceOutcome::ProcessCancelled => {
                RestateTurnCancelRaceOutcome::ProcessCancelled
            }
            RestateTurnCancelRaceOutcome::TurnCancelled => {
                RestateTurnCancelRaceOutcome::TurnCancelled
            }
            RestateTurnCancelRaceOutcome::SessionRevoked { session_id } => {
                RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
            }
        },
    )
}

/// One of the segment's workflow promises as a durable future, on a context
/// that has a workflow promise surface; `None` on every other context.
macro_rules! process_promise {
    (promises, $context:ident, $run:lifetime, $ctx:expr, $key:expr) => {{
        // Workflow contexts are covariant in their lifetime: shortening it to
        // the borrow lets the SDK's `promise` borrow the context for exactly
        // as long as the race holds the future.
        let context: &$run $context<$run> = $ctx;
        Some(erase_gate_wait(
            restate_sdk::context::ContextPromises::promise::<String>(context, $key),
        ))
    }};
    (no_promises, $context:ident, $run:lifetime, $ctx:expr, $key:expr) => {{
        let _ = $ctx;
        None::<GateWait<$run, String>>
    }};
}

/// The segment's cancel promise (FIG-3673).
macro_rules! process_cancel_promise {
    ($promises:ident, $context:ident, $run:lifetime, $ctx:expr) => {
        process_promise!(
            $promises,
            $context,
            $run,
            $ctx,
            crate::process::PROCESS_CANCEL_PROMISE_KEY
        )
    };
}

/// The segment's hand-over promise, which the drain's wake resolves with the
/// generation it drains (FIG-3799).
macro_rules! process_hand_over_promise {
    ($promises:ident, $context:ident, $run:lifetime, $ctx:expr) => {
        process_promise!(
            $promises,
            $context,
            $run,
            $ctx,
            crate::process::PROCESS_HAND_OVER_PROMISE_KEY
        )
    };
}

/// `await_signal_or_segment_end` on a context with a workflow promise
/// surface: a short event subscription races the cancel and hand-over promises.
/// The wait hands over only to a wake naming `generation`, the generation
/// that admitted the segment. A cancel releases the losing event wait, as
/// the cancel race always has: nobody is left to resolve it. A hand-over
/// leaves the logical event open and acknowledges its exact unsubscribe.
macro_rules! process_signal_wait_method {
    ($promises:ident, $context:ident, $ctx_lifetime:lifetime) => {
        fn await_signal_or_segment_end<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitAwaitRequest,
            replay_key: String,
            generation: lash_core::engine::BuildGeneration,
        ) -> TurnCancelRaceFuture<'run, SignalWaitOutcome>
        where
            $ctx_lifetime: 'run,
        {
            Box::pin(process_signal_wait_body!(
                $promises, $context, 'run, self, namespace, request, replay_key, generation
            ))
        }
    };
}

macro_rules! process_signal_wait_body {
    (
        $promises:ident, $context:ident, $run:lifetime, $ctx:expr, $namespace:ident,
        $request:ident, $replay_key:ident, $generation:ident
    ) => {
        async move {
            let context: &$run $context<$run> = $ctx;
            let (Some(cancel), Some(hand_over)) = (
                process_cancel_promise!($promises, $context, $run, context),
                process_hand_over_promise!($promises, $context, $run, context),
            ) else {
                return Err(TerminalError::new("a process signal wait needs a workflow promise surface"));
            };
            match event_wait::wait(context, $namespace, $request, $replay_key,
                segment_wait::WaitControl { generation: Some($generation), process_cancel: Some(cancel),
                    process_hand_over: Some(hand_over), ..Default::default() },
                event_wait::awakeables(context)).await? {
                TurnGateRace::HandedOver => Ok(RestateTurnCancelRaceOutcome::Completed(SignalWaitOutcome::HandedOver)),
                TurnGateRace::Ended(outcome) => Ok(outcome.map(SignalWaitOutcome::Resolved)),
            }
        }
    };
}
