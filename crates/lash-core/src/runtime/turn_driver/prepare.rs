//! A turn's new model call is prepared for its admission (ADR 0133 §6,
//! FIG-5259): its prompt composed into its request, the protocol's
//! before-call decision taken, attachments the target model cannot accept replaced with
//! their notices, its correlation scope set, and the request lowered by the
//! provider of its route to the request template every attempt and resend
//! fills and sends (ADR 0135 §6). Nothing is delivered here: an attachment
//! slot records its ref. `model.start` commits the result; nothing here runs
//! again for an admitted call, and everything here is repeat-safe until the
//! call is admitted.

use lash_protocol_types::core_support::RunIdCoreSupport;
use lash_protocol_types::llm::types::RetainedLlmRequest;
use std::sync::Arc;

use lash_protocol_types::{llm::types::LlmEventSender, llm::types::RecordedRequestTemplate};

use super::*;
use crate::runtime::durable::session::{ComposedCall, PreparedCall};

impl RuntimeTurnDriver<'static> {
    /// Prepare model call `call`, the request the machine waits on as `id`.
    ///
    /// # Errors
    ///
    /// A live fault of this activation, which admits nothing, so its resume
    /// prepares the call again.
    pub(super) async fn prepare_call(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        call: u32,
        request: Arc<RetainedLlmRequest>,
        event_tx: &TurnObserver,
    ) -> Result<PreparedCall, RuntimeError> {
        let iteration = machine.protocol_iteration();
        let messages = machine.prompt_message_sequence();
        let (request, prompt) = match self
            .compose_call(
                iteration,
                call,
                messages,
                request,
                machine.has_current_context_prefix(),
            )
            .await?
        {
            Ok(composed) => composed,
            Err(refused) => return Ok(PreparedCall::Unsent(refused)),
        };
        if !self
            .before_llm_call(machine, id, &request, event_tx)
            .await?
        {
            return Ok(PreparedCall::Ended);
        }
        let mut flat = if request.has_attachments() {
            Arc::new(request.materialize())
        } else {
            Arc::new(request.envelope().clone())
        };
        let provider = self
            .policy
            .binding()
            .bind_for_unjournaled_call()
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let target = Arc::clone(&request);
        let degraded = crate::attachments::degrade_unmaterializable_request_attachments(
            &mut flat,
            |reference, position| provider.accepts_attachment(&target, reference, position),
            |notice| {
                self.session
                    .plugins()
                    .protocol_driver()
                    .attachment_omission_text(notice)
            },
        );
        let mut request = (*request).clone();
        if !degraded.is_empty() {
            request = RetainedLlmRequest::try_from_flat(&flat, &self.allocation_budget)
                .map_err(RuntimeError::allocation)?;
        }
        for notice in degraded {
            self.emit_trace(iteration, || lash_trace::TraceEvent::AttachmentDegraded {
                attachment_id: Some(notice.attachment_id.to_string()),
                label: notice.label,
                media_type: Some(notice.media_type.to_string()),
                position: notice.position,
                reason: notice.reason,
            });
        }
        // The projector built the request from the physical turn's committed
        // frame: its frame identity stays, and the runtime-owned correlation
        // fields are this call's.
        let scope = crate::LlmRequestScope::new(
            self.session_id.clone(),
            request.scope.agent_frame_id.clone(),
            format!(
                "{}:turn:{}:llm:{}",
                self.session_id, self.turn_id, iteration
            ),
        )
        .with_turn(
            crate::RunId::from_turn_id(self.logical_run()),
            self.turn_id.clone(),
        );
        request
            .edit_envelope(&self.allocation_budget, 1024, |request| {
                request.scope = scope
            })
            .map_err(RuntimeError::allocation)?;
        match self.lower_call(&request).await? {
            Ok(template) => Ok(PreparedCall::Admit(Box::new(ComposedCall {
                request: Arc::new(request),
                prompt,
                template,
                prompt_state: self.prompt_state.clone(),
                allocation_budget: self.allocation_budget.clone(),
            }))),
            Err(refused) => Ok(PreparedCall::Unsent(refused)),
        }
    }

    /// Lower `request` to its request template on the provider the
    /// session's recorded model binds: reasoning from another route dropped,
    /// the response streamed as every turn call streams, one slot per
    /// attachment with the route's effective acceptance. The inner `Err`
    /// settles the call unsent.
    async fn lower_call(
        &self,
        request: &RetainedLlmRequest,
    ) -> Result<Result<RecordedRequestTemplate, crate::LlmCallError>, RuntimeError> {
        let mut provider = self
            .policy
            .binding()
            .bind_for_unjournaled_call()
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let mut lowered = request.clone();
        if lowered.has_replay() {
            let mut replay = lowered.materialize();
            if !replay
                .drop_foreign_replay(&provider.route_identity(lowered.model.wire_model()))
                .is_empty()
            {
                lowered = RetainedLlmRequest::try_from_request(replay, &self.allocation_budget)
                    .map_err(RuntimeError::allocation)?;
            }
        }
        lowered
            .edit_envelope(&self.allocation_budget, 128, |request| {
                request.stream_events = Some(LlmEventSender::new(|_| {}))
            })
            .map_err(RuntimeError::allocation)?;
        let answer = provider
            .lower_retained(
                &lowered,
                &self.allocation_budget,
                self.prompt_state.lowering_cache(),
            )
            .await
            .map(|mut template| {
                template.fetch_horizon = self.host.core.providers.delivery_fetch_horizon;
                template
            });
        match answer {
            Err(error) => {
                if let Some(refusal) = error.allocation_refusal() {
                    Err(RuntimeError::allocation(refusal.clone()))
                } else {
                    Ok(Err(crate::runtime::effect::llm_call_error_from_transport(
                        error,
                    )))
                }
            }
            Ok(template) => Ok(Ok(template)),
        }
    }
}
