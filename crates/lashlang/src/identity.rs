use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artifact::{HostRequirementsRef, ModuleArtifact, ModuleRef, ProcessRef};
use crate::runtime::{LASH_HOST_REQUIREMENTS_REF_KEY, LASH_MODULE_REF_KEY, LASH_PROCESS_REF_KEY};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDefinitionIdentity {
    pub module_ref: ModuleRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub process_ref: ProcessRef,
    #[serde(skip)]
    pub process_name: String,
}

impl ProcessDefinitionIdentity {
    pub fn new(
        module_ref: ModuleRef,
        host_requirements_ref: HostRequirementsRef,
        process_ref: ProcessRef,
        process_name: impl Into<String>,
    ) -> Self {
        Self {
            module_ref,
            host_requirements_ref,
            process_ref,
            process_name: process_name.into(),
        }
    }

    /// The normalized engine value contains only immutable artifact references.
    pub fn from_process_value(
        value: &serde_json::Value,
    ) -> Result<Self, ProcessDefinitionIdentityError> {
        let fields = value
            .as_object()
            .ok_or(ProcessDefinitionIdentityError::NotProcessValue)?;
        if fields.len() != 3
            || !fields.keys().all(|key| {
                [
                    LASH_MODULE_REF_KEY,
                    LASH_HOST_REQUIREMENTS_REF_KEY,
                    LASH_PROCESS_REF_KEY,
                ]
                .contains(&key.as_str())
            })
        {
            return Err(ProcessDefinitionIdentityError::NotProcessValue);
        }
        Ok(Self {
            module_ref: decode_field(value, LASH_MODULE_REF_KEY)?,
            host_requirements_ref: decode_field(value, LASH_HOST_REQUIREMENTS_REF_KEY)?,
            process_ref: decode_field(value, LASH_PROCESS_REF_KEY)?,
            process_name: String::new(),
        })
    }

    /// Canonical stock-engine descriptor, with no redundant export name.
    pub fn to_process_value(&self) -> serde_json::Value {
        serde_json::json!({
            LASH_MODULE_REF_KEY: self.module_ref,
            LASH_HOST_REQUIREMENTS_REF_KEY: self.host_requirements_ref,
            LASH_PROCESS_REF_KEY: self.process_ref,
        })
    }

    /// Canonical engine descriptor and its complete dependency manifest.
    pub fn draft(
        &self,
    ) -> Result<
        lash_core_execution::ProcessDefinitionDraft,
        lash_core_execution::ProcessDefinitionDraftError,
    > {
        lash_core_execution::ProcessDefinitionDraft::new(
            "lashlang",
            self.to_process_value(),
            [lash_core_execution::ArtifactName {
                store: lash_core_execution::ArtifactStoreId::LashlangModule,
                artifact_ref: self.module_ref.to_string(),
            }],
        )
    }

    pub fn definition(
        &self,
        signature: lash_core_execution::ProcessSignature,
    ) -> Result<
        lash_core_execution::ProcessDefinition,
        lash_core_execution::ProcessDefinitionDraftError,
    > {
        Ok(lash_core_execution::ProcessDefinition::new(
            self.draft()?.id(),
            signature,
        ))
    }

    pub fn from_artifact_export(artifact: &ModuleArtifact, process_name: &str) -> Option<Self> {
        let process_ref = artifact.process_ref(process_name)?.clone();
        Some(Self::new(
            artifact.module_ref().clone(),
            artifact.host_requirements_ref().clone(),
            process_ref,
            process_name,
        ))
    }

    pub fn matches_input_refs(
        &self,
        module_ref: &ModuleRef,
        host_requirements_ref: &HostRequirementsRef,
        process_ref: &ProcessRef,
        process_name: &str,
    ) -> bool {
        self.module_ref == *module_ref
            && self.host_requirements_ref == *host_requirements_ref
            && self.process_ref == *process_ref
            && (self.process_name.is_empty() || self.process_name == process_name)
    }

    pub fn matches_artifact_export(&self, artifact: &ModuleArtifact) -> bool {
        if &self.module_ref != artifact.module_ref()
            || &self.host_requirements_ref != artifact.host_requirements_ref()
        {
            return false;
        }
        artifact
            .process_name_for_ref(&self.process_ref)
            .is_some_and(|export_name| {
                self.process_name.is_empty() || export_name == self.process_name
            })
    }

    pub fn resolve_process_type(
        &self,
        artifact: &ModuleArtifact,
    ) -> Result<crate::TypeExpr, ProcessDefinitionIdentityError> {
        if !self.matches_artifact_export(artifact) {
            return Err(ProcessDefinitionIdentityError::ArtifactMismatch {
                process: self.process_name.clone(),
            });
        }
        let process_name = artifact
            .process_name_for_ref(&self.process_ref)
            .ok_or_else(|| ProcessDefinitionIdentityError::ArtifactMismatch {
                process: self.process_name.clone(),
            })?;
        artifact.process_type(process_name).ok_or_else(|| {
            ProcessDefinitionIdentityError::MissingSignature {
                process: self.process_name.clone(),
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProcessDefinitionIdentityError {
    #[error("definition must be a process definition value")]
    NotProcessValue,
    #[error("definition is missing {field}")]
    MissingField { field: &'static str },
    #[error("definition has invalid {field}: {message}")]
    InvalidField {
        field: &'static str,
        message: String,
    },
    #[error("process identity for `{process}` does not match the supplied artifact export")]
    ArtifactMismatch { process: String },
    #[error("artifact process `{process}` has no complete signature")]
    MissingSignature { process: String },
}

fn decode_field<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    field: &'static str,
) -> Result<T, ProcessDefinitionIdentityError> {
    serde_json::from_value(
        value
            .get(field)
            .cloned()
            .ok_or(ProcessDefinitionIdentityError::MissingField { field })?,
    )
    .map_err(|err| ProcessDefinitionIdentityError::InvalidField {
        field,
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_definition_identity_round_trips_process_value() {
        let identity = ProcessDefinitionIdentity::new(
            ModuleRef::new(&crate::ContentHash::new("mod")),
            HostRequirementsRef::new(&crate::ContentHash::new("host")),
            ProcessRef::new(crate::ContentHash::new("proc"), 7),
            "scan",
        );

        let decoded = ProcessDefinitionIdentity::from_process_value(&identity.to_process_value())
            .expect("process value should decode");

        assert_eq!(decoded.to_process_value(), identity.to_process_value());
        assert!(decoded.process_name.is_empty());
        let mut relabeled = identity.clone();
        relabeled.process_name = "another diagnostic name".into();
        assert_eq!(
            identity.draft().unwrap().id(),
            relabeled.draft().unwrap().id()
        );
        let mut legacy = identity.to_process_value();
        legacy["process_name"] = serde_json::json!("scan");
        assert!(ProcessDefinitionIdentity::from_process_value(&legacy).is_err());
    }
}
