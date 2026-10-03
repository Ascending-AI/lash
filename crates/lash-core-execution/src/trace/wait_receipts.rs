//! Wait resolution is owned by its SQL business receipt, including a fully
//! replayed final wait. A journal answer carries data, never an emission right.
use crate::store::{EngineWaitKind, WaitRequestReceipt, WaitResolutionReceipt};
use crate::{
    ProcessCommand, RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, ScopedEffectController,
};
use lash_trace::{
    TraceDurableWaitResolution, TraceEvent, TraceRecordIdentity, TraceTransitionKind,
};

pub(crate) struct WaitBoundary {
    runtime: super::TraceRuntime,
    request: WaitRequestReceipt,
}
fn encoding_error(error: serde_json::Error) -> RuntimeEffectControllerError {
    crate::StoreError::Backend(error.to_string()).into()
}
impl WaitBoundary {
    pub(crate) async fn begin(
        controller: &ScopedEffectController<'_>,
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<Option<Self>, RuntimeEffectControllerError> {
        let kind = match &envelope.command {
            RuntimeEffectCommand::AwaitEvent { .. } => EngineWaitKind::Event,
            RuntimeEffectCommand::Sleep { .. } => EngineWaitKind::Timer,
            RuntimeEffectCommand::AwaitToolCompletions { .. } => EngineWaitKind::ToolCompletion,
            RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), ProcessCommand::Await { .. }) =>
            {
                EngineWaitKind::Process
            }
            _ => return Ok(None),
        };
        let Some(runtime) = controller.frontier().runtime() else {
            return Ok(None);
        };
        let Some(store) = runtime.wait_receipts() else {
            return Ok(None);
        };
        let invocation = &envelope.invocation;
        let request = WaitRequestReceipt {
            wait_id: serde_json::to_string(invocation.address()).map_err(encoding_error)?,
            owner_key: serde_json::to_string(invocation.execution_scope())
                .map_err(encoding_error)?,
            session_id: invocation.execution_scope().session_id().cloned(),
            request_digest: lash_trace::sha256_hex(
                serde_json::to_vec(&envelope.command).map_err(encoding_error)?,
            ),
            kind,
            scope: controller.trace_scope().cloned(),
            context: crate::trace_context_for_runtime_effect_invocation(
                lash_trace::TraceContext::default(),
                invocation,
            ),
            started_at_ms: runtime.clock().timestamp_ms(),
        };
        let receipt = store.record_wait_request(&request).await?;
        if let Some(scope) = &receipt.record.scope {
            runtime.emitter().emit(
                receipt.permit().as_ref(),
                scope,
                controller.controller().attempt_observation().as_ref(),
                || TraceRecordIdentity::Wait {
                    wait_id: receipt.record.wait_id.clone(),
                    transition: TraceTransitionKind::WaitStarted,
                },
                receipt.record.started_at_ms,
                || {
                    (
                        receipt.record.context.clone(),
                        TraceEvent::DurableWaitParked {
                            wait_kind: kind.as_str().into(),
                        },
                    )
                },
            );
        }
        Ok(Some(Self {
            runtime,
            request: receipt.record,
        }))
    }
    pub(crate) async fn resolve(
        self,
        outcome: &RuntimeEffectOutcome,
    ) -> Result<(), RuntimeEffectControllerError> {
        let resolution = match outcome {
            RuntimeEffectOutcome::AwaitToolCompletions { event } => match event {
                crate::ToolCompletionEvent::Resolved { resolution, .. } => match resolution {
                    crate::Resolution::Ok(_) => TraceDurableWaitResolution::Ok,
                    crate::Resolution::Err(_) => TraceDurableWaitResolution::Error,
                    crate::Resolution::Timeout => TraceDurableWaitResolution::Timeout,
                    crate::Resolution::Cancelled => TraceDurableWaitResolution::Cancelled,
                },
                crate::ToolCompletionEvent::DispatchReady => TraceDurableWaitResolution::Resolved,
                crate::ToolCompletionEvent::HandedOver => return Ok(()),
            },
            RuntimeEffectOutcome::AwaitEvent { resolution } => match resolution {
                crate::Resolution::Ok(_) => TraceDurableWaitResolution::Ok,
                crate::Resolution::Err(_) => TraceDurableWaitResolution::Error,
                crate::Resolution::Timeout => TraceDurableWaitResolution::Timeout,
                crate::Resolution::Cancelled => TraceDurableWaitResolution::Cancelled,
            },
            RuntimeEffectOutcome::Sleep
            | RuntimeEffectOutcome::Process {
                result: crate::ProcessEffectOutcome::Await { .. },
            } => TraceDurableWaitResolution::Resolved,
            _ => return Ok(()),
        };
        let Some(store) = self.runtime.wait_receipts() else {
            return Ok(());
        };
        let value = serde_json::to_value(outcome).map_err(encoding_error)?;
        let receipt = store
            .record_wait_resolution(&WaitResolutionReceipt {
                wait_id: self.request.wait_id.clone(),
                resolution_digest: lash_trace::sha256_hex(
                    serde_json::to_vec(&value).map_err(encoding_error)?,
                ),
                resolution: value,
                resolved_at_ms: self.runtime.clock().timestamp_ms(),
            })
            .await?;
        if let Some(scope) = &self.request.scope {
            self.runtime.emitter().emit(
                receipt.permit().as_ref(),
                scope,
                None,
                || TraceRecordIdentity::Wait {
                    wait_id: self.request.wait_id.clone(),
                    transition: TraceTransitionKind::WaitResolved,
                },
                receipt.record.resolved_at_ms,
                || {
                    (
                        self.request.context.clone(),
                        TraceEvent::DurableWaitResolved {
                            started_at_ms: self.request.started_at_ms,
                            wait_kind: self.request.kind.as_str().into(),
                            resolution,
                        },
                    )
                },
            );
        }
        Ok(())
    }
}
