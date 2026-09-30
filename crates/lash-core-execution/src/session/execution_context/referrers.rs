//! The referrers an execution acquires artifacts under (ADR 0113 §3): its
//! own journal, the frame its turn was admitted on, and the subscription
//! revision a trigger command commits.

use std::sync::Arc;

use super::RuntimeExecutionContext;

impl RuntimeExecutionContext<'_> {
    /// The execution referrer of this replayable execution (ADR 0113 §3.7):
    /// the journal of the scope that runs it, which holds what it publishes
    /// until the engine settles that journal.
    ///
    /// # Errors
    ///
    /// A scope with no journal identity.
    pub fn execution_referrer(&self) -> Result<crate::ArtifactReferrer, crate::PluginError> {
        execution_referrer_of(self.dispatch.effect_controller.execution_scope())
    }

    /// The claim of [`Self::execution_referrer`], armed with its journal
    /// guard.
    ///
    /// # Errors
    ///
    /// A scope with no journal identity.
    pub fn execution_claim(&self) -> Result<crate::ReferrerClaim, crate::PluginError> {
        execution_claim_of(self.dispatch.effect_controller.execution_scope())
    }

    /// The frame-environment referrer of the frame this execution's turn was
    /// admitted on (ADR 0113 §3.1): what holds every artifact a global of
    /// that frame references until the frame ends. Only a turn admitted on
    /// the frame writes it, and a write after the frame's switch commit fails
    /// `ReferrerEnded`.
    pub fn frame_referrer(&self) -> crate::ArtifactReferrer {
        crate::ArtifactReferrer::FrameEnvironment(crate::FrameEnvironmentId::new(
            self.session_id.clone(),
            self.dispatch.agent_frame_id.clone(),
        ))
    }

    /// The claim of [`Self::frame_referrer`]; a frame is unguarded, because
    /// its record is the session's own frame commit.
    ///
    /// # Errors
    ///
    /// Never for a frame referrer; the error is the claim constructor's.
    pub fn frame_claim(&self) -> Result<crate::ReferrerClaim, crate::PluginError> {
        crate::ReferrerClaim::unguarded(self.frame_referrer())
            .map_err(|error| crate::PluginError::Session(error.to_string()))
    }

    /// `store` wrapped so a trigger command holds the revision it commits
    /// before it commits, under this execution's journal (ADR 0113 §3.4).
    pub(super) fn revision_referrer_trigger_store(
        &self,
        store: Arc<dyn crate::TriggerStore>,
    ) -> Result<Arc<dyn crate::TriggerStore>, crate::RuntimeEffectControllerError> {
        let creator = self
            .dispatch
            .effect_controller
            .execution_scope()
            .journal_identity()?;
        Ok(Arc::new(
            crate::triggers::RevisionReferrerTriggerStore::new(
                store,
                self.dispatch.process_engines.clone(),
                creator,
            ),
        ))
    }
}

/// The execution referrer of `scope`: its journal (ADR 0113 §3.7).
fn execution_referrer_of(
    scope: &crate::ExecutionScope,
) -> Result<crate::ArtifactReferrer, crate::PluginError> {
    scope
        .journal_identity()
        .map(crate::ArtifactReferrer::Execution)
        .map_err(|error| crate::PluginError::Session(error.to_string()))
}

/// The claim of `scope`'s execution referrer, armed with its journal guard.
pub(crate) fn execution_claim_of(
    scope: &crate::ExecutionScope,
) -> Result<crate::ReferrerClaim, crate::PluginError> {
    crate::ReferrerClaim::guarded(
        execution_referrer_of(scope)?,
        crate::ArtifactCleanupPlan::AwaitJournal,
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))
}
