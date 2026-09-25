//! The `RestateControllerContext` seam over Restate's context shapes.
//!
//! One responsibility: expose the durable primitives the controller needs
//! (timer, `ctx.run`, workflow scheduling, durable wait, awakeable races) over
//! every Restate context shape a handler can hold, and hold the SDK's
//! suspension protocol — including the one-shot fusing of a context future that
//! wakes synchronously and then returns `Pending`.

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lash_core::{
    AwaitEventWaitIdentity, ProcessAwaitOutput, ProcessExecutionContext, ProcessRegistration,
    Resolution, ResolveOutcome,
};
use restate_sdk::context::macro_support::SealedDurableFuture;
use restate_sdk::context::{
    Context as RestateContext, ContextAwakeables, ContextClient, ObjectContext, RunRetryPolicy,
    SharedObjectContext, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerError, TerminalError};
use restate_sdk::serde::Json;

pub use super::process_scheduling::ProcessWorkflowStartFailure;

use serde::{Serialize, de::DeserializeOwned};

use crate::durable_wait::{
    LashDurableWaitIndexClient, LashDurableWaitWorkflowClient, RestateDurableWaitAddress,
    RestateDurableWaitAwaitRequest, RestateDurableWaitDeadline, RestateDurableWaitEffectRequest,
    RestateDurableWaitGroupChildMembershipRequest, RestateDurableWaitGroupRequest,
    RestateDurableWaitResolveRefusal, RestateDurableWaitResolveRequest,
    RestateDurableWaitResolveResponse, RestateTurnCancelGate, RestateTurnCancelRaceOutcome,
    RestateTurnCancelWake, durable_wait_index_object_key, register_turn_cancel_gate,
    restate_await_event_key_for_authority, restate_durable_wait_request, retire_turn_cancel_gate,
};
use crate::effect_group::{
    EffectGroupAdmitSemanticRequest, EffectGroupAdmitSemanticResponse, EffectGroupCloseRequest,
    EffectGroupCloseResponse, EffectGroupCommitChildRequest, EffectGroupCommitChildResponse,
    EffectGroupDispatchClient, EffectGroupDispatchRequest, EffectGroupDrainBlockersRequest,
    EffectGroupDrainBlockersResponse, EffectGroupIndexClient, EffectGroupOpenRequest,
    EffectGroupOpenResponse, EffectGroupPayloadClient, EffectGroupPayloadGetResponse,
    EffectGroupProbeResponse, EffectGroupReadRankRequest, EffectGroupReadRankResponse,
};
use crate::process::{
    LashProcessWorkflowClient, RestateProcessAwaitRequest, RestateProcessCancelRequest,
    RestateProcessWorkflowInput,
};
use crate::process_attach::{LashProcessAttachClient, RestateProcessAttachRequest};

mod wake;
pub(crate) use crate::durable_wait::LASH_REPLAY_KEY_HEADER;
#[cfg(test)]
pub(crate) use wake::guard_restate_context_future;
pub(crate) use wake::{ClosureWakeRelay, guard_restate_run_future, relay_closure_wakes};

/// The future every turn-cancel race returns across this seam.
///
/// `T` is what the guarded wait produces when it wins: `()` for a timer, a
/// `Resolution` for an await-event, the process output for a process await.
/// What a handler's durable-wait resolve answers: the index's outcome, or its
/// typed cancel-decided refusal (ADR 0099 §4, W17).
pub(crate) type ResolveEventFuture<'run> = Pin<
    Box<
        dyn Future<Output = Result<RestateDurableWaitResolveResponse, TerminalError>> + Send + 'run,
    >,
>;

type TurnCancelRaceFuture<'run, T> = Pin<
    Box<dyn Future<Output = Result<RestateTurnCancelRaceOutcome<T>, TerminalError>> + Send + 'run>,
>;

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
async fn race_turn_cancel_gate<'run, 'ctx, C, T>(
    context: &C,
    session_id: &SessionId,
    turn_cancel: RestateDurableWaitAwaitRequest,
    awakeable: impl Fn() -> (String, GateWait<'run, Json<RestateTurnCancelWake>>),
    guarded: impl FnOnce() -> GateWait<'run, T>,
) -> Result<RestateTurnCancelRaceOutcome<T>, TerminalError>
where
    C: ContextClient<'ctx>,
{
    let scope = turn_cancel.key.scope.clone();
    let authority_id = crate::durable_wait::restate_authority_id_for_key(&turn_cancel.key)
        .ok_or_else(|| {
            TerminalError::from_error(crate::durable_wait::restate_unknown_or_revoked())
        })?;
    let (awakeable_id, awakeable_wait) = awakeable();
    let gate = match register_turn_cancel_gate(context, session_id, turn_cancel.key, awakeable_id)
        .await?
    {
        RestateTurnCancelGate::Registered(gate) => gate,
        RestateTurnCancelGate::Revoked => {
            return Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                session_id: SessionId::from(session_id.to_string()),
            });
        }
    };
    let guarded = guarded();
    match first_of_gate_race(&*guarded, &*awakeable_wait).await? {
        GateRaceWinner::Guarded => {
            let value = guarded.await?;
            retire_turn_cancel_gate(context, session_id, gate).await?;
            return Ok(RestateTurnCancelRaceOutcome::Completed(value));
        }
        GateRaceWinner::Gate => {}
    }
    let Json(wake) = awakeable_wait.await?;
    match wake {
        RestateTurnCancelWake::TurnCancelled => {
            return Ok(RestateTurnCancelRaceOutcome::TurnCancelled);
        }
        RestateTurnCancelWake::SessionRevoked => {
            return Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                session_id: SessionId::from(session_id.to_string()),
            });
        }
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
        session_id,
        escalation_key,
        escalation_id,
    )
    .await?
    {
        RestateTurnCancelGate::Registered(gate) => gate,
        RestateTurnCancelGate::Revoked => {
            return Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                session_id: SessionId::from(session_id.to_string()),
            });
        }
    };
    match first_of_gate_race(&*guarded, &*escalation).await? {
        GateRaceWinner::Guarded => {
            // The escalation entry is retired whichever way the guarded wait
            // settles: it only ever exists on the deferred branch, so no
            // journal written before the mode existed can reach this
            // retirement, and a failing guarded wait would otherwise leave the
            // index holding an entry for a wait that is gone. The success path
            // keeps the deployed order — guarded value first, then the
            // retirement — byte for byte.
            let value = guarded.await;
            let retirement = retire_turn_cancel_gate(context, session_id, escalation_gate).await;
            let value = value?;
            retirement?;
            Ok(RestateTurnCancelRaceOutcome::Completed(value))
        }
        GateRaceWinner::Gate => {
            let Json(wake) = escalation.await?;
            Ok(match wake {
                // The escalation promise only ever holds an immediate request;
                // a deferred wake on it would be a weaker request that cannot
                // exist there, and is honoured as the stop it escalates.
                RestateTurnCancelWake::TurnCancelled
                | RestateTurnCancelWake::TurnCancelDeferred => {
                    RestateTurnCancelRaceOutcome::TurnCancelled
                }
                RestateTurnCancelWake::SessionRevoked => {
                    RestateTurnCancelRaceOutcome::SessionRevoked {
                        session_id: SessionId::from(session_id.to_string()),
                    }
                }
            })
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

/// The default every unregistered group-index call shares: a pinned refusal
/// naming the handler, so a wiring miss surfaces as a typed terminal error
/// rather than a silent `Ok`.
fn unregistered_group_index<'run, T>(
    handler: &'static str,
) -> Pin<Box<dyn Future<Output = Result<T, TerminalError>> + Send + 'run>>
where
    T: Send + 'run,
{
    Box::pin(async move { Err(TerminalError::new(format!("{handler} is not registered"))) })
}

pub trait RestateControllerContext<'ctx>: Send + Sync + 'ctx {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    /// A durable timer, raced against the turn's cancellation gate when
    /// `turn_cancel` names one. A sleep that observes no turn races the
    /// process segment's durable cancel promise when `process_cancel` says
    /// the wait belongs to a process drive (FIG-3673); nothing live ever
    /// races it.
    fn sleep_or_turn_cancel<'run>(
        &'run self,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run;

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run;

    /// Runs one journaled `ctx.run` step whose fault is never recorded
    /// (ADR 0105 §1: an engine fault is not a domain outcome).
    ///
    /// An `Ok` value is journaled and replayed like
    /// [`run_json_send`](Self::run_json_send)'s. An `Err` ends this attempt
    /// retryably and writes nothing: the step carries no retry policy of its
    /// own, so the engine's invocation retry replays the journal up to it and
    /// runs it again (FIG-3683). A context that cannot end an attempt — the
    /// recording test contexts — records the fault and surfaces it as a
    /// terminal error instead.
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
        let run = self.run_json_send::<Result<T, String>, _>(effect_name, None, future);
        async move {
            let Json(result) = run.await?;
            result.map(Json).map_err(TerminalError::new)
        }
    }

    /// Submits the process's workflow run.
    ///
    /// The failure is classified because the scheduling boundary compensates on
    /// one class and not the other: see [`ProcessWorkflowStartFailure`].
    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProcessWorkflowStartFailure>> + Send + 'run>>
    where
        'ctx: 'run;

    fn request_process_workflow_cancel<'run>(
        &'run self,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    /// A durable await, raced against the turn's cancellation gate when
    /// `turn_cancel` names one. An await that observes no turn (a process
    /// body's `waitSignal`) races the process segment's durable cancel
    /// promise when `process_cancel` says so (FIG-3673); a lost event wait is
    /// released `Cancelled`.
    fn await_event_or_turn_cancel<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run;

    /// A journaled peek of the running process workflow's own cancellation
    /// promise (FIG-3149, FIG-3673): a process drive's cancel checkpoint and
    /// its post-runner verdict. Every redrive observes exactly the verdict the
    /// first execution committed instead of re-reading live registry state
    /// that can answer differently on replay.
    ///
    /// Contexts without a workflow promise surface carry no process
    /// cancellation and answer `false`.
    fn peek_process_cancel_requested<'run>(
        &'run self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(false) })
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
        replay_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_process_terminal<'run>(
        &'run self,
        process_id: ProcessId,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    /// Race a process terminal wait against durable turn cancellation, or,
    /// for a wait that observes no turn, against the awaiting process
    /// segment's own cancel promise when `process_cancel` says so (FIG-3673).
    fn await_process_terminal_or_turn_cancel<'run>(
        &'run self,
        process_id: ProcessId,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run;

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveEventFuture<'run>
    where
        'ctx: 'run;

    /// Hand one process terminal wait to the attach workflow and return without
    /// waiting for it.
    ///
    /// One-way by construction: the calling handler must go on to park on the
    /// wait's own promise, so the terminal read has to happen in another
    /// invocation's journal, not in this one's.
    fn attach_process_terminal<'run>(
        &'run self,
        request: RestateProcessAttachRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn update_session_waits<'run>(
        &'run self,
        session_id: SessionId,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn session_is_revoked<'run>(
        &'run self,
        _session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(false) })
    }

    /// Record an effect starting under the non-session scope whose index is
    /// `index_key`, answering whether the scope admits it (FIG-2499). A
    /// context without a durable-wait index admits everything and records
    /// nothing.
    fn scope_effect_begin<'run>(
        &'run self,
        _index_key: String,
        _replay_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(true) })
    }

    fn scope_effect_end<'run>(
        &'run self,
        _index_key: String,
        _replay_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(()) })
    }

    /// Record an effect group opened under the non-session scope whose index
    /// is `index_key`, answering whether the scope admits it (FIG-2499).
    fn scope_group_record<'run>(
        &'run self,
        _index_key: String,
        _group_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(true) })
    }

    fn effect_group_probe<'run>(
        &'run self,
        _group_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<EffectGroupProbeResponse, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/probe is not registered",
            ))
        })
    }

    fn effect_group_preflight<'run>(
        &'run self,
        _group_key: String,
        _children: Vec<lash_core::RuntimeEffectEnvelope>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<usize>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupDispatch/preflight is not registered",
            ))
        })
    }

    fn effect_group_open<'run>(
        &'run self,
        _group_key: String,
        _request: EffectGroupOpenRequest,
    ) -> Pin<Box<dyn Future<Output = Result<EffectGroupOpenResponse, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/open is not registered",
            ))
        })
    }

    fn effect_group_submit<'run>(
        &'run self,
        _request: EffectGroupDispatchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupDispatch/run is not registered",
            ))
        })
    }

    fn effect_group_read_rank<'run>(
        &'run self,
        _group_key: String,
        _request: EffectGroupReadRankRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<EffectGroupReadRankResponse, TerminalError>> + Send + 'run>,
    >
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/read_rank is not registered",
            ))
        })
    }

    fn effect_group_payload_get<'run>(
        &'run self,
        _payload_key: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<EffectGroupPayloadGetResponse, TerminalError>> + Send + 'run,
        >,
    >
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupPayload/get is not registered",
            ))
        })
    }

    fn effect_group_close<'run>(
        &'run self,
        _group_key: String,
        _request: EffectGroupCloseRequest,
    ) -> Pin<Box<dyn Future<Output = Result<EffectGroupCloseResponse, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "EffectGroupIndex/close is not registered",
            ))
        })
    }

    /// The group one replay key is a committed member of under the scope
    /// whose index is `index_key`: the membership record a §4 boundary
    /// resolves before it can name its index (FIG-3409).
    fn scope_group_child_membership<'run>(
        &'run self,
        _index_key: String,
        _replay_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        unregistered_group_index("LashDurableWaitIndex/group_child_membership")
    }

    /// The §4 boundary decision for one group child's final record.
    fn effect_group_commit_child<'run>(
        &'run self,
        _group_key: String,
        _request: EffectGroupCommitChildRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<EffectGroupCommitChildResponse, TerminalError>>
                + Send
                + 'run,
        >,
    >
    where
        'ctx: 'run,
    {
        unregistered_group_index("EffectGroupIndex/commit_child")
    }

    /// §4's admission fence for one recorded group child (FIG-3470): the
    /// index's serialized answer to "may a semantic effect minted under this
    /// child still be admitted".
    fn effect_group_admit_semantic<'run>(
        &'run self,
        _group_key: String,
        _request: EffectGroupAdmitSemanticRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<EffectGroupAdmitSemanticResponse, TerminalError>>
                + Send
                + 'run,
        >,
    >
    where
        'ctx: 'run,
    {
        unregistered_group_index("EffectGroupIndex/admit_semantic")
    }

    /// The lower-commit siblings that still owe their settlement seats — the
    /// durable §5 barrier read.
    fn effect_group_drain_blockers<'run>(
        &'run self,
        _group_key: String,
        _commit_seq: u64,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<EffectGroupDrainBlockersResponse, TerminalError>>
                + Send
                + 'run,
        >,
    >
    where
        'ctx: 'run,
    {
        unregistered_group_index("EffectGroupIndex/drain_blockers")
    }

    /// Await an effect group's wait (its readiness or one of its ranks),
    /// raced against the turn's cancellation gate when `turn_cancel` names
    /// one, exactly as a durable wait is (FIG-3672 P9). A wait that observes
    /// no turn (a process body's rank wait) races the process segment's
    /// durable cancel promise when `process_cancel` says so (FIG-3673).
    fn await_effect_group_wait<'run>(
        &'run self,
        _request: RestateDurableWaitAwaitRequest,
        _replay_key: String,
        _turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        _process_cancel: ProcessCancelRace,
    ) -> TurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run,
    {
        Box::pin(async {
            Err(TerminalError::new(
                "LashDurableWaitWorkflow/await_resolution is not registered",
            ))
        })
    }
}

/// Freeze a deadline-bearing durable wait in the invoking handler's journal.
///
/// A process replacement reconstructs Lash's monotonic `Instant` deadline and
/// can observe a different wall clock or setup delay. Restate compares nested
/// call payloads structurally, so the absolute deadline must become a journal
/// fact before the `LashDurableWaitWorkflow/await_resolution` call is emitted.
/// No-deadline waits retain their deployed command shape and emit no extra run.
pub(crate) async fn journaled_restate_durable_wait_request<'ctx, C>(
    context: &C,
    key: &lash_core::AwaitEventKey,
    deadline: Option<std::time::Instant>,
    clock: &dyn lash_core::Clock,
) -> Result<RestateDurableWaitAwaitRequest, TerminalError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let proposed = restate_durable_wait_request(key, deadline, clock);
    let Some(proposed_deadline) = proposed.deadline else {
        return Ok(proposed);
    };
    let Json(deadline): Json<RestateDurableWaitDeadline> = context
        .run_json_send(
            format!("lash:durable-wait-deadline:v2:{}", key.key_id),
            None,
            async move { proposed_deadline },
        )
        .await?;
    Ok(RestateDurableWaitAwaitRequest {
        key: key.clone(),
        deadline: Some(deadline),
    })
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

/// The segment's cancel promise as a durable future, on a context that has a
/// workflow promise surface; `None` on every other context.
macro_rules! process_cancel_promise {
    (promises, $context:ident, $run:lifetime, $ctx:expr) => {{
        // Workflow contexts are covariant in their lifetime: shortening it to
        // the borrow lets the SDK's `promise` borrow the context for exactly
        // as long as the race holds the future.
        let context: &$run $context<$run> = $ctx;
        Some(erase_gate_wait(
            restate_sdk::context::ContextPromises::promise::<String>(
                context,
                crate::process::PROCESS_CANCEL_PROMISE_KEY,
            ),
        ))
    }};
    (no_promises, $context:ident, $run:lifetime, $ctx:expr) => {{
        let _ = $ctx;
        None::<GateWait<$run, String>>
    }};
}

macro_rules! impl_restate_controller_context {
    ($($context:ident : $promises:ident),+ $(,)?) => {
        $(
            impl<'ctx> RestateControllerContext<'ctx> for $context<'ctx> {
                fn sleep_send<'run>(
                    &'run self,
                    duration: Duration,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        restate_sdk::context::ContextTimers::sleep(self, duration).await
                    })
                }

                fn sleep_or_turn_cancel<'run>(
                    &'run self,
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

                fn run_json_send<'run, T, Fut>(
                    &'run self,
                    effect_name: String,
                    retry_policy: Option<RunRetryPolicy>,
                    future: Fut,
                ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                    T: Serialize + DeserializeOwned + Send + 'static,
                    Fut: Future<Output = T> + Send + 'run,
                {
                    Box::pin(async move {
                        // Wakes from the closure's own future are relayed straight
                        // to the guard's parent waker, so they never reach the
                        // guard's tracker and only the SDK's terminal park can
                        // fuse the run - on a live attempt and on a replay that
                        // never invokes the closure at all.
                        let closure_relay = Arc::new(ClosureWakeRelay::default());
                        let relay = Arc::clone(&closure_relay);
                        let run = restate_sdk::context::ContextSideEffects::run(self, move || async move {
                            let value = relay_closure_wakes(future, relay).await;
                            Ok::<Json<T>, HandlerError>(Json(value))
                        });
                        let run = restate_sdk::context::RunFuture::name(run, effect_name);
                        let run = match retry_policy {
                            Some(policy) => restate_sdk::context::RunFuture::retry_policy(run, policy),
                            None => run,
                        };
                        // An SDK-level run failure is terminal for this attempt:
                        // the SDK records the handler state, wakes
                        // synchronously and returns `Pending` so its outer
                        // `HandlerStateAwareFuture` can consume that state. The
                        // already-resolved run future must never be re-entered,
                        // and pollers above this seam (the turn observation publisher, the
                        // effect races) can poll their enclosing future again,
                        // so fuse it here rather than trusting every caller.
                        guard_restate_run_future(run, closure_relay).await
                    })
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
                    async move {
                        let closure_relay = Arc::new(ClosureWakeRelay::default());
                        let relay = Arc::clone(&closure_relay);
                        let run = restate_sdk::context::ContextSideEffects::run(self, move || async move {
                            // A retryable closure failure under the SDK's
                            // default (infinite) policy fails the attempt
                            // without journaling a completion; the invocation
                            // retry runs the step again.
                            relay_closure_wakes(future, relay)
                                .await
                                .map(Json)
                                .map_err(|fault| HandlerError::from(std::io::Error::other(fault)))
                        });
                        let run = restate_sdk::context::RunFuture::name(run, effect_name);
                        guard_restate_run_future(run, closure_relay).await
                    }
                }

                fn start_process_workflow<'run>(
                    &'run self,
                    registration: ProcessRegistration,
                    execution_context: ProcessExecutionContext,
                ) -> Pin<
                    Box<
                        dyn Future<Output = Result<String, ProcessWorkflowStartFailure>>
                            + Send
                            + 'run,
                    >,
                >
                where
                    'ctx: 'run,
                {
                    let workflow_key = registration.id.clone();
                    let request = self
                        .workflow_client::<LashProcessWorkflowClient>(workflow_key.clone())
                        .run(Json(RestateProcessWorkflowInput {
                            registration,
                            execution_context,
                            segment_ordinal: 0,
                            journal_version: crate::process::RESTATE_PROCESS_JOURNAL_VERSION,
                        }));
                    let handle = request.send();
                    Box::pin(async move {
                        // A journaled send that completes with a terminal
                        // failure is proof of non-acceptance: the runtime
                        // records the send as an entry, and a transient
                        // condition (no connection, no reply yet) suspends and
                        // retries the handler instead of completing the entry.
                        // Anything that reaches here therefore names a decision,
                        // not a silence.
                        let handle = handle.await.map_err(ProcessWorkflowStartFailure::Rejected)?;
                        Ok(handle.invocation_id().to_owned())
                    })
                }

                fn request_process_workflow_cancel<'run>(
                    &'run self,
                    request: RestateProcessCancelRequest,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let workflow_key = request.process_ref.process_id.clone();
                    let request = self
                        .workflow_client::<LashProcessWorkflowClient>(workflow_key.clone())
                        .cancel(Json(request));
                    let call = request.call();
                    Box::pin(async move {
                        let Json(()) = call.await?;
                        Ok(())
                    })
                }

                fn await_event<'run>(
                    &'run self,
                    request: RestateDurableWaitAwaitRequest,
                    replay_key: String,
                    _cancellation: tokio_util::sync::CancellationToken,
                ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let address = RestateDurableWaitAddress::for_key(&request.key);
                        let start = self
                            .workflow_client::<LashDurableWaitWorkflowClient>(
                                address.workflow_key.clone(),
                            )
                            .await_resolution(Json(request.clone().into()))
                            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key.clone());
                        let call = start.call();
                        restate_sdk::select! {
                            result = call => {
                                let Json(resolution) = result?;
                                Ok(resolution)
                            },
                            on_cancel => {
                                let resolve_request = self
                                    .object_client::<LashDurableWaitIndexClient>(
                                        durable_wait_index_object_key(&address),
                                    )
                                    .resolve(Json(RestateDurableWaitResolveRequest {
                                        key: request.key,
                                        resolution: Resolution::Cancelled,
                                    }))
                                    .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                                let Json(response) = resolve_request.call().await?;
                                // A cancel-decided child's key refuses the
                                // release (ADR 0099 §4): the waiter was
                                // cancelled either way.
                                Ok(match response {
                                    RestateDurableWaitResolveResponse::Outcome(ResolveOutcome::AlreadyResolved {
                                        terminal,
                                    }) => terminal,
                                    RestateDurableWaitResolveResponse::Outcome(_)
                                    | RestateDurableWaitResolveResponse::Refused(RestateDurableWaitResolveRefusal::CancelDecided) => {
                                        Resolution::Cancelled
                                    }
                                })
                            }
                        }
                    })
                }

                fn await_event_or_turn_cancel<'run>(
                    &'run self,
                    request: RestateDurableWaitAwaitRequest,
                    replay_key: String,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    process_cancel: ProcessCancelRace,
                ) -> TurnCancelRaceFuture<'run, Resolution>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            if process_cancel == ProcessCancelRace::NotRaced {
                                return self
                                    .await_event(
                                        request,
                                        replay_key,
                                        tokio_util::sync::CancellationToken::new(),
                                    )
                                    .await
                                    .map(RestateTurnCancelRaceOutcome::Completed);
                            };
                            // The event wait's CallCommand, then the promise's.
                            let event_address = RestateDurableWaitAddress::for_key(&request.key);
                            let event_key = request.key.clone();
                            let event = self
                                .workflow_client::<LashDurableWaitWorkflowClient>(
                                    event_address.workflow_key.clone(),
                                )
                                .await_resolution(Json(request.into()))
                                .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key.clone());
                            let event = erase_gate_wait(event.call());
                            let Some(promise) =
                                process_cancel_promise!($promises, $context, 'run, self)
                            else {
                                return Err(TerminalError::new(
                                    "a process cancel race needs a workflow promise surface",
                                ));
                            };
                            return match race_process_cancel(promise, event).await? {
                                RestateTurnCancelRaceOutcome::ProcessCancelled => {
                                    // Release the losing event wait, as a
                                    // turn-gate loser is released: nobody is
                                    // left to resolve it.
                                    let resolve = self
                                        .object_client::<LashDurableWaitIndexClient>(
                                            durable_wait_index_object_key(&event_address),
                                        )
                                        .resolve(Json(RestateDurableWaitResolveRequest {
                                            key: event_key,
                                            resolution: Resolution::Cancelled,
                                        }))
                                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                                    let Json(_) = resolve.call().await?;
                                    Ok(RestateTurnCancelRaceOutcome::ProcessCancelled)
                                }
                                outcome => Ok(outcome.map(|Json(resolution)| resolution)),
                            };
                        };

                        let Some(session_id) = turn_cancel.key.scope.session_id().cloned()
                        else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        let event_address = RestateDurableWaitAddress::for_key(&request.key);
                        let event_key = request.key.clone();
                        // Same journal geometry as process await: the guarded
                        // wait's CallCommand is emitted first, then the gate's
                        // awakeable, then the registration.
                        let event = self
                            .workflow_client::<LashDurableWaitWorkflowClient>(
                                event_address.workflow_key.clone(),
                            )
                            .await_resolution(Json(request.into()))
                            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key.clone());
                        let event = erase_gate_wait(event.call());
                        match race_turn_cancel_gate(
                            self,
                            &SessionId::from(session_id),
                            turn_cancel,
                            || gate_awakeable(self),
                            move || event,
                        )
                        .await?
                        {
                            RestateTurnCancelRaceOutcome::Completed(Json(resolution)) => {
                                Ok(RestateTurnCancelRaceOutcome::Completed(resolution))
                            }
                            RestateTurnCancelRaceOutcome::TurnCancelled => {
                                // Release the losing event wait. The retired
                                // nested workflow did this from its own journal;
                                // on the gate it is the waiter's job, or the
                                // event workflow stays parked with nobody left
                                // to resolve it.
                                let resolve = self
                                    .object_client::<LashDurableWaitIndexClient>(
                                        durable_wait_index_object_key(&event_address),
                                    )
                                    .resolve(Json(RestateDurableWaitResolveRequest {
                                        key: event_key,
                                        resolution: Resolution::Cancelled,
                                    }))
                                    .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                                let Json(_) = resolve.call().await?;
                                Ok(RestateTurnCancelRaceOutcome::TurnCancelled)
                            }
                            outcome @ (RestateTurnCancelRaceOutcome::SessionRevoked { .. }
                            | RestateTurnCancelRaceOutcome::ProcessCancelled) => {
                                Ok(outcome.map(|Json(resolution)| resolution))
                            }
                        }
                    })
                }

                fn peek_event<'run>(
                    &'run self,
                    address: RestateDurableWaitAddress,
                    replay_key: String,
                ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request = self
                        .workflow_client::<LashDurableWaitWorkflowClient>(address.workflow_key)
                        .peek()
                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                    Box::pin(async move {
                        let Json(resolution) = request.call().await?;
                        Ok(resolution)
                    })
                }

                fn attach_process_terminal<'run>(
                    &'run self,
                    request: RestateProcessAttachRequest,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let send = self
                        .workflow_client::<LashProcessAttachClient>(
                            crate::process_attach::process_attach_workflow_key(&request.key),
                        )
                        .run(Json(request))
                        .send();
                    Box::pin(async move {
                        send.await?;
                        Ok(())
                    })
                }

                fn await_process_terminal<'run>(
                    &'run self,
                    process_id: ProcessId,
                ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request = self
                        .workflow_client::<LashProcessWorkflowClient>(process_id.clone())
                        .await_terminal(Json(RestateProcessAwaitRequest { process_id }));
                    let call = request.call();
                    Box::pin(async move {
                        let Json(output) = call.await?;
                        Ok(output)
                    })
                }

                fn await_process_terminal_or_turn_cancel<'run>(
                    &'run self,
                    process_id: ProcessId,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    process_cancel: ProcessCancelRace,
                ) -> TurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            let terminal = self
                                .workflow_client::<LashProcessWorkflowClient>(process_id.clone())
                                .await_terminal(Json(RestateProcessAwaitRequest { process_id }));
                            let terminal = terminal.call();
                            let promise = match process_cancel {
                                ProcessCancelRace::Raced => {
                                    process_cancel_promise!($promises, $context, 'run, self)
                                }
                                ProcessCancelRace::NotRaced => None,
                            };
                            let Some(promise) = promise else {
                                let Json(output) = terminal.await?;
                                return Ok(RestateTurnCancelRaceOutcome::Completed(Box::new(output)));
                            };
                            // The terminal call's command, then the promise's.
                            // A lost call is cancelled rather than left
                            // parked on the child until the child ends.
                            if let GateRaceWinner::Gate = first_of_gate_race(&terminal, &*promise).await?
                                && crate::process::process_cancel_promise_verdict(Some(promise.await?))
                            {
                                restate_sdk::context::CallFuture::invocation_handle(&terminal)
                                    .await?
                                    .cancel();
                                return Ok(RestateTurnCancelRaceOutcome::ProcessCancelled);
                            }
                            let Json(output) = terminal.await?;
                            return Ok(RestateTurnCancelRaceOutcome::Completed(Box::new(output)));
                        };
                        let Some(session_id) = turn_cancel.key.scope.session_id().cloned()
                        else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        // `Request::call()` emits its CallCommand synchronously in
                        // Restate SDK 0.10. Construct this call first so a suspended
                        // pre-FIG-790 journal remains the exact prefix of every
                        // redrive after the cancellation adjudicator was added.
                        let process = self
                            .workflow_client::<LashProcessWorkflowClient>(process_id.clone())
                            .await_terminal(Json(RestateProcessAwaitRequest {
                                process_id: process_id.clone(),
                            }));
                        let process = erase_gate_wait(process.call());
                        let outcome = race_turn_cancel_gate(
                            self,
                            &SessionId::from(session_id),
                            turn_cancel,
                            || gate_awakeable(self),
                            move || process,
                        )
                        .await?;
                        let winning_branch = match &outcome {
                            RestateTurnCancelRaceOutcome::Completed(_) => "process_terminal",
                            RestateTurnCancelRaceOutcome::TurnCancelled => "turn_cancelled",
                            RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
                                "session_revoked"
                            }
                            RestateTurnCancelRaceOutcome::ProcessCancelled => "process_cancelled",
                        };
                        tracing::info!(
                            target: "lash::restate",
                            event = "restate.process_await_adjudicated",
                            process_id = %process_id,
                            winning_branch,
                            "Restate process-await adjudication"
                        );
                        Ok(outcome.map(|Json(output)| Box::new(output)))
                    })
                }

                fn resolve_event<'run>(
                    &'run self,
                    request: RestateDurableWaitResolveRequest,
                ) -> ResolveEventFuture<'run>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let replay_key = request.key.key_id.clone();
                        let address = RestateDurableWaitAddress::for_key(&request.key);
                        let resolve = self
                            .object_client::<LashDurableWaitIndexClient>(
                                durable_wait_index_object_key(&address),
                            )
                            .resolve(Json(request))
                            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                        let Json(outcome) = resolve.call().await?;
                        Ok(outcome)
                    })
                }

                fn update_session_waits<'run>(
                    &'run self,
                    session_id: SessionId,
                    revoke: bool,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let client = self.object_client::<LashDurableWaitIndexClient>(session_id);
                    let request = if revoke {
                        client.revoke_all()
                    } else {
                        client.cancel_all()
                    };
                    let call = request.call();
                    Box::pin(async move {
                        let Json(()) = call.await?;
                        Ok(())
                    })
                }

                fn session_is_revoked<'run>(
                    &'run self,
                    session_id: SessionId,
                ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request = self
                        .object_client::<LashDurableWaitIndexClient>(session_id)
                        .is_revoked(Json(()));
                    let call = request.call();
                    Box::pin(async move {
                        let Json(revoked) = call.await?;
                        Ok(revoked)
                    })
                }
                fn scope_effect_begin<'run>(
                    &'run self,
                    index_key: String,
                    replay_key: String,
                ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<LashDurableWaitIndexClient>(index_key)
                        .begin_effect(Json(RestateDurableWaitEffectRequest {
                            replay_key: replay_key.clone(),
                        }))
                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                        .call();
                    Box::pin(async move {
                        let Json(admitted) = call.await?;
                        Ok(admitted)
                    })
                }
                fn scope_effect_end<'run>(
                    &'run self,
                    index_key: String,
                    replay_key: String,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<LashDurableWaitIndexClient>(index_key)
                        .end_effect(Json(RestateDurableWaitEffectRequest {
                            replay_key: replay_key.clone(),
                        }))
                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                        .call();
                    Box::pin(async move {
                        let Json(()) = call.await?;
                        Ok(())
                    })
                }
                fn scope_group_record<'run>(
                    &'run self,
                    index_key: String,
                    group_key: String,
                ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<LashDurableWaitIndexClient>(index_key)
                        .record_group(Json(RestateDurableWaitGroupRequest { group_key }))
                        .call();
                    Box::pin(async move {
                        let Json(admitted) = call.await?;
                        Ok(admitted)
                    })
                }

                fn effect_group_probe<'run>(
                    &'run self,
                    group_key: String,
                ) -> Pin<Box<dyn Future<Output = Result<EffectGroupProbeResponse, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupIndexClient>(group_key)
                        .probe()
                        .call();
                    Box::pin(async move {
                        let Json(response) = call.await?;
                        Ok(response)
                    })
                }

                fn effect_group_preflight<'run>(
                    &'run self,
                    group_key: String,
                    children: Vec<lash_core::RuntimeEffectEnvelope>,
                ) -> Pin<Box<dyn Future<Output = Result<Option<usize>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .workflow_client::<EffectGroupDispatchClient>(group_key)
                        .preflight(Json(children))
                        .call();
                    Box::pin(async move {
                        let Json(response) = call.await?;
                        Ok(response)
                    })
                }

                fn effect_group_open<'run>(
                    &'run self,
                    group_key: String,
                    request: EffectGroupOpenRequest,
                ) -> Pin<Box<dyn Future<Output = Result<EffectGroupOpenResponse, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupIndexClient>(group_key)
                        .open(Json(request))
                        .call();
                    Box::pin(async move {
                        let Json(response) = call.await?;
                        Ok(response)
                    })
                }

                fn effect_group_submit<'run>(
                    &'run self,
                    request: EffectGroupDispatchRequest,
                ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let handle = self
                        .workflow_client::<EffectGroupDispatchClient>(request.group_key.clone())
                        .run(Json(request))
                        .send();
                    Box::pin(async move {
                        let handle = handle.await?;
                        Ok(handle.invocation_id().to_owned())
                    })
                }

                fn effect_group_read_rank<'run>(
                    &'run self,
                    group_key: String,
                    request: EffectGroupReadRankRequest,
                ) -> Pin<Box<dyn Future<Output = Result<EffectGroupReadRankResponse, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupIndexClient>(group_key)
                        .read_rank(Json(request))
                        .call();
                    Box::pin(async move {
                        let Json(response) = call.await?;
                        Ok(response)
                    })
                }

                fn effect_group_payload_get<'run>(
                    &'run self,
                    payload_key: String,
                ) -> Pin<Box<dyn Future<Output = Result<EffectGroupPayloadGetResponse, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupPayloadClient>(payload_key)
                        .get()
                        .call();
                    Box::pin(async move {
                        let Json(response) = call.await?;
                        Ok(response)
                    })
                }

                fn effect_group_close<'run>(
                    &'run self,
                    group_key: String,
                    request: EffectGroupCloseRequest,
                ) -> Pin<Box<dyn Future<Output = Result<EffectGroupCloseResponse, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupIndexClient>(group_key)
                        .close(Json(request))
                        .call();
                    Box::pin(async move {
                        let Json(response) = call.await?;
                        Ok(response)
                    })
                }
                fn scope_group_child_membership<'run>(
                    &'run self,
                    index_key: String,
                    replay_key: String,
                ) -> Pin<Box<dyn Future<Output = Result<Option<String>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<LashDurableWaitIndexClient>(index_key)
                        .group_child_membership(Json(
                            RestateDurableWaitGroupChildMembershipRequest {
                                replay_key: replay_key.clone(),
                            },
                        ))
                        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                        .call();
                    Box::pin(async move { call.await.map(|Json(group_key)| group_key) })
                }

                fn effect_group_commit_child<'run>(
                    &'run self,
                    group_key: String,
                    request: EffectGroupCommitChildRequest,
                ) -> Pin<
                    Box<
                        dyn Future<Output = Result<EffectGroupCommitChildResponse, TerminalError>>
                            + Send
                            + 'run,
                    >,
                >
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupIndexClient>(group_key)
                        .commit_child(Json(request))
                        .call();
                    Box::pin(async move { call.await.map(|Json(response)| response) })
                }

                fn effect_group_admit_semantic<'run>(
                    &'run self,
                    group_key: String,
                    request: EffectGroupAdmitSemanticRequest,
                ) -> Pin<
                    Box<
                        dyn Future<
                                Output = Result<EffectGroupAdmitSemanticResponse, TerminalError>,
                            > + Send
                            + 'run,
                    >,
                >
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupIndexClient>(group_key)
                        .admit_semantic(Json(request))
                        .call();
                    Box::pin(async move { call.await.map(|Json(response)| response) })
                }

                fn effect_group_drain_blockers<'run>(
                    &'run self,
                    group_key: String,
                    commit_seq: u64,
                ) -> Pin<Box<dyn Future<Output = Result<EffectGroupDrainBlockersResponse, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let call = self
                        .object_client::<EffectGroupIndexClient>(group_key)
                        .drain_blockers(Json(EffectGroupDrainBlockersRequest { commit_seq }))
                        .call();
                    Box::pin(async move { call.await.map(|Json(response)| response) })
                }

                fn await_effect_group_wait<'run>(
                    &'run self,
                    request: RestateDurableWaitAwaitRequest,
                    replay_key: String,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    process_cancel: ProcessCancelRace,
                ) -> TurnCancelRaceFuture<'run, Resolution>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let address = RestateDurableWaitAddress::for_key(&request.key);
                        let call = self
                            .workflow_client::<LashDurableWaitWorkflowClient>(address.workflow_key)
                            .await_resolution(Json(request.into()))
                            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                        let Some(turn_cancel) = turn_cancel else {
                            // The rank wait's CallCommand, then the promise's.
                            let wait = erase_gate_wait(call.call());
                            let promise = match process_cancel {
                                ProcessCancelRace::Raced => {
                                    process_cancel_promise!($promises, $context, 'run, self)
                                }
                                ProcessCancelRace::NotRaced => None,
                            };
                            let outcome = match promise {
                                Some(promise) => race_process_cancel(promise, wait).await?,
                                None => RestateTurnCancelRaceOutcome::Completed(wait.await?),
                            };
                            return Ok(outcome.map(|Json(resolution)| resolution));
                        };
                        let Some(session_id) = turn_cancel.key.scope.session_id().cloned()
                        else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        // The rank wait's CallCommand, then the gate's
                        // awakeable, then its registration: the geometry every
                        // guarded durable wait already has.
                        let wait = erase_gate_wait(call.call());
                        Ok(race_turn_cancel_gate(
                            self,
                            &SessionId::from(session_id),
                            turn_cancel,
                            || gate_awakeable(self),
                            move || wait,
                        )
                        .await?
                        .map(|Json(resolution)| resolution))
                    })
                }

                fn peek_process_cancel_requested<'run>(
                    &'run self,
                ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
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
