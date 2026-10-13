//! The host's view of a session's prompt sections (ADR 0133): the plan its
//! config records, the sections its installed plugins register, and a
//! preview of how the plan resolves for a call. A preview is unadmitted: it
//! composes no text and records nothing.

use super::AdminContext;
use crate::support::{EmbedError, LashRuntime, Result};
use lash_core::plugin::prompt::{OfferedTools, PromptCatalogSnapshot};
use lash_core::prompt_sections::{PromptPlan, PromptPlanError, PromptPurpose, ResolvedPromptPlan};

#[derive(Clone)]
/// Facade handle for a session's prompt sections.
pub struct SessionPromptAdmin {
    pub(super) context: AdminContext,
}

impl SessionPromptAdmin {
    /// Read the retained snapshot of model call `call` in turn `run` from
    /// this session. Call ordinals start at one. The snapshot outlives the
    /// call's settlement, which releases only its exact provider body. This
    /// runs no renderer or wrapper and returns `None` when the snapshot is
    /// absent or released.
    pub async fn snapshot(
        &self,
        run: &crate::TurnId,
        call: u32,
    ) -> Result<Option<crate::prompt::LoadedPromptSnapshot>> {
        let context = self.context.target.context().await?;
        let key = crate::prompt::PromptCallKey {
            session: context.parts.session_id,
            call: crate::prompt::ModelCallId::Turn {
                run: run.clone(),
                ordinal: call,
            },
        };
        crate::plugin_diagnostics::read_prompt_snapshot(context.parts.effect_host.backend(), &key)
            .await
            .map_err(EmbedError::PromptSnapshotLoad)
    }

    /// The prompt plan the session's durable head records: what the next run
    /// executes under. A plan is changed only by
    /// [`SetPromptPlan`](crate::config::SetPromptPlan).
    pub async fn plan(&self) -> Result<PromptPlan> {
        let context = self.context.target.context().await?;
        crate::plugin_diagnostics::read_prompt_plan(&context.parts.store)
            .await?
            .ok_or_else(|| EmbedError::UnknownSession {
                session_id: context.parts.session_id,
            })
    }

    /// The sections, section families and wrappers the session's installed
    /// plugins register, in registration order.
    pub async fn catalog(&self) -> Result<PromptCatalogSnapshot> {
        let crate::send::SendTarget::Live(session) = &self.context.target else {
            return Err(EmbedError::MissingProtocolPlugin);
        };
        let host = session
            .binding
            .plugin_host
            .as_ref()
            .ok_or(EmbedError::MissingProtocolPlugin)?;
        crate::plugin_diagnostics::read_prompt_catalog(host).map_err(EmbedError::Plugin)
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
        self.context
            .with_writer(async |runtime: &mut LashRuntime| {
                runtime.reload_invalidated_resident_session_state().await?;
                runtime
                    .preview_prompt_plan(&plan, purpose, offered)
                    .map_err(EmbedError::Runtime)
            })
            .await
    }
}
