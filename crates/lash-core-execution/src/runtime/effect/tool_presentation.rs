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
//! execution and replay cannot change what the model was shown, and a retained
//! blob is `put` exactly once.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lash_sansio::sync::MutexExt as _;
use serde::{Deserialize, Serialize};

use super::executor::RuntimeEffectControllerError;

/// The durable format version [`ToolPresentation`] stamps and
/// [`ToolPresentation::validate`] refuses mismatches against. Guarded by
/// `scripts/versioned-surfaces.toml`.
pub const TOOL_PRESENTATION_VERSION: u16 = 1;

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
}

impl SessionPresentationArtifacts {
    pub fn new(store: Arc<crate::SessionAttachmentStore>) -> Self {
        Self {
            store,
            retained: std::sync::Mutex::new(Vec::new()),
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
            let reference = self
                .store
                .put(text.as_bytes().to_vec(), meta)
                .await
                .map_err(|error| {
                    crate::PluginError::Session(format!(
                        "retaining the full tool output as a session artifact failed: {error}"
                    ))
                })?;
            self.retained.lock_recover().push(reference.clone());
            Ok(reference)
        })
    }

    fn retained(&self) -> Vec<crate::AttachmentRef> {
        self.retained.lock_recover().clone()
    }
}
