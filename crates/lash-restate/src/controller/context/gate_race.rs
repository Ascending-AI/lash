//! A parked wait raced against its turn's durable cancel gate, and the
//! drain's hand-over wake a transferable wait's gate entry also takes
//! (FIG-4739).

use super::*;

/// Race one parked wait against this turn's durable cancel gate.
///
/// The gate awakeable's journaled value carries the mode of the request that
/// settled the gate. An `Immediate` settlement unwinds the wait at this wake,
/// exactly as every gate resolution did before the mode existed. An
/// `AfterStep` settlement composes to the step boundary instead: the wait
/// stays parked and finishes on its own terms, the iteration completes, and
/// the turn stops at its `turn_cancel.after_step.{n}` peek. So that a later
/// `Immediate` request still unwinds the wait, a deferred wake re-parks the
/// gate on the turn's escalation promise before continuing.
///
/// Journal order is the deployed contract: the awakeable, then its
/// registration, then whatever `guarded` emits. Sites whose guarded command
/// must precede the awakeable construct it first and hand it over through the
/// closure; the timer site constructs it in the closure so it lands after the
/// registration verdict. Every command a deferred wake adds sits on a branch
/// no journal written before the mode existed can take, so replay of an
/// in-flight invocation is unchanged.
pub(super) async fn race_turn_cancel_gate<'run, 'ctx, C, T>(
    context: &C,
    namespace: &crate::RestateNamespace,
    session_id: &SessionId,
    turn_cancel: RestateDurableWaitAwaitRequest,
    awakeable: impl Fn() -> (String, GateWait<'run, Json<RestateTurnCancelWake>>),
    guarded: impl FnOnce() -> GateWait<'run, T>,
) -> Result<RestateTurnCancelRaceOutcome<T>, TerminalError>
where
    C: ContextClient<'ctx>,
{
    match race_turn_gate(
        context,
        namespace,
        session_id,
        turn_cancel,
        None,
        awakeable,
        guarded,
    )
    .await?
    {
        TurnGateRace::Ended(outcome) => Ok(outcome),
        // The index wakes an entry with a hand-over only when the entry
        // registered one, and this one registered none.
        TurnGateRace::HandedOver => Err(TerminalError::new(
            "a turn wait that registered no hand-over was handed over",
        )),
    }
}

/// How a parked wait's race against its turn's gate ended.
pub(super) enum TurnGateRace<T> {
    Ended(RestateTurnCancelRaceOutcome<T>),
    /// The drain of the generation the gate entry registered woke it
    /// (FIG-4739): the guarded wait is left open, awaited by nobody here.
    HandedOver,
}

/// [`race_turn_cancel_gate`], with the gate entry registered for the drain's
/// hand-over wake when `hand_over` names the build generation the waiting
/// turn runs on (FIG-4739).
///
/// A hand-over wake ends the race with the guarded wait still open and the
/// gate entry already dropped by the index, as a fired gate's is. Nothing of
/// it is taken after an after-step stop was observed: the escalation entry
/// registers no hand-over, so a turn that owes its step boundary a stop
/// reaches that boundary where it is.
pub(super) async fn race_turn_gate<'run, 'ctx, C, T>(
    context: &C,
    namespace: &crate::RestateNamespace,
    session_id: &SessionId,
    turn_cancel: RestateDurableWaitAwaitRequest,
    hand_over: Option<lash_core::engine::BuildGeneration>,
    awakeable: impl Fn() -> (String, GateWait<'run, Json<RestateTurnCancelWake>>),
    guarded: impl FnOnce() -> GateWait<'run, T>,
) -> Result<TurnGateRace<T>, TerminalError>
where
    C: ContextClient<'ctx>,
{
    race_turn_gate_many(
        context,
        namespace,
        session_id,
        turn_cancel,
        hand_over,
        awakeable,
        move || vec![guarded()],
    )
    .await
}

pub(super) async fn race_turn_gate_many<'run, 'ctx, C, T>(
    context: &C,
    namespace: &crate::RestateNamespace,
    session_id: &SessionId,
    turn_cancel: RestateDurableWaitAwaitRequest,
    hand_over: Option<lash_core::engine::BuildGeneration>,
    awakeable: impl Fn() -> (String, GateWait<'run, Json<RestateTurnCancelWake>>),
    guarded: impl FnOnce() -> Vec<GateWait<'run, T>>,
) -> Result<TurnGateRace<T>, TerminalError>
where
    C: ContextClient<'ctx>,
{
    let scope = turn_cancel.key.scope.clone();
    let authority_id = crate::durable_wait::restate_authority_id_for_key(&turn_cancel.key)
        .ok_or_else(|| {
            TerminalError::from_error(crate::durable_wait::restate_unknown_or_revoked())
        })?;
    let (awakeable_id, awakeable_wait) = awakeable();
    let gate = match register_turn_cancel_gate(
        context,
        namespace,
        session_id,
        turn_cancel.key,
        awakeable_id,
        hand_over,
    )
    .await?
    {
        RestateTurnCancelGate::Registered(gate) => *gate,
        RestateTurnCancelGate::Revoked => {
            return Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::SessionRevoked {
                    session_id: session_id.clone(),
                },
            ));
        }
    };
    let mut guarded = guarded();
    match first_of_many_gate_race(&guarded, &*awakeable_wait).await? {
        winner if winner < guarded.len() => {
            let value = guarded.remove(winner).await?;
            retire_turn_cancel_gate(context, namespace, session_id, gate).await?;
            return Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::Completed(value),
            ));
        }
        _ => {}
    }
    let Json(wake) = awakeable_wait.await?;
    match wake {
        RestateTurnCancelWake::TurnCancelled => {
            return Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::TurnCancelled,
            ));
        }
        RestateTurnCancelWake::SessionRevoked => {
            return Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::SessionRevoked {
                    session_id: session_id.clone(),
                },
            ));
        }
        RestateTurnCancelWake::HandedOver => return Ok(TurnGateRace::HandedOver),
        RestateTurnCancelWake::TurnCancelDeferred => {}
    }
    // The stop is deferred to the step boundary. The index dropped the gate
    // entry when it fired, so nothing is retired here; the wait now parks
    // against the escalation promise, which only an `Immediate` request that
    // found the gate holding this after-step request ever writes.
    tracing::debug!(
        target: "lash::restate",
        event = "restate.turn_cancel_deferred",
        session_id = session_id.as_str(),
        "after-step stop observed by a parked durable wait; composing to the step boundary"
    );
    let escalation_key = restate_await_event_key_for_authority(
        &authority_id,
        &scope,
        AwaitEventWaitIdentity::TurnCancelEscalation,
    )
    .map_err(TerminalError::from_error)?;
    let (escalation_id, escalation) = awakeable();
    let escalation_gate = match register_turn_cancel_gate(
        context,
        namespace,
        session_id,
        escalation_key,
        escalation_id,
        None,
    )
    .await?
    {
        RestateTurnCancelGate::Registered(gate) => *gate,
        RestateTurnCancelGate::Revoked => {
            return Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::SessionRevoked {
                    session_id: session_id.clone(),
                },
            ));
        }
    };
    match first_of_many_gate_race(&guarded, &*escalation).await? {
        winner if winner < guarded.len() => {
            // The escalation entry is retired whichever way the guarded wait
            // settles: it only ever exists on the deferred branch, so no
            // journal written before the mode existed can reach this
            // retirement, and a failing guarded wait would otherwise leave the
            // index holding an entry for a wait that is gone. The success path
            // keeps the deployed order — guarded value first, then the
            // retirement — byte for byte.
            let value = guarded.remove(winner).await;
            let retirement =
                retire_turn_cancel_gate(context, namespace, session_id, escalation_gate).await;
            let value = value?;
            retirement?;
            Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::Completed(value),
            ))
        }
        _ => {
            let Json(wake) = escalation.await?;
            Ok(TurnGateRace::Ended(match wake {
                // The escalation promise only ever holds an immediate request;
                // a deferred wake on it would be a weaker request that cannot
                // exist there, and is honoured as the stop it escalates. Nor
                // can a hand-over: the escalation entry registered none.
                RestateTurnCancelWake::TurnCancelled
                | RestateTurnCancelWake::TurnCancelDeferred
                | RestateTurnCancelWake::HandedOver => RestateTurnCancelRaceOutcome::TurnCancelled,
                RestateTurnCancelWake::SessionRevoked => {
                    RestateTurnCancelRaceOutcome::SessionRevoked {
                        session_id: session_id.clone(),
                    }
                }
            }))
        }
    }
}

fn first_of_many_gate_race<T, A>(
    guarded: &[GateWait<'_, T>],
    gate: &A,
) -> impl std::future::Future<Output = Result<usize, TerminalError>> + Send + use<T, A>
where
    A: SealedDurableFuture + ?Sized,
{
    let inner = gate.inner_context();
    let handles = guarded
        .iter()
        .map(|wait| wait.handle())
        .chain(std::iter::once(gate.handle()))
        .collect();
    async move { inner.select(handles).await }
}
