use super::*;
use crate::runtime::effect::tool_child_driver::{complete_deferred_tool, resolve_model_return};
use lash_sansio::sync::MutexExt;

impl RuntimeExecutionContext<'_> {
    pub async fn await_deferred_tool_completions(
        &self,
        step: &str,
        waits: Vec<crate::ToolCompletionWait>,
        dispatch: Option<crate::ToolDispatchCursor>,
        transferable: bool,
    ) -> Result<crate::ToolCompletionEvent, crate::RuntimeEffectControllerError> {
        let wait = self.turn_cancel_wait(self.cancellation_token.clone().unwrap_or_default());
        let mut invocation = self.language_runtime_invocation(step);
        if let Some(crate::ExecutionScope::Turn {
            session_id,
            turn_id,
        }) = wait.observed_scope()
        {
            invocation.attribution.session_id = Some(session_id.clone());
            invocation.attribution.turn_id = Some(turn_id.clone());
        }
        let outcome = self
            .dispatch
            .effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::AwaitToolCompletions {
                        waits,
                        dispatch,
                        transferable,
                    },
                ),
                crate::RuntimeEffectLocalExecutor::await_event_under(
                    &wait,
                    Arc::clone(&self.dispatch.clock),
                ),
            )
            .await?;
        match outcome {
            crate::RuntimeEffectOutcome::AwaitToolCompletions {
                event: crate::ToolCompletionEvent::HandedOver,
            } => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::TurnWaitHandedOver,
                "the deferred tool round handed over",
            )),
            crate::RuntimeEffectOutcome::AwaitToolCompletions { event } => Ok(event),
            other => Err(crate::RuntimeEffectControllerError::wrong_outcome(
                crate::RuntimeEffectKind::AwaitToolCompletions,
                other.kind(),
            )),
        }
    }

    pub(crate) async fn abandon_deferred_tool_completion(
        &self,
        completion: crate::tool_dispatch::DeferredToolCompletion,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let resolved = self
            .dispatch
            .effect_controller
            .controller()
            .resolve_await_event(&completion.pending.key, crate::Resolution::Cancelled)
            .await?;
        let resolution = match resolved {
            crate::ResolveOutcome::AlreadyResolved { terminal } => terminal,
            _ => crate::Resolution::Cancelled,
        };
        let wait = self.turn_cancel_wait(self.cancellation_token.clone().unwrap_or_default());
        let site = crate::tool_dispatch::ParkSite {
            processes: self.dispatch.processes.as_ref(),
            owner: self.dispatch.owner.runtime_owner(),
            call_id: &completion.pending.call_id,
            scope: self.dispatch.process_scope().with_turn_cancellation(&wait),
            child_trace_hook: None,
        };
        crate::tool_dispatch::finish_parked_wait(
            &site,
            &completion.pending.pending,
            &completion.armed,
            &completion.pending.key,
            &resolution,
        )
        .await
    }

    pub async fn cancel_deferred_tool_completion(
        &self,
        completion: crate::tool_dispatch::DeferredToolCompletion,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        let resolved = self
            .dispatch
            .effect_controller
            .controller()
            .resolve_await_event(&completion.pending.key, crate::Resolution::Cancelled)
            .await?;
        let resolution = match resolved {
            crate::ResolveOutcome::AlreadyResolved { terminal } => terminal,
            _ => crate::Resolution::Cancelled,
        };
        self.finish_deferred_tool_completion(completion, resolution)
            .await
    }

    pub async fn finish_deferred_tool_completion(
        &self,
        mut completion: crate::tool_dispatch::DeferredToolCompletion,
        resolution: crate::Resolution,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        let request = completion.request.clone();
        if let Some(receipt) = &request.trace_request {
            self.tool_requests
                .lock_recover()
                .insert(request.call.call_id.clone(), receipt.clone());
        }
        // The dispatch rank already incorporated these facts before this wait began.
        completion.pending.captures.clear();
        completion.pending.triggers.clear();
        let call_key = format!("tool-completion:{}", request.call.call_id);
        let started = self.dispatch.clock.now();
        let wait = self.turn_cancel_wait(self.cancellation_token.clone().unwrap_or_default());
        let mut dispatch = self.dispatch.observation_keyed(&call_key);
        dispatch.owner = request.scope.owner.clone();
        dispatch.parent_invocation = request.lineage.parent_invocation().cloned();
        dispatch.tool_catalog = Arc::new(
            crate::runtime::effect::tool_child_driver::admitted_catalog(&request),
        );
        dispatch.execution_env_spec = crate::runtime::load_process_execution_env(
            self.process_env_store.as_ref(),
            &request.execution_env,
        )
        .await
        .map_err(|error| {
            crate::runtime::effect::executor::unresolved_execution_env(
                "deferred tool completion",
                &request.execution_env,
                error,
            )
        })?;
        dispatch.checkpoint_messages = crate::tool_dispatch::CheckpointMessageBuffer::default();
        dispatch.trigger_outcomes = crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        let mut outcome = complete_deferred_tool(&dispatch, completion, resolution, &wait).await?;
        let capture = crate::runtime::ToolAttemptCapture {
            messages: dispatch.checkpoint_messages.drain(),
            ..Default::default()
        };
        if !capture.is_empty() {
            outcome.captures.push(capture);
        }
        outcome.triggers.extend(dispatch.trigger_outcomes.drain());
        let duration = self
            .dispatch
            .clock
            .now()
            .saturating_duration_since(started)
            .as_millis() as u64;
        let model_return = resolve_model_return(
            &dispatch,
            &request,
            &outcome,
            crate::tool_dispatch::model_visible_intent_outcomes(&outcome),
            duration,
        )
        .await?;
        let settlement = crate::runtime::ToolSettlement::from_dispatch(&outcome, model_return);
        self.incorporate_tool_settlement(
            crate::session::SettlementSource::Invocation {
                call_id: request.call.call_id.clone(),
            },
            &settlement,
        )?;
        self.apply_tool_child_settlement(&call_key, &request.call, outcome, settlement, duration)
            .await
    }
}
