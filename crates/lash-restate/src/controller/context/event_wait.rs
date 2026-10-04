//! Event terminals stay at their logical key; segments hold only short observers.
use super::*;
use crate::durable_wait::{RestateDurableWaitAwakeableRequest, RestateDurableWaitRegistration};

pub(super) struct Awakeables<'run> {
    event: (String, GateWait<'run, Json<RestateTurnCancelWake>>),
    gate:
        Box<dyn Fn() -> (String, GateWait<'run, Json<RestateTurnCancelWake>>) + Send + Sync + 'run>,
}

pub(super) fn awakeables<'run, 'ctx, C>(context: &'run C) -> Awakeables<'run>
where
    C: ContextAwakeables<'ctx> + Sync,
    'ctx: 'run,
{
    Awakeables {
        event: gate_awakeable(context),
        gate: Box::new(move || gate_awakeable(context)),
    }
}

async fn cancel<'ctx, C: ContextClient<'ctx>>(
    context: &C,
    namespace: &crate::RestateNamespace,
    key: lash_core::AwaitEventKey,
    replay_key: String,
) -> Result<Resolution, TerminalError> {
    let address = RestateDurableWaitAddress::for_key(&key);
    let response = namespace
        .durable_wait_registry(context, address.index_key())
        .resolve(RestateDurableWaitResolveRequest {
            key,
            resolution: Resolution::Cancelled,
        })
        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
        .call()
        .await?
        .into_body();
    Ok(match response {
        RestateDurableWaitResolveResponse::Outcome(ResolveOutcome::AlreadyResolved {
            terminal,
        }) => terminal,
        _ => Resolution::Cancelled,
    })
}

pub(super) async fn wait<'run, C: ContextClient<'run>>(
    context: &'run C,
    namespace: &'run crate::RestateNamespace,
    request: RestateDurableWaitAwaitRequest,
    replay_key: String,
    control: segment_wait::WaitControl<'run>,
    awakeables: Awakeables<'run>,
) -> Result<TurnGateRace<Resolution>, TerminalError> {
    let segment_wait::WaitControl {
        turn_cancel,
        generation,
        process_cancel,
        process_hand_over,
    } = control;
    let address = RestateDurableWaitAddress::for_key(&request.key);
    let observer = RestateDurableWaitAwakeableRequest {
        key: request.key.clone(),
        awakeable_id: awakeables.event.0,
        hand_over: None,
    };
    let outcome = async {
        let logical = namespace
            .durable_wait_registry(context, address.index_key())
            .register(RestateDurableWaitIndexRequest {
                key: request.key.clone(),
            })
            .call()
            .await?
            .into_body();
        if let RestateDurableWaitRegistration::Resolved(terminal) = logical {
            return Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::Completed(terminal),
            ));
        }
        let registered = namespace
            .durable_wait_registry(context, address.index_key())
            .register_awakeable(observer.clone())
            .call()
            .await?
            .into_body();
        if matches!(registered, RestateDurableWaitRegistration::Revoked) {
            return Ok(TurnGateRace::Ended(match request.key.scope.session_id() {
                Some(session_id) => RestateTurnCancelRaceOutcome::SessionRevoked {
                    session_id: session_id.clone(),
                },
                None => RestateTurnCancelRaceOutcome::Completed(Resolution::Cancelled),
            }));
        }
        let race = if let Some(turn_cancel) = turn_cancel {
            let session = turn_cancel.key.scope.session_id().cloned().ok_or_else(|| {
                TerminalError::new("turn cancellation gate is missing its session id")
            })?;
            gate_race::race_turn_gate(
                context,
                namespace,
                &session,
                turn_cancel,
                generation,
                || (awakeables.gate)(),
                move || awakeables.event.1,
            )
            .await?
        } else {
            segment_wait::race_segment_wait(
                vec![awakeables.event.1],
                process_cancel,
                process_hand_over,
                generation.as_ref(),
            )
            .await?
        };
        Ok(match race {
            TurnGateRace::HandedOver => TurnGateRace::HandedOver,
            TurnGateRace::Ended(RestateTurnCancelRaceOutcome::Completed(Json(wake))) => {
                if wake == RestateTurnCancelWake::SessionRevoked {
                    TurnGateRace::Ended(match request.key.scope.session_id() {
                        Some(session_id) => RestateTurnCancelRaceOutcome::SessionRevoked {
                            session_id: session_id.clone(),
                        },
                        None => RestateTurnCancelRaceOutcome::Completed(Resolution::Cancelled),
                    })
                } else {
                    let registration = namespace
                        .durable_wait_registry(context, address.index_key())
                        .register(RestateDurableWaitIndexRequest {
                            key: request.key.clone(),
                        })
                        .call()
                        .await?
                        .into_body();
                    // An armed source owns this terminal; reread its seal
                    // after the wake without settling a second event promise.
                    match registration {
                        RestateDurableWaitRegistration::Resolved(terminal) => {
                            return Ok(TurnGateRace::Ended(
                                RestateTurnCancelRaceOutcome::Completed(terminal),
                            ));
                        }
                        RestateDurableWaitRegistration::Revoked => {
                            return Ok(TurnGateRace::Ended(match request.key.scope.session_id() {
                                Some(session_id) => RestateTurnCancelRaceOutcome::SessionRevoked {
                                    session_id: session_id.clone(),
                                },
                                None => {
                                    RestateTurnCancelRaceOutcome::Completed(Resolution::Cancelled)
                                }
                            }));
                        }
                        RestateDurableWaitRegistration::Registered => {}
                    }
                    let terminal = namespace
                        .durable_wait_workflow(context, address.workflow_key.clone())
                        .peek()
                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key.clone())
                        .call()
                        .await?
                        .into_body()
                        .unwrap_or(Resolution::Cancelled);
                    namespace
                        .durable_wait_registry(context, address.index_key())
                        .settle(crate::durable_wait::RestateDurableWaitSettleRequest {
                            key: request.key.clone(),
                            resolution: terminal.clone(),
                        })
                        .call()
                        .await?;
                    TurnGateRace::Ended(RestateTurnCancelRaceOutcome::Completed(terminal))
                }
            }
            TurnGateRace::Ended(RestateTurnCancelRaceOutcome::TurnCancelled) => {
                TurnGateRace::Ended(RestateTurnCancelRaceOutcome::TurnCancelled)
            }
            TurnGateRace::Ended(RestateTurnCancelRaceOutcome::ProcessCancelled) => {
                TurnGateRace::Ended(RestateTurnCancelRaceOutcome::ProcessCancelled)
            }
            TurnGateRace::Ended(RestateTurnCancelRaceOutcome::SessionRevoked { session_id }) => {
                TurnGateRace::Ended(RestateTurnCancelRaceOutcome::SessionRevoked { session_id })
            }
        })
    }
    .await;
    // This ACK replaces cancellation/attachment of the old long read. Every
    // exit, including an invocation cancellation, retires this exact observer.
    namespace
        .durable_wait_registry(context, address.index_key())
        .unregister_awakeable(observer)
        .call()
        .await?;
    match outcome {
        Err(error) if is_engine_cancellation(&error) => Ok(TurnGateRace::Ended(
            RestateTurnCancelRaceOutcome::Completed(
                cancel(context, namespace, request.key, replay_key).await?,
            ),
        )),
        Ok(TurnGateRace::Ended(outcome))
            if matches!(
                outcome,
                RestateTurnCancelRaceOutcome::ProcessCancelled
                    | RestateTurnCancelRaceOutcome::TurnCancelled
            ) =>
        {
            cancel(context, namespace, request.key, replay_key).await?;
            Ok(TurnGateRace::Ended(outcome))
        }
        outcome => outcome,
    }
}

macro_rules! event_wait_methods {
    ($context:ident, $promises:ident, $ctx:lifetime) => {
        fn await_event<'run>(
            &'run self, namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitAwaitRequest, replay_key: String,
            _cancellation: tokio_util::sync::CancellationToken,
        ) -> crate::JournaledFuture<'run, Resolution> where $ctx: 'run {
            let context: &'run $context<'run> = self;
            Box::pin(async move {
                match event_wait::wait(context, namespace, request, replay_key, segment_wait::WaitControl::default(),
                    event_wait::awakeables(context)).await? {
                    TurnGateRace::Ended(RestateTurnCancelRaceOutcome::Completed(resolution)) => Ok(resolution),
                    TurnGateRace::Ended(_) => Ok(Resolution::Cancelled),
                    TurnGateRace::HandedOver => Err(TerminalError::new("an event without a generation handed over")),
                }
            })
        }

        fn await_event_or_turn_cancel<'run>(
            &'run self, namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitAwaitRequest, replay_key: String,
            turn_cancel: Option<RestateDurableWaitAwaitRequest>, process_cancel: ProcessCancelRace,
        ) -> TurnCancelRaceFuture<'run, Resolution> where $ctx: 'run {
            let context: &'run $context<'run> = self;
            Box::pin(async move {
                let cancel = if turn_cancel.is_none() && process_cancel == ProcessCancelRace::Raced {
                    Some(process_cancel_promise!($promises, $context, 'run, context)
                        .ok_or_else(|| TerminalError::new("a process cancel race needs a workflow promise surface"))?)
                } else { None };
                match event_wait::wait(context, namespace, request, replay_key, segment_wait::WaitControl { turn_cancel, process_cancel: cancel, ..Default::default() },
                    event_wait::awakeables(context)).await? {
                    TurnGateRace::Ended(outcome) => Ok(outcome),
                    TurnGateRace::HandedOver => Err(TerminalError::new("an event without a generation handed over")),
                }
            })
        }

        fn await_event_or_turn_end<'run>(
            &'run self, namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitAwaitRequest, replay_key: String,
            turn_cancel: RestateDurableWaitAwaitRequest,
            generation: lash_core::engine::BuildGeneration,
        ) -> TurnCancelRaceFuture<'run, TurnWaitOutcome> where $ctx: 'run {
            let context: &'run $context<'run> = self;
            Box::pin(async move {
                match event_wait::wait(context, namespace, request, replay_key, segment_wait::WaitControl { turn_cancel: Some(turn_cancel), generation: Some(generation), ..Default::default() },
                    event_wait::awakeables(context)).await? {
                    TurnGateRace::HandedOver => Ok(RestateTurnCancelRaceOutcome::Completed(TurnWaitOutcome::HandedOver)),
                    TurnGateRace::Ended(outcome) => Ok(outcome.map(TurnWaitOutcome::Resolved)),
                }
            })
        }
    };
}
