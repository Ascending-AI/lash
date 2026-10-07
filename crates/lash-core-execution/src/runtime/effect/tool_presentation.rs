//! The recorded presentation of one settled tool result (ADR 0099 §6
//! presentation boundary, FIG-3420).
//!
//! [`ToolPresentation`] is what the journaled
//! [`PresentToolResult`](super::envelope::RuntimeEffectCommand::PresentToolResult)
//! effect produces: the `ModelToolReturn` the registered presentation steps
//! folded to, plus every artifact a step retained through the context's
//! [`ToolPresentationArtifacts`](crate::plugin::ToolPresentationArtifacts)
//! capability. Because the chain runs inside the journaled boundary, a replay
//! serves this record verbatim — a step added, removed, or changed between
//! execution and replay cannot change what the model was shown. A retained
//! blob is `put` again only when a crash lost the outcome before the journal
//! recorded it, and the content-addressed store converges that repeat on the
//! same blob (FIG-4095).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use lash_core_store::tool_run::PresentationBinding;
use lash_sansio::sync::MutexExt as _;
use serde::{Deserialize, Serialize};

use super::executor::RuntimeEffectControllerError;

/// Record selection independently of the callback body. Both current completion
/// paths consume this same K1 binding; replay never reselects installed hooks.
pub(crate) async fn record_tool_presentation_plan(
    context: &crate::tool_dispatch::ToolDispatchContext<'_>,
    call_id: &crate::ToolCallId,
) -> Result<PresentationBinding, RuntimeEffectControllerError> {
    let key = format!("{call_id}:presentation_plan");
    let controller = &context.effect_controller;
    let plugins = Arc::clone(&context.plugins);
    let envelope = super::RuntimeEffectEnvelope::new(
        super::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(controller.execution_scope().clone(), key.clone())?,
            context.parentless_attribution(),
            key,
        ),
        super::RuntimeEffectCommand::LanguageRuntimeValue {
            operation: "tool-presentation-plan".into(),
        },
    );
    let recorded = controller
        .vm_effect(
            envelope,
            super::RuntimeEffectLocalExecutor::language_runtime_value_with(move |_| async move {
                plugins
                    .validate_recorded_admission()
                    .map_err(RuntimeEffectControllerError::from)?;
                let value =
                    serde_json::to_value(plugins.tool_presentation_plan()).map_err(|error| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            format!("cannot record the presentation callback plan: {error}"),
                        )
                    })?;
                Ok(super::RuntimeEffectOutcome::LanguageRuntimeValue { value })
            }),
        )
        .await?
        .into_language_runtime_value()?;
    serde_json::from_value(recorded).map_err(|error| {
        RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode,
            format!("cannot read the recorded presentation callback plan: {error}"),
        )
    })
}

/// The journaled product of one tool result's presentation chain.
///
/// `model_return` has no serde default on purpose: a presentation without one
/// means the boundary was never reached, which is a refusal the reader must
/// see, not a hole to fill. `artifacts` lists every blob a step retained
/// through the journaled capability, in retain order, so the recorded outcome
/// names the refs the model-facing text can mention.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPresentation {
    /// The model-facing return the steps folded to, after the
    /// attachment-materialization notices computed under the recorded
    /// environment.
    pub model_return: crate::ModelToolReturn,
    /// Artifacts the steps retained while this presentation ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<crate::AttachmentRef>,
    /// The byte policy the boundary measured the folded return against
    /// (FIG-1643). Recorded whether or not it retained anything, so a replay
    /// under another policy serves this decision unchanged.
    pub retention: crate::OutputRetentionPolicy,
    /// What a host shows of the call, as the steps folded it and bounded to
    /// [`crate::TOOL_DISPLAY_LIMIT_BYTES`] (FIG-5290). Never model-facing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<crate::ToolDisplay>,
}

/// The [`crate::plugin::ToolPresentationArtifacts`] implementation the runtime
/// binds onto a presentation context: retains bytes into the session's
/// content-addressed [`RuntimeAttachmentStore`](crate::RuntimeAttachmentStore)
/// (manifest-referenced, so mark-and-sweep GC retains them) and collects the
/// returned refs for the journaled outcome.
pub struct SessionPresentationArtifacts {
    store: Arc<crate::RuntimeAttachmentStore>,
    retained: std::sync::Mutex<Vec<crate::AttachmentRef>>,
    failure: std::sync::Mutex<Option<RuntimeEffectControllerError>>,
}

impl SessionPresentationArtifacts {
    pub fn new(store: Arc<crate::RuntimeAttachmentStore>) -> Self {
        Self {
            store,
            retained: std::sync::Mutex::new(Vec::new()),
            failure: std::sync::Mutex::new(None),
        }
    }
}

impl crate::plugin::ToolPresentationArtifacts for SessionPresentationArtifacts {
    fn retain_text<'a>(
        &'a self,
        label: &'a str,
        text: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::AttachmentRef, crate::AttachmentStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let meta = crate::AttachmentCreateMeta::new(
                crate::MediaType::parse("text/plain").map_err(|error| {
                    crate::AttachmentStoreError::Contract(format!(
                        "the retained-output media type is fixed and cannot parse: {error}"
                    ))
                })?,
                None,
                Some(label.to_string()),
            );
            let reference = match self.store.put(text.as_bytes().to_vec(), meta).await {
                Ok(reference) => reference,
                Err(error) => {
                    self.failure.lock_recover().get_or_insert_with(|| {
                        RuntimeEffectControllerError::output_retention_failed(&error)
                    });
                    return Err(error);
                }
            };
            self.retained.lock_recover().push(reference.clone());
            Ok(reference)
        })
    }

    fn retention_policy(&self) -> crate::OutputRetentionPolicy {
        self.store.output_retention()
    }

    fn retained(&self) -> Vec<crate::AttachmentRef> {
        self.retained.lock_recover().clone()
    }

    fn retention_failure(&self) -> Option<RuntimeEffectControllerError> {
        self.failure.lock_recover().clone()
    }
}

/// Retains the folded return's text when it is longer than `policy` allows
/// in history (FIG-1643): the text blocks are retained whole as one session
/// artifact, and one [`crate::ModelToolReturnPart::Retained`] block — a
/// bounded witness and the artifact's reference — takes the first text
/// block's place. Attachment and already-retained blocks keep their order.
///
/// This runs after every step and after the materialization notices, so it
/// bounds what any of them added: a failure the renderer passed through, a
/// plugin step's appendix, a child's result.
pub async fn retain_oversized_return(
    model_return: &mut crate::ModelToolReturn,
    call_id: &crate::ToolCallId,
    artifacts: &dyn crate::plugin::ToolPresentationArtifacts,
    policy: crate::OutputRetentionPolicy,
) -> Result<(), RuntimeEffectControllerError> {
    let text = model_return
        .parts
        .iter()
        .filter_map(|part| match part {
            crate::ModelToolReturnPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if !policy.retains(text.len()) {
        return Ok(());
    }
    let reference = artifacts
        .retain_text(&format!("tool-output:{call_id}"), &text)
        .await
        .map_err(|error| RuntimeEffectControllerError::output_retention_failed(&error))?;
    let notice = format!(
        "\n[output retained: {} bytes exceed the {}-byte history limit; showing the first bytes; full output: attachment {}]",
        text.len(),
        policy.inline_limit_bytes,
        reference.id
    );
    let retained = crate::ModelToolReturnPart::Retained(crate::RetainedOutput {
        witness: policy.witness(&text, &notice),
        reference,
    });
    let mut parts = Vec::with_capacity(model_return.parts.len());
    let mut retained = Some(retained);
    for part in std::mem::take(&mut model_return.parts) {
        match part {
            crate::ModelToolReturnPart::Text { .. } => {
                if let Some(retained) = retained.take() {
                    parts.push(retained);
                }
            }
            other => parts.push(other),
        }
    }
    model_return.parts = parts;
    Ok(())
}
