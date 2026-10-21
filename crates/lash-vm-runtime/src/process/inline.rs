//! Inline author input is fulfilled fresh; a linked callable retains its admitted origin.
use super::{KernelProcessDefinition, KernelProcessEngine, definition_draft, entry_signature};
use lash_execution_contracts::process::linking::*;
use lash_execution_contracts::process::{
    InlineCode, InlineDefinition, InlineRequest, InlineStarts,
};
use lash_vocabulary::names::LinkedStart;
use std::sync::Arc;
pub(crate) struct KernelInlineStarts {
    pub(crate) engine: Arc<KernelProcessEngine>,
}
#[async_trait::async_trait]
impl InlineStarts for KernelInlineStarts {
    async fn inspect(&self, request: &InlineRequest) -> Result<InlineDefinition, EnginePortError> {
        let documents = &self.engine.documents;
        let (start, linked) = match &request.code {
            InlineCode::Unlinked { document, selector } => {
                let fulfilled = documents
                    .fulfil(&FulfilRequest {
                        document: *document,
                        selector: ReachSelector::Start(selector.clone()),
                        snapshot: request.snapshot.clone(),
                        context: request.context.clone(),
                        purpose: LinkPurpose::Workflow {
                            attachment: request.attachment.clone(),
                        },
                    })
                    .await?;
                (
                    LinkedStart {
                        link: fulfilled.content.identity(),
                        selector: selector.clone(),
                    },
                    fulfilled.inspection,
                )
            }
            InlineCode::Linked { start } => {
                // A guest LinkId alone cannot enter the preparation cache: only a retained Link loads here.
                (
                    start.clone(),
                    documents
                        .inspect_link(start.link, &ReachSelector::Start(start.selector.clone()))
                        .await?,
                )
            }
        };
        let draft = definition_draft(&KernelProcessDefinition {
            code: super::InlineRequest::Linked { start },
        })
        .map_err(|e| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: e.to_string(),
            })
        })?;
        let signature = linked.signature.as_ref().map_or(
            lash_execution_contracts::process::ProcessSignature::Unknown,
            entry_signature,
        );
        Ok(InlineDefinition {
            draft,
            signature,
            linked,
        })
    }
    async fn realize(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: &InlineRequest,
    ) -> Result<InlineDefinition, EnginePortError> {
        let definition = self.inspect(request).await?;
        self.engine
            .documents
            .hold_publications(claim, &request.snapshot)
            .await?;
        self.engine
            .documents
            .hold(claim, &ContentRef::Link(definition.linked.link))
            .await?;
        Ok(definition)
    }
}
