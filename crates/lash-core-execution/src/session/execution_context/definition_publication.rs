use super::RuntimeExecutionContext;

impl RuntimeExecutionContext<'_> {
    /// Publishes a compiled literal through its enclosing execution journal.
    /// A recorded publication returns its recorded definition without dispatch.
    ///
    /// # Errors
    ///
    /// A publication refusal, unavailable artifact ports, or journal failure.
    pub async fn publish_compiled_definition(
        &self,
        effect_id: String,
        draft: crate::ProcessDefinitionDraft,
        module: Option<crate::DeclaredModuleArtifact>,
    ) -> Result<crate::ProcessDefinition, crate::RuntimeEffectControllerError> {
        let claim = self.execution_claim()?;
        let invocation = self.language_runtime_invocation(&effect_id);
        let outcome = self
            .dispatch
            .effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::process(
                        crate::ProcessCommand::PublishDefinition { draft, module },
                    ),
                ),
                crate::RuntimeEffectLocalExecutor::definition_artifacts(
                    self.definition_engines().clone(),
                    claim,
                ),
            )
            .await?
            .into_process()?;
        match outcome {
            crate::ProcessEffectOutcome::Definition { definition } => Ok(*definition),
            _ => Err(crate::RuntimeEffectControllerError::foreign(
                "definition_publication_outcome",
                crate::TurnFailureCause::Outcome,
                "definition publication returned a different outcome",
            )),
        }
    }
}
