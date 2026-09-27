//! A process segment's waits race the segment's own promises: its cancel
//! promise (FIG-3673) and, for a signal wait, its hand-over promise, which a
//! drain's wake resolves with the build generation it drains (FIG-3799).

use std::future::Future;

use lash_core::Resolution;
use restate_sdk::context::macro_support::SealedDurableFuture;
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::Json;

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

/// Race a process segment's signal wait against the segment's cancel
/// promise and its hand-over promise (FIG-3673, FIG-3799).
///
/// Journal order is the contract: the event's call, then the cancel
/// promise's, then the hand-over promise's. A cancel promise holding the
/// segment's own `SegmentFinished` retirement is not a cancel, and a
/// hand-over naming another generation is not this segment's: each drops
/// out and the rest race on. The event is never released on a hand-over —
/// the wait stays open for the successor, and the orphaned call completes
/// harmlessly when the signal resolves it.
pub(super) async fn race_signal_wait<'run>(
    event: GateWait<'run, Json<Resolution>>,
    cancel: GateWait<'run, String>,
    hand_over: GateWait<'run, String>,
    generation: &lash_core::engine::BuildGeneration,
) -> Result<RestateTurnCancelRaceOutcome<SignalWaitOutcome>, TerminalError> {
    let mut cancel = Some(cancel);
    let mut hand_over = Some(hand_over);
    loop {
        // The handles are taken synchronously: no borrow of the futures is
        // held across the await.
        let race = {
            let mut waits: Vec<&dyn SealedDurableFuture> = vec![&*event];
            if let Some(cancel) = &cancel {
                waits.push(&**cancel);
            }
            if let Some(hand_over) = &hand_over {
                waits.push(&**hand_over);
            }
            first_completed(&waits)
        };
        let winner = race.await?;
        // Index 0 is the event; the promises follow in journal order among
        // the ones still racing.
        let promise = match winner {
            0 => break,
            1 if cancel.is_some() => SignalWaitPromise::Cancel,
            _ => SignalWaitPromise::HandOver,
        };
        match promise {
            SignalWaitPromise::Cancel => {
                if let Some(cancel) = cancel.take()
                    && crate::process::process_cancel_promise_verdict(Some(cancel.await?))
                {
                    return Ok(RestateTurnCancelRaceOutcome::ProcessCancelled);
                }
            }
            SignalWaitPromise::HandOver => {
                if let Some(hand_over) = hand_over.take()
                    && crate::process::process_hand_over_verdict(&hand_over.await?, generation)
                {
                    return Ok(RestateTurnCancelRaceOutcome::Completed(
                        SignalWaitOutcome::HandedOver,
                    ));
                }
            }
        }
    }
    let Json(resolution) = event.await?;
    Ok(RestateTurnCancelRaceOutcome::Completed(
        SignalWaitOutcome::Resolved(resolution),
    ))
}

/// Which of a signal wait's promises won a round of its race.
enum SignalWaitPromise {
    Cancel,
    HandOver,
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
/// surface (FIG-3799): the event wait's CallCommand, then the cancel
/// promise's, then the hand-over promise's, raced by [`race_signal_wait`].
/// The wait hands over only to a wake naming `generation`, the generation
/// that admitted the segment. A cancel releases the losing event wait, as
/// the cancel race always has: nobody is left to resolve it. A hand-over
/// leaves it open for the successor.
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
            let context = $ctx;
            let event_address = RestateDurableWaitAddress::for_key(&$request.key);
            let event = $namespace
                .durable_wait_workflow(context, event_address.workflow_key.clone())
                .await_resolution(Json($request.clone().into()))
                .header(LASH_REPLAY_KEY_HEADER.to_string(), $replay_key.clone());
            let event = erase_gate_wait(event.call());
            let (Some(cancel), Some(hand_over)) = (
                process_cancel_promise!($promises, $context, $run, context),
                process_hand_over_promise!($promises, $context, $run, context),
            ) else {
                return Err(TerminalError::new(
                    "a process signal wait needs a workflow promise surface",
                ));
            };
            let outcome = race_signal_wait(event, cancel, hand_over, &$generation).await?;
            if matches!(outcome, RestateTurnCancelRaceOutcome::ProcessCancelled) {
                let resolve = $namespace
                    .durable_wait_registry(context, durable_wait_index_object_key(&event_address))
                    .resolve(Json(RestateDurableWaitResolveRequest {
                        key: $request.key,
                        resolution: Resolution::Cancelled,
                    }))
                    .header(LASH_REPLAY_KEY_HEADER.to_string(), $replay_key);
                let Json(_) = resolve.call().await?;
            }
            Ok(outcome)
        }
    };
}
