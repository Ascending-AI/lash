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

use lash_sansio::sync::MutexExt as _;
use serde::{Deserialize, Serialize};

use super::executor::RuntimeEffectControllerError;

/// The durable format version [`ToolPresentation`] stamps and
/// [`ToolPresentation::validate`] refuses mismatches against.
///
/// Version 2 (FIG-3515) carries message parts whose tool results hold ordered
/// text and attachment blocks, one result per call.
///
/// Version 3 (FIG-3607) names processes by their minted id alone, so a
/// presented process handle carries no incarnation.
///
/// Under the pre-1.0 freeze the shape changes in place (FIG-1643): it
/// journals the output-retention policy the boundary applied, and a result
/// block may be a retained output — a bounded witness and the attachment
/// holding the complete text.
pub const TOOL_PRESENTATION_VERSION: u16 = 3;

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
    /// The durable format version, refused rather than defaulted.
    pub version: u16,
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
}

impl ToolPresentation {
    /// Refuses a presentation this build cannot read completely, the same
    /// contract [`ToolSettlement::validate`](super::ToolSettlement::validate)
    /// gives the settlement record.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.version != TOOL_PRESENTATION_VERSION {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolSettlementVersion,
                format!(
                    "tool presentation records format version {}, and this build reads version \
                     {TOOL_PRESENTATION_VERSION}; a presentation that cannot be read completely is \
                     refused rather than served as a prefix of what the chain produced",
                    self.version
                ),
            ));
        }
        Ok(())
    }
}

/// The [`crate::plugin::ToolPresentationArtifacts`] implementation the runtime
/// binds onto a presentation context: retains bytes into the session's
/// content-addressed [`SessionAttachmentStore`](crate::SessionAttachmentStore)
/// (manifest-referenced, so mark-and-sweep GC retains them) and collects the
/// returned refs for the journaled outcome.
pub struct SessionPresentationArtifacts {
    store: Arc<crate::SessionAttachmentStore>,
    retained: std::sync::Mutex<Vec<crate::AttachmentRef>>,
    failure: std::sync::Mutex<Option<String>>,
}

impl SessionPresentationArtifacts {
    pub fn new(store: Arc<crate::SessionAttachmentStore>) -> Self {
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
    ) -> Pin<Box<dyn Future<Output = Result<crate::AttachmentRef, crate::PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            let meta = crate::AttachmentCreateMeta::new(
                crate::MediaType::parse("text/plain").map_err(|error| {
                    crate::PluginError::Session(format!(
                        "the retained-output media type is fixed and cannot parse: {error}"
                    ))
                })?,
                None,
                Some(label.to_string()),
            );
            let reference = match self.store.put(text.as_bytes().to_vec(), meta).await {
                Ok(reference) => reference,
                Err(error) => {
                    let message = format!(
                        "retaining the full tool output as a session artifact failed: {error}"
                    );
                    self.failure
                        .lock_recover()
                        .get_or_insert_with(|| message.clone());
                    return Err(crate::PluginError::Session(message));
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

    fn retention_failure(&self) -> Option<String> {
        self.failure.lock_recover().clone()
    }
}

/// The typed failure of a retention a presentation needed (FIG-1643): the
/// output is never recorded in its place, and the step retries.
pub fn output_retention_failed(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(crate::RuntimeErrorCode::OutputRetentionFailed, message)
        .retryable_uncommitted_derivation()
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
        .map_err(|error| output_retention_failed(error.to_string()))?;
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
