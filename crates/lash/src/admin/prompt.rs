//! The host's view of a session's prompt sections (ADR 0133): the plan its
//! config records, the sections its installed plugins register, and a
//! preview of how the plan resolves for a call. A preview is unadmitted: it
//! composes no text and records nothing.

use super::*;
use lash_core::plugin::prompt::{OfferedTools, PromptCatalog};
use lash_core::prompt_sections::{PromptPlan, PromptPlanError, PromptPurpose, ResolvedPromptPlan};

#[derive(Clone)]
/// Facade handle for a session's prompt sections.
pub struct SessionPromptAdmin {
    pub(super) control: SessionAdmin,
}

impl SessionPromptAdmin {
    /// The prompt plan the session's durable head records: what the next run
    /// executes under. A plan is changed only by
    /// [`SetPromptPlan`](crate::config::SetPromptPlan).
    pub async fn plan(&self) -> Result<PromptPlan> {
        let context = self.control.target.context().await?;
        let head = context
            .parts
            .store
            .load_session_head_meta()
            .await
            .map_err(EmbedError::Store)?
            .ok_or_else(|| EmbedError::UnknownSession {
                session_id: context.parts.session_id.clone(),
            })?;
        Ok(head.config.prompt_plan)
    }

    /// The sections, section families and wrappers the session's installed
    /// plugins register, in registration order.
    pub async fn catalog(&self) -> Result<PromptCatalog> {
        self.control
            .with_writer(async |runtime: &mut LashRuntime| {
                runtime.reload_invalidated_resident_session_state().await?;
                runtime.prompt_catalog().map_err(EmbedError::Runtime)
            })
            .await
    }

    /// Preview how the recorded plan resolves for a `purpose` call offered
    /// `offered`: the sections it selects in order, each placement and whose
    /// choice it is, and every wrapper chain. It is unadmitted: no renderer
    /// runs and no call records it. A plan that does not resolve answers its
    /// typed [`PromptPlanError`].
    pub async fn preview(
        &self,
        purpose: &PromptPurpose,
        offered: &OfferedTools,
    ) -> Result<std::result::Result<ResolvedPromptPlan, PromptPlanError>> {
        let plan = self.plan().await?;
        let catalog = self.catalog().await?;
        Ok(catalog
            .resolve(&plan, purpose, offered)
            .map(|composition| composition.record().clone()))
    }
}
