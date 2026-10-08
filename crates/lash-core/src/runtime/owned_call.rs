//! Compaction and direct calls are admitted under the execution that owns
//! them (ADR 0133 §8, FIG-5259), as a turn's call is at `model.start`.
//!
//! A new call composes the sections of its purpose over an empty tool offer
//! and lowers them into its request, the provider of its route lowers the
//! request to its request template, every attachment the template's slots
//! name is held under the owner (ADR 0135 §7), and `completion.start`
//! commits the call's record (its prompt snapshot, template, response
//! context and model-total deadline) under the owner's fence before the
//! first byte is sent. The
//! call's identity is its owner's execution scope and its stable key there,
//! with no turn required.
//!
//! A redrive of the owner makes the same call again: it finds the record,
//! sends the stored template under the pinned deadline, its slots filled
//! afresh, reads the response under the stored response context, and
//! composes, lowers and records nothing. The provider is handed the body
//! and that context on both paths and never a request, so a resend cannot
//! be answered differently from its first attempt (FIG-5479). A record whose template
//! cannot be read back as admitted, or a deadline that has passed, settles
//! the call unsent. The call's response
//! is durable only through its owner's own commit, as a turn's is through
//! `model.done`.

use std::sync::Arc;

use lash_durable::CommitLabel;
use lash_durable::domain::{ModelCallId, PromptCallKey};

use crate::plugin::prompt::{
    AdmittedCallLoadError, OfferedTools, ProjectedHistoryStats, PromptCall, PromptCompositionError,
    PromptCutParts, PromptModel, admission_record, load_admitted_call,
};
use crate::prompt_sections::{PromptPlan, PromptPurpose};
use crate::{
    ActorContext, ExecutionBudgets, ExecutionLimit, FailureCode, LlmCallError, LlmRequest,
    PluginError, TurnFailureCode,
};
use lash_core_execution::AdmittedDirectSend;
use lash_core_execution::core_internal::{compose_prompt, prompt_cut};

/// What an owned call composes its prompt from: the owner's recorded plan,
/// config and plugins, and its committed session view when a session owns
/// it.
pub(in crate::runtime) struct OwnedPrompt {
    pub(in crate::runtime) facts: Option<Arc<dyn std::any::Any + Send + Sync>>,
    pub(in crate::runtime) plugins: Arc<crate::PluginSession>,
    pub(in crate::runtime) plan: PromptPlan,
    pub(in crate::runtime) config: crate::AdmittedPluginConfig,
    pub(in crate::runtime) frame: Option<crate::FrameNodeId>,
    pub(in crate::runtime) session: Option<crate::SessionReadView>,
}

/// One owned call to admit, or to resend as admitted.
pub(in crate::runtime) struct OwnedCall<'a> {
    /// The owner: its fence commits the admission.
    pub(in crate::runtime) cx: &'a ActorContext,
    pub(in crate::runtime) key: PromptCallKey,
    pub(in crate::runtime) purpose: PromptPurpose,
    pub(in crate::runtime) prompt: OwnedPrompt,
    /// The caller's request, before its prompt is composed into it.
    pub(in crate::runtime) request: LlmRequest,
    pub(in crate::runtime) binding: crate::LlmProfileBinding,
    /// The session's attachment store: every attempt delivers the
    /// template's slots from it.
    pub(in crate::runtime) attachment_store: Arc<crate::RuntimeAttachmentStore>,
    pub(in crate::runtime) budgets: ExecutionBudgets,
}

/// How an owned call goes on.
pub(in crate::runtime) enum OwnedAdmission {
    /// Send the admitted template.
    Send {
        /// The request the call's effect is issued and traced with: the one
        /// the template was lowered from, or on a resend the caller's own,
        /// with no prompt composed. It never reaches the provider: a send
        /// takes `admitted`'s template and recorded response context.
        request: Box<LlmRequest>,
        admitted: AdmittedDirectSend,
    },
    /// The call settles unsent with this error.
    Unsent(LlmCallError),
}

/// The key of the call `owner` makes as `key`, in `session`.
pub(in crate::runtime) fn owned_call_key(
    session: crate::SessionId,
    owner: &crate::ExecutionScope,
    key: impl Into<String>,
) -> PromptCallKey {
    PromptCallKey {
        session,
        call: ModelCallId::Owned {
            owner: owner.id().to_string(),
            key: key.into(),
        },
    }
}

impl OwnedCall<'_> {
    /// Admit the call, or find its admission.
    ///
    /// # Errors
    ///
    /// A live fault of the owner (a store fault, lost ownership, a full
    /// render queue, a model this worker cannot bind), which admitted
    /// nothing new: the owner's redrive makes the call again.
    pub(in crate::runtime) async fn admit(self) -> Result<OwnedAdmission, PluginError> {
        let reads = self.cx.durable_reads().map_err(live)?;
        let now = self.cx.durable_now().await.map_err(live)?;
        let now_ms = u64::try_from(now.0).unwrap_or(0);
        let per_request = self.budgets.provider().per_request();
        match load_admitted_call(reads, &self.key).await {
            Ok(Some(admitted)) => {
                // A resend: the stored template under the pinned deadline.
                let Some(deadline) = admitted.deadline else {
                    return Ok(OwnedAdmission::Unsent(unavailable(format!(
                        "{} was admitted with no deadline",
                        self.key.call
                    ))));
                };
                let deadline_ms = u64::try_from(deadline.0).unwrap_or(0);
                if now_ms >= deadline_ms {
                    return Ok(OwnedAdmission::Unsent(expired(&self.key, deadline_ms)));
                }
                Ok(OwnedAdmission::Send {
                    request: Box::new(self.request),
                    admitted: AdmittedDirectSend {
                        response: admitted.response(),
                        template: admitted.template,
                        deliveries: self.attachment_store,
                        limit: ExecutionLimit::starting_at(
                            now_ms,
                            std::time::Duration::from_millis(deadline_ms - now_ms),
                            per_request,
                        ),
                    },
                })
            }
            Ok(None) => self.admit_new(now_ms).await,
            Err(AdmittedCallLoadError::Store(error)) => Err(live(error)),
            Err(error) => Ok(OwnedAdmission::Unsent(unavailable(format!(
                "{}'s admitted request template cannot be sent: {error}",
                self.key.call
            )))),
        }
    }

    async fn admit_new(self, now_ms: u64) -> Result<OwnedAdmission, PluginError> {
        let Self {
            cx,
            key,
            purpose,
            prompt,
            request,
            binding,
            attachment_store,
            budgets,
        } = self;
        if !request.tools.is_empty() {
            return Ok(OwnedAdmission::Unsent(refused(
                TurnFailureCode::PromptCompositionFailed,
                format!("a {purpose:?} call offers no tools"),
            )));
        }
        let composed = match compose(&key, &purpose, prompt, &request).await? {
            Ok(composed) => composed,
            Err(error) => return Ok(OwnedAdmission::Unsent(error)),
        };
        let mut request = request;
        if let Some(composed) = &composed
            && (composed.initial_instructions.is_some() || composed.current_context.is_some())
        {
            crate::sansio::place_prompt(
                &mut request,
                composed.initial_instructions.as_deref().map(Arc::from),
                composed.current_context.as_deref().map(Arc::from),
                // An owned call's request is no projection: current
                // context is its own trailing User message.
                false,
            );
        }
        let mut provider = binding
            .bind_for_unjournaled_call()
            .map_err(PluginError::RuntimeEffectController)?;
        let mut lowered = request.clone();
        lowered.drop_foreign_replay(&provider.route_identity(lowered.model.wire_model()));
        lowered.stream_events = crate::session_model::transport_stream_events(&provider, None);
        let template = match provider.lower(&lowered).await {
            Ok(template) => template,
            Err(error) => {
                return Ok(OwnedAdmission::Unsent(
                    crate::runtime::effect::llm_call_error_from_transport(error),
                ));
            }
        };
        // Every attachment a slot names is held under the owner before the
        // call is admitted: a ref only this call names survives a takeover
        // and is released when the owner settles.
        if let Err(refused) = hold_slots(cx, &template)
            .await
            .map_err(PluginError::RuntimeEffectController)?
        {
            return Ok(OwnedAdmission::Unsent(refused));
        }
        let limit = budgets.model_call_limit(now_ms, None);
        let deadline =
            lash_durable::DurableInstant(i64::try_from(limit.expires_at).unwrap_or(i64::MAX));
        // What every send reads the response under is fixed here, from the
        // request the template was lowered from, and recorded with it.
        let response =
            lash_sansio::llm::types::ResponseContext::of_request(&lowered).with_senders(None, None);
        let record = admission_record(key, composed.as_ref(), &template, &response, Some(deadline))
            .map_err(|error| {
                PluginError::Session(format!("the call's admission does not encode: {error}"))
            })?;
        let mut tx = cx.begin().await.map_err(live)?;
        tx.write(record);
        cx.commit(tx, CommitLabel::COMPLETION_START)
            .await
            .map_err(live)?;
        Ok(OwnedAdmission::Send {
            request: Box::new(request),
            admitted: AdmittedDirectSend {
                template: Arc::new(template),
                response,
                deliveries: attachment_store,
                limit,
            },
        })
    }
}

/// Acquire every attachment `template`'s slots name under the holder of the
/// call's owner, `cx`'s execution: its journal, or a process's record
/// (ADR 0135 §7, ADR 0124 §4). It runs before the admission commits, and a
/// crash between the two leaves only a hold its owner's end releases. The
/// inner `Err` settles the call unsent: a ref whose content the store holds
/// no evidence for, or a permanent refusal (`AttachmentResolutionFailed`).
///
/// # Errors
///
/// A live fault of the owner: a transient store fault, or an owner whose
/// referrer already ended. It admitted nothing, so the owner's redrive
/// prepares the call again.
pub(in crate::runtime) async fn hold_slots(
    cx: &ActorContext,
    template: &lash_sansio::llm::types::RecordedRequestTemplate,
) -> Result<Result<(), LlmCallError>, crate::RuntimeEffectControllerError> {
    use lash_core_execution::runtime::attachment_delivery::{
        DeliveryAcquisition, acquire_under, receiving_claim,
    };
    let mut ids: Vec<crate::AttachmentId> = template
        .slots()
        .map(|slot| slot.reference.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(Ok(()));
    }
    ids.sort();
    ids.dedup();
    let live = |error: crate::PluginError| match error {
        crate::PluginError::RuntimeEffectController(error) => error,
        other => crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectAttachmentStore,
            other.to_string(),
        ),
    };
    let claim = receiving_claim(cx.execution_scope()).map_err(live)?;
    let referrers = cx.backend().attachment_referrers();
    match acquire_under(referrers.as_ref(), &claim, &ids)
        .await
        .map_err(live)?
    {
        DeliveryAcquisition::Held => Ok(Ok(())),
        DeliveryAcquisition::SourceGone { digest } => Ok(Err(unresolved(format!(
            "attachment `{digest}` the call names is not held by any owner"
        )))),
        DeliveryAcquisition::Refused { refusal } => Ok(Err(unresolved(format!(
            "the call's attachments cannot be held: {}",
            refusal.message
        )))),
        DeliveryAcquisition::ReceiverEnded { referrer } => {
            Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectAttachmentStore,
                format!(
                    "the call's owner `{}` ended before its attachments were held",
                    referrer.canonical_id()
                ),
            ))
        }
    }
}

/// The settlement of a call whose attachments cannot be held.
fn unresolved(message: String) -> LlmCallError {
    refused(TurnFailureCode::AttachmentResolutionFailed, message)
}

/// Compose `purpose`'s sections for the call over an empty tool offer.
/// `Ok(Ok(None))` when the owner's plugins register no sections.
async fn compose(
    key: &PromptCallKey,
    purpose: &PromptPurpose,
    prompt: OwnedPrompt,
    request: &LlmRequest,
) -> Result<Result<Option<crate::plugin::prompt::ComposedPrompt>, LlmCallError>, PluginError> {
    let OwnedPrompt {
        facts,
        plugins,
        plan,
        config,
        frame,
        session,
    } = prompt;
    let catalog = plugins.prompt_catalog();
    if catalog.sections().is_empty() {
        return Ok(Ok(None));
    }
    let profile = &request.model;
    let cut = prompt_cut(
        PromptCutParts {
            call: PromptCall {
                session_id: key.session.clone(),
                frame,
                run: request.scope.turn.as_ref().map(|turn| turn.run.clone()),
                turn: request.scope.turn.as_ref().map(|turn| turn.turn_id.clone()),
                iteration: 0,
                call: 0,
                purpose: purpose.clone(),
            },
            config,
            session,
            offered: OfferedTools::default(),
            model: PromptModel {
                profile: Some(profile.key().clone()),
                context_window_tokens: Some(profile.context_window_tokens() as u64),
                committed_usage: None,
            },
            history: ProjectedHistoryStats {
                messages: u32::try_from(request.messages.len()).unwrap_or(u32::MAX),
                estimated_tokens: u64::from(request.estimated_tokens()),
            },
            namespaces: plugins.committed_namespaces(),
        },
        facts,
    );
    match compose_prompt(
        &catalog,
        &plan,
        purpose,
        Arc::new(cut),
        plugins.prompt_render_pool(),
    )
    .await
    {
        Ok(composed) => Ok(Ok(Some(composed))),
        // A full shared queue is this process's load, not the call's
        // outcome: the owner's redrive composes again.
        Err(error @ PromptCompositionError::RenderersBusy { .. }) => Err(
            PluginError::RuntimeEffectController(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::PromptRenderersBusy,
                error.to_string(),
            )),
        ),
        Err(error) => Ok(Err(refused(
            TurnFailureCode::PromptCompositionFailed,
            error.to_string(),
        ))),
    }
}

fn live(error: lash_durable::DurableError) -> PluginError {
    PluginError::RuntimeEffectController(crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::StoreCommitFailed,
        error.to_string(),
    ))
}

fn refused(code: TurnFailureCode, message: String) -> LlmCallError {
    LlmCallError {
        message,
        retryable: false,
        kind: crate::ProviderFailureKind::Unknown,
        raw: None,
        code: Some(FailureCode::lash(code)),
        terminal_reason: crate::LlmTerminalReason::ProviderError,
        request_body: None,
        partial_response: None,
    }
}

fn unavailable(message: String) -> LlmCallError {
    LlmCallError {
        kind: crate::ProviderFailureKind::Validation,
        ..refused(TurnFailureCode::AdmittedRequestUnavailable, message)
    }
}

/// The settlement of an owned call whose pinned deadline passed before a
/// resend: `ModelTotalExceeded`, never sent again.
fn expired(key: &PromptCallKey, deadline_ms: u64) -> LlmCallError {
    LlmCallError {
        kind: crate::ProviderFailureKind::Timeout,
        ..refused(
            TurnFailureCode::ModelTotalExceeded,
            format!(
                "{} reached its total limit (expired at {deadline_ms} ms)",
                key.call
            ),
        )
    }
}

#[cfg(test)]
#[path = "owned_call_tests.rs"]
mod tests;
