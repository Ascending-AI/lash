//! Run-owned waits on K4 sources. Only short subscriptions live at the source.
use super::*;
use crate::durable_wait::{
    RestateSourceArmReply, RestateSourceArmRequest, RestateSourceSealReply,
    RestateSourceSealRequest, RestateSourceSubscribeReply, RestateSourceSubscribeRequest,
};
use lash_core::tool_run::{
    SealOutcome, SealWriter, SourceDescriptor, SourceSeal, SourceSubscription,
};

pub(super) async fn arm<'ctx, C: ContextClient<'ctx>>(
    context: &C,
    namespace: &crate::RestateNamespace,
    descriptor: SourceDescriptor,
) -> Result<(), TerminalError> {
    let address = RestateDurableWaitAddress::for_key(&descriptor.source);
    match namespace
        .durable_wait_registry(context, address.index_key())
        .arm_source(RestateSourceArmRequest { descriptor })
        .call()
        .await?
        .into_body()
    {
        RestateSourceArmReply::Armed { .. } => Ok(()),
        RestateSourceArmReply::Refused { refusal } => Err(TerminalError::new(
            lash_core::RuntimeEffectControllerError::from(refusal).to_record(),
        )),
    }
}

pub(super) async fn cancel<'ctx, C: ContextClient<'ctx>>(
    context: &C,
    namespace: &crate::RestateNamespace,
    descriptor: SourceDescriptor,
) -> Result<SourceSeal, TerminalError> {
    let address = RestateDurableWaitAddress::for_key(&descriptor.source);
    let reply = namespace
        .durable_wait_registry(context, address.index_key())
        .seal_source(RestateSourceSealRequest {
            source: descriptor.source,
            writer: SealWriter::Owner {
                opener: descriptor.owner,
            },
            seal: SourceSeal::Cancelled,
        })
        .call()
        .await?
        .into_body();
    match reply {
        RestateSourceSealReply::Outcome {
            outcome: SealOutcome::Sealed { seal } | SealOutcome::AlreadySealed { seal },
        } => Ok(seal),
        RestateSourceSealReply::Refused { refusal } => Err(TerminalError::new(
            lash_core::RuntimeEffectControllerError::from(refusal).to_record(),
        )),
    }
}

type SourceAwakeable<'run> =
    Box<dyn Fn(usize) -> (String, GateWait<'run, (usize, SourceSeal)>) + Send + Sync + 'run>;

pub(super) struct Awakeables<'run> {
    pub source: SourceAwakeable<'run>,
    pub gate:
        Box<dyn Fn() -> (String, GateWait<'run, Json<RestateTurnCancelWake>>) + Send + Sync + 'run>,
}

pub(super) async fn wait<'run, C: ContextClient<'run>>(
    context: &'run C,
    namespace: &'run crate::RestateNamespace,
    subscriptions: Vec<SourceSubscription>,
    control: segment_wait::WaitControl<'run>,
    awakeables: Awakeables<'run>,
) -> Result<RestateTurnCancelRaceOutcome<(usize, SourceSeal)>, TerminalError> {
    let segment_wait::WaitControl {
        turn_cancel,
        generation,
        process_cancel,
        process_hand_over,
    } = control;
    let mut observers = Vec::new();
    let outcome = async {
        let mut events = Vec::new();
        let mut sealed = None;
        for (position, subscription) in subscriptions.into_iter().enumerate() {
            let address = RestateDurableWaitAddress::for_key(&subscription.source);
            let (awakeable_id, event) = (awakeables.source)(position);
            let request = RestateSourceSubscribeRequest {
                subscription,
                awakeable_id,
            };
            observers.push(request.clone());
            match namespace
                .durable_wait_registry(context, address.index_key())
                .subscribe_source(request.clone())
                .call()
                .await?
                .into_body()
            {
                RestateSourceSubscribeReply::Subscribed => {
                    events.push(event);
                }
                RestateSourceSubscribeReply::Sealed { seal } => {
                    sealed = Some((position, seal));
                    break;
                }
                RestateSourceSubscribeReply::Refused { refusal } => {
                    return Err(TerminalError::new(
                        lash_core::RuntimeEffectControllerError::from(refusal).to_record(),
                    ));
                }
            }
        }
        if let Some(seal) = sealed {
            Ok(TurnGateRace::Ended(
                RestateTurnCancelRaceOutcome::Completed(seal),
            ))
        } else if let Some(turn_cancel) = turn_cancel {
            let session = turn_cancel
                .key
                .scope
                .session_id()
                .cloned()
                .ok_or_else(|| TerminalError::new("a Run source wait has no session"))?;
            gate_race::race_turn_gate_many(
                context,
                namespace,
                &session,
                turn_cancel,
                generation,
                || (awakeables.gate)(),
                move || events,
            )
            .await
        } else {
            segment_wait::race_segment_wait(
                events,
                process_cancel,
                process_hand_over,
                generation.as_ref(),
            )
            .await
        }
    }
    .await;
    for observer in observers {
        let address = RestateDurableWaitAddress::for_key(&observer.subscription.source);
        namespace
            .durable_wait_registry(context, address.index_key())
            .unsubscribe_source(observer)
            .call()
            .await?;
    }
    match outcome? {
        TurnGateRace::Ended(outcome) => Ok(outcome),
        TurnGateRace::HandedOver => Err(TerminalError::new(
            lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::TurnWaitHandedOver,
                "the Run source wait handed over",
            )
            .to_record(),
        )),
    }
}

macro_rules! run_source_methods {
    ($context:ident, $promises:ident, $ctx:lifetime) => {
                fn arm_run_source<'run>(
                    &'run self, namespace: &'run crate::RestateNamespace,
                    descriptor: lash_core::tool_run::SourceDescriptor,
                ) -> crate::JournaledFuture<'run, ()> where $ctx: 'run {
                    Box::pin(source_wait::arm(self, namespace, descriptor))
                }
                fn cancel_run_source<'run>(
                    &'run self, namespace: &'run crate::RestateNamespace,
                    descriptor: lash_core::tool_run::SourceDescriptor,
                ) -> crate::JournaledFuture<'run, lash_core::tool_run::SourceSeal> where $ctx: 'run {
                    Box::pin(source_wait::cancel(self, namespace, descriptor))
                }
                fn await_run_sources<'run>(
                    &'run self, namespace: &'run crate::RestateNamespace,
                    subscriptions: Vec<lash_core::tool_run::SourceSubscription>,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    generation: Option<lash_core::engine::BuildGeneration>,
                    process_cancel: ProcessCancelRace,
                ) -> TurnCancelRaceFuture<'run, (usize, lash_core::tool_run::SourceSeal)> where $ctx: 'run {
                    let context: &'run $context<'run> = self;
                    let promise = if turn_cancel.is_none() && process_cancel == ProcessCancelRace::Raced {
                        let Some(promise) = process_cancel_promise!($promises, $context, 'run, context) else {
                            return Box::pin(async { Err(TerminalError::new("a process source wait needs a cancel promise")) });
                        };
                        Some(promise)
                    } else { None };
                    let hand_over = if turn_cancel.is_none() && generation.is_some() && process_cancel == ProcessCancelRace::Raced {
                        process_hand_over_promise!($promises, $context, 'run, context)
                    } else { None };
                    use restate_sdk::context::DurableFuture;
                    Box::pin(source_wait::wait(context, namespace, subscriptions, segment_wait::WaitControl { turn_cancel, generation, process_cancel: promise, process_hand_over: hand_over }, source_wait::Awakeables {
                            source: Box::new(move |position| {
                                let (id, wait) = context.awakeable::<Json<lash_core::tool_run::SourceSeal>>();
                                (id, erase_gate_wait(wait.map_ok(move |Json(seal)| (position, seal))))
                            }),
                            gate: Box::new(move || gate_awakeable(context)),
                        }))
                }

    };
}

macro_rules! run_source_defaults {
    ($ctx:lifetime) => {
        fn arm_run_source<'run>(
            &'run self,
            _namespace: &'run crate::RestateNamespace,
            _descriptor: lash_core::tool_run::SourceDescriptor,
        ) -> crate::JournaledFuture<'run, ()>
        where
            $ctx: 'run,
        {
            Box::pin(async { Err(TerminalError::new("Run source arming is unavailable")) })
        }

        fn cancel_run_source<'run>(
            &'run self,
            _namespace: &'run crate::RestateNamespace,
            _descriptor: lash_core::tool_run::SourceDescriptor,
        ) -> crate::JournaledFuture<'run, lash_core::tool_run::SourceSeal>
        where
            $ctx: 'run,
        {
            Box::pin(async { Err(TerminalError::new("Run source cancellation is unavailable")) })
        }

        fn await_run_sources<'run>(
            &'run self,
            _namespace: &'run crate::RestateNamespace,
            _subscriptions: Vec<lash_core::tool_run::SourceSubscription>,
            _turn_cancel: Option<RestateDurableWaitAwaitRequest>,
            _generation: Option<lash_core::engine::BuildGeneration>,
            _process_cancel: ProcessCancelRace,
        ) -> TurnCancelRaceFuture<'run, (usize, lash_core::tool_run::SourceSeal)>
        where
            $ctx: 'run,
        {
            Box::pin(async { Err(TerminalError::new("Run source waits are unavailable")) })
        }
    };
}
