//! Laws of one call's admitted attempt in memory.

use std::sync::Arc;

use lash_core_ids::BehaviorRevision;

use super::super::singleton_run::{
    BeforeCheckReply, SingletonAttempt, SingletonBodyOutcome, SingletonCapture,
    SingletonPreparedRequest, SingletonPresentationError, SingletonToolCall, SingletonToolHandlers,
    StartLaunch,
};
use super::{AdmittedToolCall, AttemptEnd, CallEnd};
use crate::ToolCallId;
use crate::store::plugin_writers::{PluginCallbackIdentity, PluginRevision};
use crate::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict, ExternalCancelPolicy,
    PresentationBinding, ToolDeclaration,
};

fn callback(key: &str) -> PluginCallbackIdentity {
    PluginCallbackIdentity {
        owner: PluginRevision::new("tools", BehaviorRevision::ONE),
        key: key.into(),
    }
}

/// An ordinary call `label` of `read_file`.
fn ordinary_call(label: &str) -> SingletonToolCall {
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

    async fn stage_start(
        &self,
        _obligation: &crate::runtime::process::DeclaredStartObligation,
    ) -> Result<StartLaunch, String> {
        self.launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err("an ordinary call launches no process".into())
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
        let call = ordinary_call("ordinary");
        assert!(!call.declaration.isolated, "the call is ordinary");
        let admitted = AdmittedToolCall::admit(
            handlers.clone(),
            call,
            &crate::ExecutionScope::session_operation("session", "tools"),
        )
        .await
        .expect("the call is admitted");
        let end = admitted
            .attempt(
                AttemptOrdinal::FIRST,
                false,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("the attempt ends");
        assert!(
            matches!(
                end,
                AttemptEnd::Ended(CallEnd::Final {
                    capture: SingletonCapture::Failed { .. } | SingletonCapture::Done { .. },
                    launched: None,
                    ..
                })
            ),
            "the ordinary attempt is the call's final: {end:?}"
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
