use super::*;
impl ProcessEngineRegistry {
    /// The definition `draft` is: its content-derived id and the signature
    /// its owning engine derives for it.
    ///
    /// # Errors
    ///
    /// A [`lash_execution_contracts::process::ProcessDefinitionError::Refused`] judgment (no engine of
    /// that kind or an unresolvable value), or a typed operational fault.
    pub async fn derive_definition(
        &self,
        draft: &ProcessDefinitionDraft,
    ) -> Result<ProcessDefinition, super::ProcessDefinitionError> {
        let resolution = self.resolve_definition(draft).await?;
        Ok(ProcessDefinition::new(draft.id(), resolution.signature))
    }

    /// What `draft`'s owning engine says of it, after checking its manifest.
    ///
    /// # Errors
    ///
    /// A [`lash_execution_contracts::process::ProcessDefinitionError::Refused`] judgment for `draft`,
    /// or a typed operational fault.
    pub async fn resolve_definition(
        &self,
        draft: &ProcessDefinitionDraft,
    ) -> Result<ProcessDefinitionResolution, super::ProcessDefinitionError> {
        self.check_definition_manifest(draft)?;
        self.resolve(&ProcessDefinitionRef::unclaimed(
            draft.engine_kind().clone(),
            draft.value().clone(),
        ))
        .await
    }

    /// Refuses a draft whose manifest is not exactly the set of artifacts its
    /// owning engine resolves its value to
    /// ([`ProcessEngine::start_artifacts`](lash_execution_contracts::process::ProcessEngine::start_artifacts)):
    /// a manifest is what every reader holds, so one that says less or other
    /// than what the engine reads would let a start outlive its bytes.
    ///
    /// # Errors
    ///
    /// [`ProcessDefinitionRefusal::UnknownEngine`], an
    /// [`ProcessDefinitionRefusal::UnresolvableDefinition`] for a value the
    /// engine cannot name artifacts for, and
    /// [`ProcessDefinitionRefusal::ManifestMismatch`], wrapped as a judgment.
    /// An operational error from the engine stays a typed fault.
    pub fn check_definition_manifest(
        &self,
        draft: &ProcessDefinitionDraft,
    ) -> Result<(), super::ProcessDefinitionError> {
        let engine = self.get(draft.engine_kind().as_str()).ok_or_else(|| {
            ProcessDefinitionRefusal::UnknownEngine {
                engine_kind: draft.engine_kind().clone(),
            }
        })?;
        let unresolvable = |message: String| ProcessDefinitionRefusal::UnresolvableDefinition {
            engine_kind: draft.engine_kind().clone(),
            message,
        };
        let resolved =
            engine
                .start_artifacts(draft.value().as_json())
                .map_err(|error| {
                    match error {
                lash_execution_contracts::plugin_error::PluginError::Runtime(error)
                    if error.code
                        == lash_core_store::runtime_error::RuntimeErrorCode::DefinitionRefused =>
                {
                    lash_execution_contracts::process::ProcessDefinitionError::Refused(
                        unresolvable(error.message),
                    )
                }
                error => lash_execution_contracts::process::ProcessDefinitionError::Fault(error),
            }
                })?;
        // The engine's names in the draft's own order: sorted by leaf bytes
        // and deduplicated.
        let resolved = ProcessDefinitionDraft::new(
            draft.engine_kind().clone(),
            draft.value().clone(),
            resolved,
        )
        .map_err(|error| unresolvable(error.to_string()))?
        .artifacts()
        .to_vec();
        if resolved != draft.artifacts() {
            return Err(ProcessDefinitionRefusal::ManifestMismatch {
                engine_kind: draft.engine_kind().clone(),
                declared: draft.artifacts().to_vec(),
                resolved,
            }
            .into());
        }
        Ok(())
    }

    /// Checks a held definition value against the descriptor its id names,
    /// answering the derived definition.
    ///
    /// The claimed id must be `draft`'s. An unknown signature asserts nothing
    /// and adopts the derivation; a known one must equal it exactly.
    ///
    /// # Errors
    ///
    /// [`ProcessDefinitionRefusal::DefinitionIdMismatch`] when the descriptor
    /// is not the claimed id's, [`ProcessDefinitionRefusal::SignatureMismatch`]
    /// for a forged signature, and any refusal of
    /// [`Self::derive_definition`].
    pub async fn verify_definition_claim(
        &self,
        draft: &ProcessDefinitionDraft,
        claimed: &ProcessDefinition,
    ) -> Result<ProcessDefinition, super::ProcessDefinitionError> {
        let derived_id = draft.id();
        if claimed.id != derived_id {
            return Err(ProcessDefinitionRefusal::DefinitionIdMismatch {
                claimed: claimed.id.clone(),
                derived: derived_id,
            }
            .into());
        }
        let derived = self.derive_definition(draft).await?;
        if !claimed.signature.is_unknown() && claimed.signature != derived.signature {
            return Err(ProcessDefinitionRefusal::SignatureMismatch {
                engine_kind: draft.engine_kind().clone(),
                claimed: claimed.signature.clone(),
                authoritative: derived.signature,
            }
            .into());
        }
        Ok(derived)
    }
}

#[cfg(test)]
#[path = "definition_tests.rs"]
mod tests;
