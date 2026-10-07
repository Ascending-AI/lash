//! Laws of a Run's calls in memory: its capacity, and what a check's verdict
//! does to the consumer that reads the call.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core_ids::BehaviorRevision;
use lash_sansio::sync::MutexExt as _;
use tokio::sync::Notify;

use super::super::singleton_run::{
    BeforeCheckReply, SingletonAttempt, SingletonBodyOutcome, SingletonCapture,
    SingletonPreparedRequest, SingletonPresentationError, SingletonRunError, SingletonToolCall,
    SingletonToolHandlers, StartLaunch,
};
use super::{Answer, CallEnd, Consumer, Leaf, ToolRun};
use crate::store::plugin_writers::{PluginCallbackIdentity, PluginRevision};
use crate::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, CallDecision, CapacityScope,
    ExternalCancelPolicy, HookCause, PresentationBinding, ToolDeclaration,
};
use crate::{ProcessId, ToolCallId};

fn callback(key: &str) -> PluginCallbackIdentity {
    PluginCallbackIdentity {
        owner: PluginRevision::new("tools", BehaviorRevision::ONE),
        key: key.into(),
    }
}

fn cause(error_type: &str) -> HookCause {
    HookCause {
        error_type: error_type.into(),
        error_version: std::num::NonZeroU32::MIN,
        payload: serde_json::Value::Null,
    }
}

/// Bodies that end when the test releases them, and before-checks it sets.
#[derive(Default)]
struct Calls {
    gates: Mutex<BTreeMap<ToolCallId, Arc<Notify>>>,
    before: Mutex<BTreeMap<ToolCallId, BeforeCheckReply>>,
}

impl Calls {
    fn gate(&self, call_id: &ToolCallId) -> Arc<Notify> {
        Arc::clone(
            self.gates
                .lock_recover()
                .entry(call_id.clone())
                .or_default(),
        )
    }

    fn release(&self, call_id: &ToolCallId) {
        self.gate(call_id).notify_one();
    }

    fn check(&self, call_id: &ToolCallId, reply: BeforeCheckReply) {
        self.before.lock_recover().insert(call_id.clone(), reply);
    }

    fn call(&self, label: &str) -> SingletonToolCall {
        SingletonToolCall {
            owner: crate::EffectOpener::turn("session", "turn"),
            call_id: ToolCallId::fixture(label),
            tool_name: "read_file".into(),
            arguments: serde_json::Value::Null,
            declaration: ToolDeclaration::default(),
            binding: AdmittedBinding {
                executable: callback("tool_provider:0"),
                preparation: callback("tool_provider:0"),
                presentation: PresentationBinding {
                    presenter: None,
                    steps: Vec::new(),
                },
            },
            available: vec![PluginRevision::new("tools", BehaviorRevision::ONE)],
            cancel: ExternalCancelPolicy::CancelExternalWork,
            environment: None,
        }
    }
}

#[async_trait::async_trait]
impl SingletonToolHandlers for Calls {
    async fn prepare(&self, _call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }

    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        _request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(self
            .before
            .lock_recover()
            .get(&call.call_id)
            .cloned()
            .map(|verdict| AttributedVerdict {
                callback: callback("tool_args_check:0"),
                verdict,
            })
            .into_iter()
            .collect())
    }

    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        self.gate(attempt.call_id).notified().await;
        Ok(SingletonBodyOutcome::Done {
            output: "done".into(),
            commands: crate::plugin::StateCommands::default(),
            intents: Vec::new(),
            start: None,
        })
    }

    async fn after_checks(
        &self,
        _call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }

    async fn cancel_call(&self, _call_id: &ToolCallId) -> Result<(), String> {
        Ok(())
    }

    async fn present(
        &self,
        _call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        Ok(capture.output().unwrap_or_default().to_owned())
    }

    fn emit_stream(&self, _call_id: &ToolCallId, _stream: &crate::runtime::effect::AttemptStream) {}

    async fn launch_start(
        &self,
        _obligation: &crate::runtime::process::DeclaredStartObligation,
    ) -> Result<StartLaunch, String> {
        Err("no call here declares a start".into())
    }

    async fn discharge_start(
        &self,
        _obligation: &crate::runtime::process::DeclaredStartObligation,
        _process_id: &ProcessId,
        _cancel: bool,
    ) -> Result<(), String> {
        Err("no call here declares a start".into())
    }
}

fn run<'a>() -> ToolRun<'a> {
    ToolRun::new(
        crate::ExecutionScope::session_operation("session", "tools"),
        Arc::new(crate::SystemClock),
    )
}

fn limit(calls: usize) -> crate::MaxToolCalls {
    crate::MaxToolCalls::new(calls)
}

fn control(error: &SingletonRunError) -> Option<(ToolCallId, bool)> {
    let SingletonRunError::Controller(error) = error else {
        return None;
    };
    match &error.cause {
        Some(crate::RuntimeErrorCause::ToolRunControl {
            call_id, aborted, ..
        }) => Some(((**call_id).clone(), *aborted)),
        _ => None,
    }
}

#[tokio::test]
async fn capacity_holds_a_round_whole_until_every_member_is_presented() {
    let calls = Arc::new(Calls::default());
    let handlers: Arc<dyn SingletonToolHandlers> = calls.clone();
    let mut run = run();
    let (a, b) = (calls.call("held-a"), calls.call("held-b"));
    run.admit_capacity(
        &CapacityScope::Held,
        vec![a.call_id.clone(), b.call_id.clone()],
        limit(3),
    )
    .expect("a round within the limit is admitted");
    let (a_id, b_id) = (a.call_id.clone(), b.call_id.clone());
    run.start(a, Arc::clone(&handlers)).unwrap();
    run.start(b, Arc::clone(&handlers)).unwrap();
    let next = vec![ToolCallId::fixture("held-c"), ToolCallId::fixture("held-d")];
    let refused = run
        .admit_capacity(&CapacityScope::Held, next.clone(), limit(3))
        .expect_err("two held calls leave room for one");
    assert_eq!((refused.counted, refused.requested), (2, 2));

    calls.release(&a_id);
    run.next_end().await;
    assert_eq!(
        run.counted(&CapacityScope::Held),
        2,
        "a member that ended releases nothing while its sibling runs"
    );
    calls.release(&b_id);
    run.next_end().await;
    assert_eq!(run.counted(&CapacityScope::Held), 0);
    run.admit_capacity(&CapacityScope::Held, next, limit(3))
        .expect("the ended round released its capacity");

    // A cell counts every call it ever made, ended or not.
    let cell = CapacityScope::Cell { key: "cell".into() };
    run.admit_capacity(&cell, vec![ToolCallId::fixture("cell-a")], limit(2))
        .unwrap();
    let refused = run
        .admit_capacity(
            &cell,
            vec![ToolCallId::fixture("cell-b"), ToolCallId::fixture("cell-c")],
            limit(2),
        )
        .expect_err("a cell's count never shrinks");
    assert_eq!((refused.counted, refused.requested), (1, 2));
}

#[tokio::test]
async fn a_check_cancel_rejects_its_operand_and_an_abort_is_run_control() {
    let calls = Arc::new(Calls::default());
    let handlers: Arc<dyn SingletonToolHandlers> = calls.clone();
    let mut run = run();
    let cancelled = calls.call("check-cancelled");
    let cancelled_id = cancelled.call_id.clone();
    calls.check(
        &cancelled_id,
        BeforeCheckReply::Cancel {
            cause: cause("tool_cancellation"),
        },
    );
    run.start(cancelled, Arc::clone(&handlers)).unwrap();
    run.form(
        "race".into(),
        vec![Leaf::Call(cancelled_id.clone())],
        vec![0],
    )
    .unwrap();
    assert_eq!(
        run.consume("race", Consumer::Race, true).await.unwrap(),
        Answer::Selected(0),
        "a check's cancel settles its operand as a rejection, never as the Run's control"
    );
    assert!(matches!(
        run.end(&cancelled_id).unwrap(),
        Some(CallEnd::Withheld {
            decision: CallDecision::CheckCancelled,
            ..
        })
    ));

    let aborted = calls.call("aborted");
    let aborted_id = aborted.call_id.clone();
    calls.check(
        &aborted_id,
        BeforeCheckReply::AbortRun {
            cause: cause("plugin_abort"),
        },
    );
    run.start(aborted, Arc::clone(&handlers)).unwrap();
    run.form("all".into(), vec![Leaf::Call(aborted_id.clone())], vec![0])
        .unwrap();
    assert_eq!(
        run.consume("all", Consumer::AllSettled, true)
            .await
            .unwrap(),
        Answer::HostControl {
            call_id: aborted_id,
            decision: CallDecision::Aborted,
        },
    );
}

#[tokio::test]
async fn an_aborted_run_admits_nothing_more() {
    let calls = Arc::new(Calls::default());
    let handlers: Arc<dyn SingletonToolHandlers> = calls.clone();
    let mut run = run();
    let aborted = calls.call("aborts");
    let aborted_id = aborted.call_id.clone();
    calls.check(
        &aborted_id,
        BeforeCheckReply::AbortRun {
            cause: cause("plugin_abort"),
        },
    );
    run.start(aborted, Arc::clone(&handlers)).unwrap();
    run.next_end().await;

    let refused = run
        .start(calls.call("after-abort"), Arc::clone(&handlers))
        .expect_err("an aborted Run starts no call");
    assert_eq!(control(&refused), Some((aborted_id.clone(), true)));
    let refused = run
        .form(
            "timers".into(),
            vec![Leaf::Timer { duration_ms: 1 }],
            vec![0],
        )
        .expect_err("an aborted Run forms no aggregate");
    assert_eq!(control(&refused), Some((aborted_id, true)));
}

/// An ordinary call whose tool also has a registered process
/// implementation: its body answers slowly with `body`, and every execution
/// and every start launch is counted.
struct SlowOrdinary {
    body: SingletonBodyOutcome,
    engines: crate::ProcessEngineRegistry,
    executions: std::sync::atomic::AtomicUsize,
    launches: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl SingletonToolHandlers for SlowOrdinary {
    fn process_engines(&self) -> Option<&crate::ProcessEngineRegistry> {
        Some(&self.engines)
    }

    fn isolated_start(
        &self,
        _call: &SingletonToolCall,
    ) -> Option<crate::tool_dispatch::IsolatedToolStart> {
        Some(crate::tool_dispatch::IsolatedToolStart {
            registration: crate::testing::held_engine_registration(
                serde_json::Value::Null,
                crate::ProcessProvenance::host(),
                crate::Lifetime::Detached,
            )
            .into(),
        })
    }

    async fn prepare(&self, _call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }

    async fn before_checks(
        &self,
        _call: &SingletonToolCall,
        _request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(Vec::new())
    }

    async fn execute(
        &self,
        _attempt: SingletonAttempt<'_>,
    ) -> Result<SingletonBodyOutcome, String> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(self.body.clone())
    }

    async fn after_checks(
        &self,
        _call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }

    async fn present(
        &self,
        _call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        Ok(capture.output().unwrap_or_default().to_owned())
    }

    fn emit_stream(&self, _call_id: &ToolCallId, _stream: &crate::runtime::effect::AttemptStream) {}

    async fn launch_start(
        &self,
        _obligation: &crate::runtime::process::DeclaredStartObligation,
    ) -> Result<StartLaunch, String> {
        self.launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err("an ordinary call launches no process".into())
    }

    async fn discharge_start(
        &self,
        _obligation: &crate::runtime::process::DeclaredStartObligation,
        _process_id: &ProcessId,
        _cancel: bool,
    ) -> Result<(), String> {
        Err("an ordinary call discharges no start".into())
    }
}

/// Q3 and D04: ordinary work stays cooperative. A slow success, or the
/// body's own transport timeout, is that one ordinary attempt even when the
/// tool also has a registered process implementation: isolation is chosen
/// by the declaration at admission, never by how the attempt went, so
/// nothing is launched as a process.
#[tokio::test]
async fn slow_or_timed_out_ordinary_work_is_never_rerun_as_a_process() {
    for body in [
        SingletonBodyOutcome::Failed {
            output: "transport timed out".into(),
            suggested_delay_ms: None,
        },
        SingletonBodyOutcome::Done {
            output: "done".into(),
            commands: crate::plugin::StateCommands::default(),
            intents: Vec::new(),
            start: None,
        },
    ] {
        let handlers = Arc::new(SlowOrdinary {
            body,
            engines: crate::testing::process_engine_fixture(),
            executions: Default::default(),
            launches: Default::default(),
        });
        let call = Calls::default().call("ordinary");
        assert!(!call.declaration.isolated, "the call is ordinary");
        let call_id = call.call_id.clone();
        let mut run = run();
        run.start(call, handlers.clone()).unwrap();
        run.next_end().await;
        assert!(
            matches!(
                run.end(&call_id).unwrap(),
                Some(CallEnd::Final {
                    capture: SingletonCapture::Failed { .. } | SingletonCapture::Done { .. },
                    launched: None,
                    ..
                })
            ),
            "the ordinary attempt is the call's final: {:?}",
            run.end(&call_id)
        );
        assert_eq!(
            handlers
                .executions
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            handlers.launches.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no process was launched"
        );
    }
}
