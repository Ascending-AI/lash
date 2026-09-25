//! The engine contract admits both a `!Send` engine and a `Send` one, and the
//! race-and-escalation shape of ADR 0105 §3 runs on it as written.
//!
//! `LocalTestCx` holds its state in `Rc<RefCell<_>>`, so nothing in the
//! contract may require an engine or its ops to be `Send`. `SendTestCx` holds
//! the same state in `Arc<Mutex<_>>`, and a drive-side function generic over
//! the context is then `Send` with no bound of its own.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_util::future::FusedFuture;

use super::*;
use crate::runtime::effect::{
    EffectGroupChildCommitOutcome, GroupChildFinalCommit, RankedGroupSettlement,
};
use crate::{
    AwaitEventKey, EffectGroupHandle, ExecutionScope, GroupSettlement, LoserPolicy, Resolution,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeErrorCode,
    TurnCancellationEvidence,
};

/// The scripted answers of a test engine, and what it was asked to do.
#[derive(Default)]
struct Script {
    /// Each step's answer, ready after this many pending polls.
    steps: VecDeque<(usize, EffectResult)>,
    /// Each turn gate's signal; `None` never fires.
    turn_gates: VecDeque<Option<TurnCancelSignal>>,
    escalations: VecDeque<Option<TurnCancelSignal>>,
    log: Vec<String>,
}

fn ready_after<T>(
    polls: usize,
    value: Result<T, EngineFault>,
) -> impl Future<Output = Result<T, EngineFault>> {
    let mut remaining = polls;
    let mut value = Some(value);
    std::future::poll_fn(move |cx| {
        if remaining == 0 {
            return Poll::Ready(value.take().expect("a scripted op resolves once"));
        }
        remaining -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
}

fn unscripted<T>(op: &str) -> Result<T, EngineFault> {
    Err(EngineFault::Terminal(EngineTerminal {
        message: format!("{op} is not scripted"),
    }))
}

/// Declares a test engine whose op boxes a future with the given auto-trait
/// bounds, over state shared through `$shared` and locked by `$lock`.
macro_rules! test_engine {
    ($cx:ident, $op:ident, $shared:ty, $new:expr, |$s:ident| $lock:expr, [$($bound:tt)*]) => {
        struct $op<'a, T> {
            inner: Pin<Box<dyn Future<Output = Result<T, EngineFault>> $($bound)* + 'a>>,
            done: bool,
        }

        impl<'a, T> $op<'a, T> {
            fn of(fut: impl Future<Output = Result<T, EngineFault>> $($bound)* + 'a) -> Self {
                Self { inner: Box::pin(fut), done: false }
            }

            fn now(value: Result<T, EngineFault>) -> Self
            where
                T: 'a $($bound)*,
            {
                Self::of(std::future::ready(value))
            }

            fn never() -> Self
            where
                T: 'a,
            {
                Self::of(std::future::pending())
            }
        }

        impl<T> Future for $op<'_, T> {
            type Output = Result<T, EngineFault>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let polled = self.inner.as_mut().poll(cx);
                if polled.is_ready() {
                    self.done = true;
                }
                polled
            }
        }

        impl<T> FusedFuture for $op<'_, T> {
            fn is_terminated(&self) -> bool {
                self.done
            }
        }

        struct $cx {
            script: $shared,
        }

        impl $cx {
            fn new(script: Script) -> Self {
                Self { script: ($new)(script) }
            }

            fn with<R>(&self, f: impl FnOnce(&mut Script) -> R) -> R {
                let $s = &self.script;
                let mut guard = $lock;
                f(&mut guard)
            }

            fn log(&self) -> Vec<String> {
                self.with(|script| script.log.clone())
            }
        }

        impl EngineContext for $cx {
            type Op<'a, T: 'a> = $op<'a, T>;

            fn now_ms(&self) -> Self::Op<'_, EpochMs> {
                $op::now(Ok(EpochMs(0)))
            }

            fn observe(&self, observation: DriveObservation) {
                self.with(|script| script.log.push(format!("observe:{}", observation.key)));
            }

            fn race<'r, 'a: 'r, 'b: 'r, A: 'a, B: 'b>(
                &'r self,
                mut first: Pin<&'r mut Self::Op<'a, A>>,
                mut second: Pin<&'r mut Self::Op<'b, B>>,
            ) -> Self::Op<'r, Winner<A, B>>
            where
                Self: 'a + 'b,
            {
                $op::of(std::future::poll_fn(move |cx| {
                    // Declaration order: `first` wins a tie at the same poll.
                    if !first.is_terminated()
                        && let Poll::Ready(result) = first.as_mut().poll(cx)
                    {
                        return Poll::Ready(result.map(Winner::First));
                    }
                    if !second.is_terminated()
                        && let Poll::Ready(result) = second.as_mut().poll(cx)
                    {
                        return Poll::Ready(result.map(Winner::Second));
                    }
                    Poll::Pending
                }))
            }

            fn dispose<'a, T: 'a>(
                &self,
                op: Self::Op<'a, T>,
                how: Disposition,
            ) -> Self::Op<'_, Disposed>
            where
                Self: 'a,
            {
                let disposed = if op.is_terminated() {
                    Disposed::AlreadyCompleted
                } else {
                    match how {
                        Disposition::Abandon => Disposed::Abandoned,
                        Disposition::RequestCancel => Disposed::CancelRequested,
                        Disposition::AwaitCancelled => Disposed::Cancelled,
                    }
                };
                self.with(|script| script.log.push(format!("dispose:{disposed:?}")));
                $op::now(Ok(disposed))
            }
        }

        impl DriveAdmission for $cx {
            fn admit(&self, _req: &AdmitRequest) -> Self::Op<'_, AdmitVerdict> {
                $op::now(unscripted("admit"))
            }

            fn seal(&self, _admitted: Admitted) -> Self::Op<'_, SealVerdict> {
                $op::now(unscripted("seal"))
            }

            fn inherit(&self, _authority: &InheritedAuthority) -> Self::Op<'_, InheritVerdict> {
                $op::now(unscripted("inherit"))
            }
        }

        impl DriveContext for $cx {
            fn handover_suggested(&self) -> bool {
                false
            }

            fn continue_as_new(&self, _handover: DriveHandover) -> Self::Op<'_, Never> {
                $op::never()
            }

            fn step(&self, _f: &DriveFence, cmd: EffectCommand) -> Self::Op<'_, EffectResult> {
                let (polls, result) = self.with(|script| {
                    script.log.push(format!("step:{}", cmd.invocation.replay_key()));
                    script.steps.pop_front().expect("a scripted step")
                });
                $op::of(ready_after(polls, Ok(result)))
            }

            fn timer(&self, _f: &DriveFence, _at: EpochMs, _key: &ReplayKey) -> Self::Op<'_, ()> {
                $op::now(unscripted("timer"))
            }

            fn await_key(&self, _f: &DriveFence, _key: &AwaitEventKey) -> Self::Op<'_, Resolution> {
                $op::now(unscripted("await_key"))
            }

            fn peek_key(
                &self,
                _f: &DriveFence,
                _key: &AwaitEventKey,
            ) -> Self::Op<'_, Option<Resolution>> {
                $op::now(unscripted("peek_key"))
            }

            fn resolve_key(
                &self,
                _f: &DriveFence,
                _key: &AwaitEventKey,
                _resolution: Resolution,
            ) -> Self::Op<'_, ResolveAck> {
                $op::now(unscripted("resolve_key"))
            }

            fn turn_cancel(
                &self,
                _f: &DriveFence,
                _scope: &ExecutionScope,
                gate: CancelGate,
            ) -> Self::Op<'_, TurnCancelSignal> {
                let signal = self.with(|script| match gate {
                    CancelGate::Turn => script.turn_gates.pop_front(),
                    CancelGate::Escalation => script.escalations.pop_front(),
                });
                match signal.expect("a scripted gate") {
                    Some(signal) => $op::now(Ok(signal)),
                    None => $op::never(),
                }
            }

            fn child(&self, _f: &DriveFence, _start: ChildStart) -> Self::Op<'_, ChildOutcome> {
                $op::now(unscripted("child"))
            }
        }

        impl DriveGroups for $cx {
            fn open_group(
                &self,
                _f: &DriveFence,
                _group: RuntimeEffectGroup,
            ) -> Self::Op<'_, EffectGroupHandle> {
                $op::now(unscripted("open_group"))
            }

            fn next_settlement<'h>(
                &'h self,
                _h: &'h mut EffectGroupHandle,
            ) -> Self::Op<'h, GroupSettlement> {
                $op::now(unscripted("next_settlement"))
            }

            fn read_settlement(
                &self,
                _group: &GroupKey,
                _rank: u64,
            ) -> Self::Op<'_, Option<RankedGroupSettlement>> {
                $op::now(unscripted("read_settlement"))
            }

            fn settled_count(&self, _group: &GroupKey) -> Self::Op<'_, u64> {
                $op::now(unscripted("settled_count"))
            }

            fn commit_child_final(
                &self,
                _a: &InheritedAuthority,
                _c: GroupChildFinalCommit,
            ) -> Self::Op<'_, EffectGroupChildCommitOutcome> {
                $op::now(unscripted("commit_child_final"))
            }

            fn await_drain_admission(&self, _group: &GroupKey, _commit_seq: u64) -> Self::Op<'_, ()> {
                $op::now(unscripted("await_drain_admission"))
            }

            fn close_group(&self, _h: EffectGroupHandle, _d: LoserPolicy) -> Self::Op<'_, GroupClosed> {
                $op::now(unscripted("close_group"))
            }
        }
    };
}

test_engine!(
    LocalTestCx,
    LocalOp,
    Rc<RefCell<Script>>,
    |script| Rc::new(RefCell::new(script)),
    |shared| shared.borrow_mut(),
    []
);

test_engine!(
    SendTestCx,
    SendOp,
    Arc<Mutex<Script>>,
    |script| Arc::new(Mutex::new(script)),
    |shared| shared.lock().expect("script lock"),
    [+ Send]
);

/// How [`step_under_cancel`] settled.
#[derive(Debug, PartialEq, Eq)]
enum Settled {
    Stepped {
        code: RuntimeErrorCode,
    },
    StopAtBoundary {
        code: RuntimeErrorCode,
        evidence: TurnCancellationEvidence,
    },
    Cancelled(TurnCancellationEvidence),
    Revoked,
}

fn settled_code(result: EffectResult) -> RuntimeErrorCode {
    result
        .expect_err("scripted steps answer with a domain error")
        .code
}

/// ADR 0105 §3's race-and-escalation shape, written once and generic over
/// the engine, exactly as drive code will be.
async fn step_under_cancel<C: DriveContext>(
    f: &Fenced<'_, C>,
    cmd: EffectCommand,
    scope: &ExecutionScope,
) -> Result<Settled, EngineFault> {
    let cx = f.context();
    let mut step = f.step(cmd);
    let mut gate = f.turn_cancel(scope, CancelGate::Turn);
    let first = cx.race(Pin::new(&mut step), Pin::new(&mut gate)).await?;
    Ok(match first {
        Winner::First(out) => {
            cx.dispose(gate, Disposition::Abandon).await?;
            Settled::Stepped {
                code: settled_code(out),
            }
        }
        Winner::Second(TurnCancelSignal::Immediate(evidence)) => {
            cx.dispose(step, Disposition::RequestCancel).await?;
            Settled::Cancelled(evidence)
        }
        Winner::Second(TurnCancelSignal::AfterStep(evidence)) => {
            // A fresh gate over the same scope; the step is the loser the
            // first race kept, still pending and still durable.
            let mut escalation = f.turn_cancel(scope, CancelGate::Escalation);
            let second = cx
                .race(Pin::new(&mut step), Pin::new(&mut escalation))
                .await?;
            match second {
                Winner::First(out) => {
                    cx.dispose(escalation, Disposition::Abandon).await?;
                    Settled::StopAtBoundary {
                        code: settled_code(out),
                        evidence,
                    }
                }
                Winner::Second(_) => {
                    cx.dispose(step, Disposition::RequestCancel).await?;
                    Settled::Cancelled(evidence)
                }
            }
        }
        Winner::Second(TurnCancelSignal::SessionRevoked) => {
            cx.dispose(step, Disposition::RequestCancel).await?;
            Settled::Revoked
        }
    })
}

/// Polls a future to completion with no runtime, as a single-threaded
/// workflow executor would.
fn run<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1_000 {
        if let Poll::Ready(output) = fut.as_mut().poll(&mut cx) {
            return output;
        }
    }
    panic!("the drive did not settle within 1000 polls");
}

fn fence() -> DriveFence {
    serde_json::from_value(serde_json::json!({
        "session": "session-1",
        "epoch": 7,
        "admission": "admission-1",
    }))
    .expect("a recorded fence decodes")
}

fn sealed() -> FenceSource {
    FenceSource::Sealed(SealVerdict::Sealed(fence()))
}

fn scope() -> ExecutionScope {
    ExecutionScope::runtime_operation("turn-scope")
}

fn command(replay_key: &str) -> EffectCommand {
    RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope(), replay_key).expect("effect address"),
            crate::RuntimeAttribution::none(),
            "engine-contract-test",
        ),
        crate::RuntimeEffectCommand::Sleep {
            spec: crate::SleepSpec::For { duration_ms: 1 },
        },
    )
}

fn step_answer() -> EffectResult {
    Err(RuntimeEffectControllerError::new(
        RuntimeErrorCode::StoreCommitFailed,
        "scripted step answer",
    ))
}

fn evidence() -> TurnCancellationEvidence {
    TurnCancellationEvidence::internal("engine-contract-test")
}

fn script(
    step_polls: usize,
    turn_gate: Option<TurnCancelSignal>,
    escalation: Option<Option<TurnCancelSignal>>,
) -> Script {
    Script {
        steps: VecDeque::from([(step_polls, step_answer())]),
        turn_gates: VecDeque::from([turn_gate]),
        escalations: escalation.into_iter().collect(),
        log: Vec::new(),
    }
}

#[test]
fn after_step_cancel_keeps_the_losing_step_and_stops_at_its_boundary() {
    let cx = LocalTestCx::new(script(
        1,
        Some(TurnCancelSignal::AfterStep(evidence())),
        Some(None),
    ));
    let f = cx.fenced(sealed()).expect("a sealed verdict fences");

    let settled = run(step_under_cancel(&f, command("step-1"), &scope())).expect("no engine fault");

    assert_eq!(
        settled,
        Settled::StopAtBoundary {
            code: RuntimeErrorCode::StoreCommitFailed,
            evidence: evidence(),
        }
    );
    assert_eq!(cx.log(), ["step:step-1", "dispose:Abandoned"]);
}

#[test]
fn immediate_cancel_requests_the_losing_step_cancelled() {
    let cx = LocalTestCx::new(script(
        3,
        Some(TurnCancelSignal::Immediate(evidence())),
        None,
    ));
    let f = cx.fenced(sealed()).expect("a sealed verdict fences");

    let settled = run(step_under_cancel(&f, command("step-1"), &scope())).expect("no engine fault");

    assert_eq!(settled, Settled::Cancelled(evidence()));
    assert_eq!(cx.log(), ["step:step-1", "dispose:CancelRequested"]);
}

#[test]
fn escalation_during_an_after_step_wait_cancels_the_step() {
    let cx = LocalTestCx::new(script(
        5,
        Some(TurnCancelSignal::AfterStep(evidence())),
        Some(Some(TurnCancelSignal::Immediate(evidence()))),
    ));
    let f = cx.fenced(sealed()).expect("a sealed verdict fences");

    let settled = run(step_under_cancel(&f, command("step-1"), &scope())).expect("no engine fault");

    assert_eq!(settled, Settled::Cancelled(evidence()));
    assert_eq!(cx.log(), ["step:step-1", "dispose:CancelRequested"]);
}

#[test]
fn a_tie_at_one_poll_goes_to_the_first_arm() {
    let cx = LocalTestCx::new(script(
        0,
        Some(TurnCancelSignal::Immediate(evidence())),
        None,
    ));
    let f = cx.fenced(sealed()).expect("a sealed verdict fences");

    let settled = run(step_under_cancel(&f, command("step-1"), &scope())).expect("no engine fault");

    assert_eq!(
        settled,
        Settled::Stepped {
            code: RuntimeErrorCode::StoreCommitFailed
        }
    );
    assert_eq!(cx.log(), ["step:step-1", "dispose:Abandoned"]);
}

#[test]
fn only_a_sealed_or_valid_verdict_fences() {
    let cx = LocalTestCx::new(Script::default());

    assert!(cx.fenced(sealed()).is_some());
    assert!(
        cx.fenced(FenceSource::Inherited(InheritVerdict::Valid(fence())))
            .is_some()
    );
    assert!(
        cx.fenced(FenceSource::Sealed(SealVerdict::Superseded { epoch: 8 }))
            .is_none()
    );
    assert!(
        cx.fenced(FenceSource::Inherited(InheritVerdict::Stale {
            current_epoch: 8
        }))
        .is_none()
    );

    let f = cx.fenced(sealed()).expect("a sealed verdict fences");
    let child = f.inherited(ReplayKey::new("parent"), Some((GroupKey::new("group"), 2)));
    assert_eq!(child.fence(), &fence());
    assert_eq!(child.fence().epoch(), 7);
}

#[test]
fn a_send_engine_makes_the_generic_drive_send() {
    fn assert_send<T: Send>(_: &T) {}

    let cx = SendTestCx::new(script(0, Some(TurnCancelSignal::SessionRevoked), None));
    let f = cx.fenced(sealed()).expect("a sealed verdict fences");
    let scope = scope();
    let drive = step_under_cancel(&f, command("step-1"), &scope);
    assert_send(&drive);

    let settled = run(drive).expect("no engine fault");
    assert_eq!(
        settled,
        Settled::Stepped {
            code: RuntimeErrorCode::StoreCommitFailed
        }
    );
    assert_eq!(cx.log(), ["step:step-1", "dispose:Abandoned"]);
}

#[test]
fn the_revoked_session_cancels_the_step() {
    let cx = LocalTestCx::new(script(2, Some(TurnCancelSignal::SessionRevoked), None));
    let f = cx.fenced(sealed()).expect("a sealed verdict fences");

    let settled = run(step_under_cancel(&f, command("step-1"), &scope())).expect("no engine fault");

    assert_eq!(settled, Settled::Revoked);
    assert_eq!(cx.log(), ["step:step-1", "dispose:CancelRequested"]);
}

#[test]
fn recorded_verdicts_round_trip() {
    let verdicts = [
        SealVerdict::Sealed(fence()),
        SealVerdict::SubstrateLost {
            root: crate::TurnId::from("root-1"),
        },
        SealVerdict::Superseded { epoch: 9 },
    ];
    for verdict in verdicts {
        let bytes = serde_json::to_vec(&verdict).expect("encode");
        let decoded: SealVerdict = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, verdict);
    }

    let signal = TurnCancelSignal::AfterStep(evidence());
    let decoded: TurnCancelSignal =
        serde_json::from_value(serde_json::to_value(&signal).expect("encode")).expect("decode");
    assert_eq!(decoded, signal);
}
