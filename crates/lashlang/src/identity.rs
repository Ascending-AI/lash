use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artifact::{HostRequirementsRef, ModuleArtifact, ModuleRef, ProcessRef};
use crate::runtime::{
    LASH_HOST_REQUIREMENTS_REF_KEY, LASH_MODULE_REF_KEY, LASH_PROCESS_NAME_KEY,
    LASH_PROCESS_REF_KEY, LASH_PROCESS_VALUE_KEY,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessDefinitionIdentity {
    pub module_ref: ModuleRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub process_ref: ProcessRef,
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

    pub fn from_process_value(
        value: &serde_json::Value,
    ) -> Result<Self, ProcessDefinitionIdentityError> {
        if value
            .get(LASH_PROCESS_VALUE_KEY)
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(ProcessDefinitionIdentityError::NotProcessValue);
        }
        Ok(Self {
            module_ref: decode_field(value, LASH_MODULE_REF_KEY)?,
            host_requirements_ref: decode_field(value, LASH_HOST_REQUIREMENTS_REF_KEY)?,
            process_ref: decode_field(value, LASH_PROCESS_REF_KEY)?,
            process_name: value
                .get(LASH_PROCESS_NAME_KEY)
                .and_then(serde_json::Value::as_str)
                .ok_or(ProcessDefinitionIdentityError::MissingField {
                    field: LASH_PROCESS_NAME_KEY,
                })?
                .to_string(),
        })
    }

    pub fn to_process_value(&self) -> serde_json::Value {
        serde_json::json!({
            LASH_PROCESS_VALUE_KEY: true,
            LASH_MODULE_REF_KEY: self.module_ref,
            LASH_HOST_REQUIREMENTS_REF_KEY: self.host_requirements_ref,
            LASH_PROCESS_REF_KEY: self.process_ref,
            LASH_PROCESS_NAME_KEY: self.process_name,
        })
    }

    pub fn from_artifact_export(artifact: &ModuleArtifact, process_name: &str) -> Option<Self> {
        let process_ref = artifact.process_ref(process_name)?.clone();
        Some(Self::new(
            artifact.module_ref.clone(),
            artifact.host_requirements_ref.clone(),
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
            && self.process_name == process_name
    }

    pub fn matches_artifact_export(&self, artifact: &ModuleArtifact) -> bool {
        if self.module_ref != artifact.module_ref
            || self.host_requirements_ref != artifact.host_requirements_ref
        {
            return false;
        }
        artifact
            .process_name_for_ref(&self.process_ref)
            .is_some_and(|export_name| export_name == self.process_name)
    }

    /// Resolves this immutable identity to the complete signature stored by its artifact.
    pub fn resolve_process_type(
        &self,
        artifact: &ModuleArtifact,
    ) -> Result<crate::TypeExpr, ProcessDefinitionIdentityError> {
        artifact
            .verify()
            .map_err(|source| ProcessDefinitionIdentityError::InvalidArtifact {
                process: self.process_name.clone(),
                message: source.to_string(),
            })?;
        if !self.matches_artifact_export(artifact) {
            return Err(ProcessDefinitionIdentityError::ArtifactMismatch {
                process: self.process_name.clone(),
            });
        }
        artifact.process_type(&self.process_name).ok_or_else(|| {
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
    #[error("artifact for process `{process}` failed identity verification: {message}")]
    InvalidArtifact { process: String, message: String },
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

        assert_eq!(decoded, identity);
    }
}
