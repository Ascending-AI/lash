//! A turn's new model call is prepared for its admission (ADR 0133 §6,
//! FIG-5259): its prompt composed into its request, the protocol's
//! before-call decision taken, its attachments normalized, its correlation
//! scope set, and the request lowered by the provider of its route to the
//! exact body every attempt and resend sends. `model.start` commits the
//! result; nothing here runs again for an admitted call, and everything here
//! is repeat-safe until the call is admitted.

use std::sync::Arc;

use lash_sansio::llm::types::{LlmEventSender, ProviderRequestBody};

use super::*;
use crate::runtime::durable::session::{ComposedCall, PreparedCall};
use crate::{FailureCode, TurnFailureCode};

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
        request: Arc<LlmRequest>,
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
        let mut request = request;
        let degraded =
            crate::attachments::degrade_unmaterializable_request_attachments(&mut request);
        let mut request = LlmRequest::clone(&request);
        for notice in degraded {
            self.emit_trace(iteration, || lash_trace::TraceEvent::AttachmentDegraded {
                attachment_id: notice.attachment_id,
                label: notice.label,
                media_type: notice.media_type,
                source: notice.source,
                reason: notice.reason,
            });
        }
        // The projector built the request from the physical turn's committed
        // frame: its frame identity stays, and the runtime-owned correlation
        // fields are this call's.
        request.scope = crate::LlmRequestScope::new(
            self.session_id.clone(),
            request.scope.agent_frame_id.clone(),
            format!(
                "{}:turn:{}:llm:{}",
                self.session_id, self.turn_id, iteration
            ),
        )
        .with_turn(self.logical_run().into(), self.turn_id.clone());
        match self.lower_call(&request).await? {
            Ok(body) => Ok(PreparedCall::Admit(Box::new(ComposedCall {
                request: Arc::new(request),
                prompt,
                body,
            }))),
            Err(refused) => Ok(PreparedCall::Unsent(refused)),
        }
    }

    /// Lower `request` to its exact body on the provider the session's
    /// recorded model binds: attachments resolved, reasoning from another
    /// route dropped, the response streamed as every turn call streams. The
    /// inner `Err` settles the call unsent.
    async fn lower_call(
        &self,
        request: &LlmRequest,
    ) -> Result<Result<ProviderRequestBody, crate::LlmCallError>, RuntimeError> {
        let mut provider = self
            .policy
            .binding()
            .bind_for_unjournaled_call()
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let mut resolved = match crate::attachments::resolve_llm_request_attachments(
            request.clone(),
            self.host.core.durability.attachment_store.as_ref(),
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(err) => {
                return Ok(Err(super::prompt::refused(
                    FailureCode::lash(TurnFailureCode::AttachmentResolutionFailed),
                    err.to_string(),
                    None,
                )));
            }
        };
        resolved.drop_foreign_replay(&provider.route_identity(resolved.model.wire_model()));
        resolved.stream_events = Some(LlmEventSender::new(|_| {}));
        Ok(provider
            .lower(&resolved)
            .await
            .map_err(crate::runtime::effect::llm_call_error_from_transport))
    }
}
