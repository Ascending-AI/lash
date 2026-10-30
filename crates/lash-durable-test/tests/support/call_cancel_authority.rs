//! A singleton cannot borrow an enclosing admission or invent one from IDs.
use super::*;
use lash_core_execution::tool_dispatch::*;
use lash_core_execution::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, ExternalCancelPolicy,
    PresentationBinding,
};
use lash_vocabulary::providers::PluginCallbackIdentity;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Probe(AtomicUsize);

#[async_trait::async_trait]
impl SingletonToolHandlers for Probe {
    async fn prepare(&self, _: &SingletonToolCall) -> Result<Value, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Value::Null)
    }
    async fn before_checks(
        &self,
        _: &SingletonToolCall,
        _: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(Vec::new())
    }
    async fn execute(&self, _: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        panic!("an unauthorised singleton must never execute")
    }
    async fn after_checks(
        &self,
        _: &ToolCallId,
        _: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }
    async fn present(
        &self,
        _: &ToolCallId,
        _: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        panic!("an unauthorised singleton must never present")
    }
    fn emit_stream(&self, _: &ToolCallId, _: &lash_core_execution::runtime::effect::AttemptStream) {
    }
    async fn stage_start(
        &self,
        _: &lash_core_execution::runtime::process::DeclaredStartObligation,
    ) -> Result<StartLaunch, String> {
        panic!("an unauthorised singleton must never launch")
    }
}

pub(crate) async fn probe(call_id: ToolCallId) -> Vec<String> {
    let handlers = Arc::new(Probe::default());
    let callback = PluginCallbackIdentity {
        owner: "tools".to_owned(),
        key: "authority-probe".to_owned(),
    };
    let call = SingletonToolCall {
        owner: lash_core_execution::EffectOpener::turn("foreign-session", "foreign-turn"),
        call_id,
        tool_name: "authority-probe".to_owned(),
        arguments: Value::Null,
        declaration: lash_vocabulary::ToolDeclaration::default(),
        binding: AdmittedBinding {
            executable: callback.clone(),
            preparation: callback,
            presentation: PresentationBinding {
                presenter: None,
                steps: Vec::new(),
            },
        },
        cancel: ExternalCancelPolicy::CancelExternalWork,
    };
    let result = AdmittedToolCall::admit(
        handlers.clone(),
        call,
        &lash_core_execution::ExecutionScope::session_operation(
            "foreign-session",
            "authority-probe",
        ),
    )
    .await;
    let mut violations = Vec::new();
    if result.is_ok() {
        violations.push("a singleton without its own actor authority was admitted".to_owned());
    }
    if handlers.0.load(Ordering::SeqCst) != 0 {
        violations.push("unauthorised preparation ran before authority was refused".to_owned());
    }
    violations
}

pub(crate) async fn typed_cancel(
    world: &ExternalWorld,
    call: ToolCall<'_>,
) -> lash_core::ToolAttemptOutcome {
    let violations = call_cancel::authority::probe(call.context.call_id().clone()).await;
    eprintln!("same-ID foreign-owner refusals: {violations:?}");
    world.authority_violations.lock_recover().extend(violations);
    assert!(!call.context.cancellation_token().unwrap().is_cancelled());
    let mut cancellation = ToolCancellation::runtime("cancel the selected call")
        .with_origin(lash_core::CancelOrigin::OperatorRequested);
    cancellation.raw =
        Some(json!({"request_id": "host-call-cancel", "origin": "host-race-loser"}).into());
    lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::from_output(
        ToolCallOutput::cancelled(cancellation),
    ))
}
