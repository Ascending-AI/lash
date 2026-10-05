//! The `RestateControllerContext` seam over Restate's context shapes.
//!
//! One responsibility: expose the durable primitives the controller needs
//! (timer, `ctx.run`, workflow scheduling, durable wait, awakeable races) over
//! every Restate context shape a handler can hold, and hold the SDK's
//! suspension protocol — including one-shot fusing when the SDK records a
//! terminal state behind `Pending`.

/// version_surface = "coexist"
use lash_sansio::SessionId;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use lash_core::{
    AwaitEventWaitIdentity, ProcessExecutionContext, ProcessRegistration, Resolution,
    ResolveOutcome,
};
use restate_sdk::context::macro_support::SealedDurableFuture;
use restate_sdk::context::{
    Context as RestateContext, ContextAwakeables, ContextClient, ObjectContext, RunRetryPolicy,
    SharedObjectContext, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::Json;

pub use super::process_scheduling::ProcessWorkflowStartFailure;

use serde::{Serialize, de::DeserializeOwned};

use crate::durable_wait::process_terminal::RestateProcessTerminalRequest;
use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitAwaitRequest, RestateDurableWaitEffectRequest,
    RestateDurableWaitIndexRequest, RestateDurableWaitResolveRequest,
    RestateDurableWaitResolveResponse, RestateTurnCancelGate, RestateTurnCancelRaceOutcome,
    RestateTurnCancelWake, RestateTurnGatePeek, durable_wait_index_object_key,
    register_turn_cancel_gate, restate_await_event_key_for_authority, retire_turn_cancel_gate,
};
use crate::process::{
    RestateProcessCancelRequest, RestateProcessWorkflowInput, RestateProcessWorkflowOutput,
    RestateProcessWorkflowPayload,
};

#[macro_use]
mod index_calls;
mod gate_race;
#[macro_use]
mod segment_wait;
#[macro_use]
mod source_wait;
#[macro_use]
mod event_wait;
mod contract;
pub use contract::RestateControllerContext;
mod run_bridge;
mod wake;
pub(crate) use crate::durable_wait::LASH_REPLAY_KEY_HEADER;
use gate_race::{TurnGateRace, race_turn_cancel_gate, race_turn_gate};
pub use segment_wait::{ProcessCancelRace, SignalWaitOutcome, TurnSleepOutcome, TurnWaitOutcome};
#[cfg(test)]
pub(crate) use wake::guard_restate_context_future;

/// Whether `error` is the engine's cancellation of this invocation. The SDK
/// surfaces it once, at whichever await the handler is parked on when the
/// signal lands, and journals the signal, so a replay meets it at the same
/// await; every later journaled step completes as usual.
pub(crate) fn is_engine_cancellation(error: &TerminalError) -> bool {
    error.code() == 409
}

/// The future every turn-cancel race returns across this seam.
///
/// `T` is what the guarded wait produces when it wins: `()` for a timer, a
/// `Resolution` for an await-event, the process output for a process await.
/// What a handler's durable-wait resolve answers: the index's outcome, or its
/// typed cancel-decided refusal (ADR 0099 §4, W17).
pub(crate) type ResolveEventFuture<'run> =
    crate::JournaledFuture<'run, RestateDurableWaitResolveResponse>;

type TurnCancelRaceFuture<'run, T> = crate::JournaledFuture<'run, RestateTurnCancelRaceOutcome<T>>;

/// Which side of a gate race the journal completed first.
enum GateRaceWinner {
    Guarded,
    Gate,
}

/// A durable SDK future that can also be raced by notification handle.
///
/// The concrete sites erase the SDK's opaque futures into this object before
/// handing them to the generic race so the race never has to project the
/// opaque type through the context type parameter (rust-lang/rust#100013).
trait GateWaitFuture: Future + SealedDurableFuture {}

impl<F: Future + SealedDurableFuture> GateWaitFuture for F {}

type GateWait<'run, T> =
    Pin<Box<dyn GateWaitFuture<Output = Result<T, TerminalError>> + Send + 'run>>;

/// The engine's first-completed await over the notification handles `keys`
/// name: the index of the first the journal completes, non-consuming so every
/// other source stays awaitable — [`first_completed`] for sources a
/// coordinator names by key.
fn select_run_sources<'ctx, 'run, C>(
    context: &'run C,
    keys: Vec<u32>,
) -> crate::JournaledFuture<'run, usize>
where
    C: restate_sdk::context::ContextSideEffects<'ctx>,
    'ctx: 'run,
{
    let inner = context.inner_context().clone();
    Box::pin(async move {
        let handles = keys
            .into_iter()
            .map(|key| Some(key.into()))
            .collect::<Vec<_>>();
        let index = inner.select(handles.clone()).await?;
        if index < handles.len() {
            Ok(index)
        } else {
            Err(TerminalError::new(format!(
                "a source selection completed out-of-range branch {index}"
            )))
        }
    })
}

/// A fresh gate awakeable, erased for the race.
fn gate_awakeable<'run, 'ctx, C>(
    context: &'run C,
) -> (String, GateWait<'run, Json<RestateTurnCancelWake>>)
where
    C: ContextAwakeables<'ctx>,
    'ctx: 'run,
{
    let (id, wait) = context.awakeable::<Json<RestateTurnCancelWake>>();
    (id, erase_gate_wait(wait))
}

fn erase_gate_wait<'run, T>(
    wait: impl GateWaitFuture<Output = Result<T, TerminalError>> + Send + 'run,
) -> GateWait<'run, T> {
    Box::pin(wait)
}

/// This is the SDK's `select!` without its consuming semantics: the macro
/// awaits the winner and drops the loser, but a deferred wake must keep the
/// guarded wait alive and await it afterwards. The VM's first-completed await
/// does not consume either notification, so the loser stays awaitable.
fn first_of_gate_race<G, A>(
    guarded: &G,
    gate: &A,
) -> impl Future<Output = Result<GateRaceWinner, TerminalError>> + Send + use<G, A>
where
    G: SealedDurableFuture + ?Sized,
    A: SealedDurableFuture + ?Sized,
{
    // Take the handles synchronously so no borrow of the erased futures is
    // held across the await.
    let inner = guarded.inner_context();
    let handles = vec![guarded.handle(), gate.handle()];
    async move {
        match inner.select(handles).await? {
            0 => Ok(GateRaceWinner::Guarded),
            1 => Ok(GateRaceWinner::Gate),
            index => Err(TerminalError::new(format!(
                "gate race completed out-of-range branch {index}"
            ))),
        }
    }
}

/// Race one parked wait of a process segment against the segment's own
/// durable cancel promise (FIG-3673).
///
/// The promise is the segment workflow's `process_cancel_requested`, which
/// the `cancel` and `deliver_cancel` handlers resolve after the registry
/// records the request. Journal order is the deployed contract: the guarded
/// wait's command, then the promise's. The VM's first-completed await records
/// which completed first, so a replay takes the branch the first execution
/// took whatever the promise's state is by then. A promise holding the
/// segment's own `SegmentFinished` retirement is not a cancel: the guarded
/// wait finishes on its own terms.
async fn race_process_cancel<'run, T>(
    promise: GateWait<'run, String>,
    guarded: GateWait<'run, T>,
) -> Result<RestateTurnCancelRaceOutcome<T>, TerminalError> {
    match first_of_gate_race(&*guarded, &*promise).await? {
        GateRaceWinner::Guarded => {}
        GateRaceWinner::Gate => {
            let payload = promise.await?;
            if crate::process::process_cancel_promise_verdict(Some(payload)) {
                return Ok(RestateTurnCancelRaceOutcome::ProcessCancelled);
            }
        }
    }
    guarded.await.map(RestateTurnCancelRaceOutcome::Completed)
}

macro_rules! impl_process_cancel_peek {
    (promises, $ctx:expr) => {
        Box::pin(async move {
            let payload = restate_sdk::context::ContextPromises::peek_promise::<String>(
                $ctx,
                crate::process::PROCESS_CANCEL_PROMISE_KEY,
            )
            .await?;
            Ok(crate::process::process_cancel_promise_verdict(payload))
        })
    };
    (no_promises, $ctx:expr) => {
        Box::pin(async move { Ok(false) })
    };
}

macro_rules! impl_restate_controller_context {
    ($($context:ident : $promises:ident),+ $(,)?) => {
        $(
            impl<'ctx> RestateControllerContext<'ctx> for $context<'ctx> {
                fn invocation_id(&self) -> &str {
                    $context::invocation_id(self)
                }

                fn sleep_send<'run>(
                    &'run self,
                    duration: Duration,
                ) -> crate::JournaledFuture<'run, ()>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        restate_sdk::context::ContextTimers::sleep(self, duration).await
                    })
                }

                fn start_sleep_send<'run>(
                    &'run self,
                    duration: Duration,
                ) -> (Option<u32>, crate::JournaledFuture<'run, ()>)
                where 'ctx: 'run,
                {
                    let timer = restate_sdk::context::ContextTimers::sleep(self, duration);
                    let key = SealedDurableFuture::handle(&timer).map(u32::from);
                    (key, Box::pin(timer))
                }

                fn sleep_or_turn_cancel<'run>(
                    &'run self,
                    namespace: &'run crate::RestateNamespace,
                    duration: Duration,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    process_cancel: ProcessCancelRace,
                ) -> TurnCancelRaceFuture<'run, ()>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            // `sleep()` journals `sys_sleep` synchronously at
                            // construction, ahead of the promise it races.
                            let timer = erase_gate_wait(
                                restate_sdk::context::ContextTimers::sleep(self, duration),
                            );
                            let promise = match process_cancel {
                                ProcessCancelRace::Raced => {
                                    process_cancel_promise!($promises, $context, 'run, self)
                                }
                                ProcessCancelRace::NotRaced => None,
                            };
                            return match promise {
                                Some(promise) => race_process_cancel(promise, timer).await,
                                None => timer.await.map(RestateTurnCancelRaceOutcome::Completed),
                            };
                        };

                        let Some(session_id) = turn_cancel.key.scope.session_id().cloned()
                        else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        // Journal order is the deployed contract: the awakeable,
                        // then its registration, then the timer. The race helper
                        // owns the index calls and the wake verdict; this site
                        // keeps the timer behind the registration verdict.
                        race_turn_cancel_gate(
                            self,
                            namespace,
                            &SessionId::from(session_id),
                            turn_cancel,
                            || gate_awakeable(self),
                            || erase_gate_wait(restate_sdk::context::ContextTimers::sleep(
                                self, duration,
                            )),
                        )
                        .await
                    })
                }

                fn sleep_or_turn_end<'run>(
                    &'run self,
                    namespace: &'run crate::RestateNamespace,
                    duration: Duration,
                    turn_cancel: RestateDurableWaitAwaitRequest,
                    generation: lash_core::engine::BuildGeneration,
                ) -> TurnCancelRaceFuture<'run, TurnSleepOutcome>
                where 'ctx: 'run,
                {
                    Box::pin(async move {
                        let session_id = turn_cancel.key.scope.session_id().cloned()
                            .ok_or_else(|| TerminalError::new("turn sleep gate is missing its session id"))?;
                        match race_turn_gate(
                            self, namespace, &SessionId::from(session_id), turn_cancel,
                            Some(generation), || gate_awakeable(self),
                            || erase_gate_wait(restate_sdk::context::ContextTimers::sleep(self, duration)),
                        ).await? {
                            TurnGateRace::HandedOver => Ok(RestateTurnCancelRaceOutcome::Completed(TurnSleepOutcome::HandedOver)),
                            TurnGateRace::Ended(outcome) => Ok(outcome.map(|()| TurnSleepOutcome::Resolved)),
                        }
                    })
                }

                fn run_json_send<'run, T, Fut>(
                    &'run self,
                    effect_name: String,
                    retry_policy: Option<RunRetryPolicy>,
                    future: Fut,
                ) -> crate::JournaledFuture<'run, Json<T>>
                where
                    'ctx: 'run,
                    T: Serialize + DeserializeOwned + Send + 'static,
                    Fut: Future<Output = T> + Send + 'run,
                {
                    Box::pin(run_bridge::register(self, effect_name, retry_policy, async move { Ok(future.await) }))
                }

                fn run_json_eager_or_retry_send<'run, T, Fut>(
                    &'run self,
                    effect_name: String,
                    future: Fut,
                ) -> (
                    impl Future<Output = ()> + Send + 'run,
                    Option<u32>,
                    impl Future<Output = Result<Json<T>, TerminalError>> + Send + 'run,
                )
                where 'ctx: 'run,
                      T: Serialize + DeserializeOwned + Send + 'static,
                      Fut: Future<Output = Result<T, String>> + Send + 'run,
                {
                    run_bridge::issue(self, effect_name, None, future)
                }

                fn select_run_sources<'run>(
                    &'run self,
                    keys: Vec<u32>,
                ) -> crate::JournaledFuture<'run, usize>
                where 'ctx: 'run,
                {
                    select_run_sources(self, keys)
                }

                fn run_json_schedule_or_retry_send<'run, T, Fut>(
                    &'run self,
                    effect_name: String,
                    future: Fut,
                ) -> impl Future<Output = Result<Json<T>, TerminalError>> + Send + 'run
                where
                    'ctx: 'run,
                    T: Serialize + DeserializeOwned + Send + 'static,
                    Fut: Future<Output = Result<T, String>> + Send + 'run,
                {
                    let context: &'run $context<'run> = self;
                    run_bridge::schedule(context, effect_name, future)
                }

                fn run_json_or_retry_send<'run, T, Fut>(
                    &'run self,
                    effect_name: String,
                    future: Fut,
                ) -> impl Future<Output = Result<Json<T>, TerminalError>> + Send + 'run
                where
                    'ctx: 'run,
                    T: Serialize + DeserializeOwned + Send + 'static,
                    Fut: Future<Output = Result<T, String>> + Send + 'run,
                {
                    run_bridge::register(self, effect_name, None, future)
                }

                fn issue_run_realization<'run>(
                    &'run self, namespace: &'run crate::RestateNamespace,
                    request: lash_core::tool_dispatch::RealizationRequest,
                ) -> crate::JournaledFuture<'run, lash_core::tool_dispatch::IssuedRealization<'run>, lash_core::RuntimeEffectControllerError>
                where 'ctx: 'run {
Box::pin(async move {
                    use lash_core::tool_dispatch::{RunSelectable, SelectKey};
                    let key = request.key.to_string();
                    let send = crate::services::routed_service::<_, _, lash_core::tool_dispatch::RealizationReceipt>(
                        self, &namespace.stable(crate::LashService::ToolRealization), "realize", request,
                    ).idempotency_key(key).send();
                    let invocation = send.await.map_err(|error| crate::wire::lash_terminal(&error, lash_core::RuntimeErrorCode::EngineEffectController))?;
                    let attach = invocation.attach::<crate::Reply<lash_core::tool_dispatch::RealizationReceipt>>();
                    let handle = SealedDurableFuture::handle(&attach).ok_or_else(|| lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::EngineEffectController, "realization attach has no engine notification",
                    ))?;
                    Ok(lash_core::tool_dispatch::IssuedRealization { invocation_id: invocation.invocation_id().to_owned(), receipt: RunSelectable {
                        key: Box::pin(std::future::ready(Ok(SelectKey::from_engine(u32::from(handle))))),
                        value: Box::pin(async move {
                            attach.await.map(|reply| reply.body).map_err(|error| crate::wire::lash_terminal(&error, lash_core::RuntimeErrorCode::EngineEffectController))
                        }),
                    } })
                })
}


                fn attach_run_realization<'run>(&'run self, invocation_id: String)
                    -> crate::JournaledFuture<'run, lash_core::tool_dispatch::RunSelectable<'run, lash_core::tool_dispatch::RealizationReceipt>, lash_core::RuntimeEffectControllerError>
                    where 'ctx: 'run {
                    let invocation = ContextClient::invocation_handle(self, invocation_id);
                    let attach = invocation.attach::<crate::Reply<lash_core::tool_dispatch::RealizationReceipt>>();
                    let handle = SealedDurableFuture::handle(&attach);
                    Box::pin(async move {
                        let handle = handle.ok_or_else(|| lash_core::RuntimeEffectControllerError::new(lash_core::RuntimeErrorCode::EngineEffectController, "realization attach has no notification"))?;
                        Ok(lash_core::tool_dispatch::RunSelectable {
                            key: Box::pin(std::future::ready(Ok(lash_core::tool_dispatch::SelectKey::from_engine(u32::from(handle))))),
                            value: Box::pin(async move { attach.await.map(|reply| reply.body).map_err(|error| crate::wire::lash_terminal(&error, lash_core::RuntimeErrorCode::EngineEffectController)) }),
                        })
                    })
                }

                fn start_process_workflow<'run>(
                    &'run self,
                    namespace: &'run crate::RestateNamespace,
                    process_id: lash_core::ProcessId,
                    registration: ProcessRegistration,
                    execution_context: ProcessExecutionContext,
                    sender_generation: lash_core::engine::BuildGeneration,
                ) -> crate::JournaledFuture<'run, String, ProcessWorkflowStartFailure>
                where
                    'ctx: 'run,
                {
                    // A new process starts on the stable lane (FIG-3795):
                    // Restate runs segment 0 on the newest build.
                    let workflow_key = process_id.to_string();
                    let request = crate::services::routed_workflow::<_, _, RestateProcessWorkflowOutput>(
                        self,
                        &namespace.stable(crate::LashService::ProcessWorkflow),
                        workflow_key,
                        "run",
                        RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                            process_id,
                            registration,
                            execution_context,
                            segment_ordinal: 0,
                            sender_generation,
                        }),
                    );
                    let handle = request.send();
                    Box::pin(async move {
                        let handle = handle.await.map_err(ProcessWorkflowStartFailure::of_send)?;
                        Ok(handle.invocation_id().to_owned())
                    })
                }

                fn request_process_workflow_cancel<'run>(
                    &'run self,
                    namespace: &'run crate::RestateNamespace,
                    request: RestateProcessCancelRequest,
                ) -> crate::JournaledFuture<'run, ()>
                where
                    'ctx: 'run,
                {
                    // A process-level cancel goes to the stable root, which
                    // routes it on to the live segment's recorded lane.
                    let workflow_key = request.process_id.to_string();
                    let call = crate::services::routed_workflow::<_, _, ()>(
                        self,
                        &namespace.stable(crate::LashService::ProcessWorkflow),
                        workflow_key,
                        "cancel",
                        request,
                    )
                    .call();
                    Box::pin(async move {
                        call.await?;
                        Ok(())
                    })
                }

                event_wait_methods!($context, $promises, 'ctx);

                run_source_methods!($context, $promises, 'ctx);



                process_signal_wait_method!($promises, $context, 'ctx);

                durable_wait_index_methods!('ctx);

                fn attach_process_terminal<'run>(
                    &'run self,
                    namespace: &'run crate::RestateNamespace,
                    request: RestateProcessTerminalRequest,
                ) -> crate::JournaledFuture<'run, ()>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let owner = lash_core::EffectOpener::for_scope(&lash_core::AdmittedScope::new(request.key.scope.clone()))
                            .map_err(TerminalError::from_error)?;
                        let call_id = match &request.key.wait {
                            AwaitEventWaitIdentity::ToolCompletion { tool_call_id } => tool_call_id.clone(),
                            _ => owner.tool_call_admission().call_id(&[lash_sansio::ToolCallPosition::CodeCell(&request.key.key_id)]),
                        };
                        let descriptor = lash_core::tool_run::SourceDescriptor {
                            source: request.key.clone(), call_id, owner,
                            resolver: lash_core::plugin::PluginRevision::new("lash.process-terminal",
                                lash_core::plugin::BehaviorRevision::ONE),
                            authority: lash_core::tool_run::SourceAuthority::ProcessTerminal { process_id: request.process_id },
                            cancel: lash_core::tool_run::ExternalCancelPolicy::Ignore,
                        };
                        let subscription = crate::durable_wait::ProcessTerminalSubscription::for_source(descriptor)
                            .map_err(|error| TerminalError::new(error.to_record()))?;
                        let address = RestateDurableWaitAddress::for_key(&request.key);
                        let receiver = namespace.durable_wait_registry(self, address.index_key());
                        if !receiver.attach_process_terminal(subscription.clone()).call().await?.into_body() {
                            return Ok(());
                        }
                        let source = RestateDurableWaitAddress::for_key(&subscription.terminal);
                        let output = namespace.durable_wait_registry(self, source.index_key())
                            .subscribe_process_terminal(subscription.clone()).call().await?.into_body();
                        if let Some(output) = output {
                            receiver.deliver_process_terminal(crate::durable_wait::ProcessTerminalDelivery { subscription, output }).call().await?;
                        }
                        Ok(())
                    })
                }

                fn scope_effect_begin<'run>(
                    &'run self,
                    namespace: &'run crate::RestateNamespace,
                    index_key: String,
                    replay_key: String,
                ) -> crate::JournaledFuture<'run, bool>
                where
                    'ctx: 'run,
                {
                    let call = namespace.durable_wait_registry(self, index_key)
                        .begin_effect(RestateDurableWaitEffectRequest {
                            replay_key: replay_key.clone(),
                        })
                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                        .call();
                    Box::pin(async move {
                        let admitted = call.await?.into_body();
                        Ok(admitted)
                    })
                }
                fn scope_effect_end<'run>(
                    &'run self,
                    namespace: &'run crate::RestateNamespace,
                    index_key: String,
                    replay_key: String,
                ) -> crate::JournaledFuture<'run, ()>
                where
                    'ctx: 'run,
                {
                    let call = namespace.durable_wait_registry(self, index_key)
                        .end_effect(RestateDurableWaitEffectRequest {
                            replay_key: replay_key.clone(),
                        })
                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                        .call();
                    Box::pin(async move {
                        call.await?;
                        Ok(())
                    })
                }












                fn peek_process_cancel_requested<'run>(
                    &'run self,
                ) -> crate::JournaledFuture<'run, bool>
                where
                    'ctx: 'run,
                {
                    impl_process_cancel_peek!($promises, self)
                }
            }

        )+
    };
}

impl_restate_controller_context!(
    RestateContext: no_promises,
    SharedObjectContext: no_promises,
    ObjectContext: no_promises,
    SharedWorkflowContext: promises,
    WorkflowContext: promises,
);
