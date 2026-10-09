//! Worker-verified artifacts: opaque executable bytes and bounded host metadata.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessMetadata {
    pub lifted: bool,
    pub params: BTreeMap<String, lash_vm::TypeExpr>,
    pub process_type: Option<lash_vm::TypeExpr>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedArtifact {
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    pub module_ref: lash_vm::ModuleRef,
    pub host_requirements_ref: lash_vm::HostRequirementsRef,
    pub host_requirements: lash_vm::HostRequirements,
    pub exports: lash_vm::ModuleExports,
    pub source_identity: String,
    pub processes: BTreeMap<String, ProcessMetadata>,
    pub graph: lash_vm::WorkflowGraph,
}
impl InspectedArtifact {
    pub fn module_ref(&self) -> &lash_vm::ModuleRef {
        &self.module_ref
    }
    pub fn host_requirements_ref(&self) -> &lash_vm::HostRequirementsRef {
        &self.host_requirements_ref
    }
    pub fn host_requirements(&self) -> &lash_vm::HostRequirements {
        &self.host_requirements
    }
    pub fn exports(&self) -> &lash_vm::ModuleExports {
        &self.exports
    }
    pub fn source_identity(&self) -> String {
        self.source_identity.clone()
    }
    pub fn process_ref(&self, name: &str) -> Option<&lash_vm::ProcessRef> {
        self.exports.processes.get(name)
    }
    pub fn process_name_for_ref(&self, process_ref: &lash_vm::ProcessRef) -> Option<&str> {
        self.exports
            .processes
            .iter()
            .find_map(|(name, reference)| (reference == process_ref).then_some(name.as_str()))
    }
    pub fn process(&self, name: &str) -> Option<&ProcessMetadata> {
        self.processes.get(name)
    }
    pub fn definition_identity(&self, name: &str) -> Option<lash_vm::ProcessDefinitionIdentity> {
        Some(lash_vm::ProcessDefinitionIdentity {
            module_ref: self.module_ref.clone(),
            host_requirements_ref: self.host_requirements_ref.clone(),
            process_ref: self.process_ref(name)?.clone(),
            process_name: name.to_owned(),
        })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn process_type(
        &self,
        identity: &lash_vm::ProcessDefinitionIdentity,
    ) -> Result<lash_vm::TypeExpr, String> {
        let name = self
            .process_name_for_ref(&identity.process_ref)
            .ok_or_else(|| "process definition has no worker-verified export".to_string())?;
        if identity.module_ref != self.module_ref
            || identity.host_requirements_ref != self.host_requirements_ref
            || (!identity.process_name.is_empty() && identity.process_name != name)
        {
            return Err("process definition differs from its worker-verified export".into());
        }
        self.process(name)
            .and_then(|process| process.process_type.clone())
            .ok_or_else(|| "process export has no complete signature".into())
    }
}
/// A workflow document a worker admitted against a host environment: the
/// artifact the linker derived from it, whose `graph` is the admitted
/// document, and the admitted id of each node and process container of the
/// submitted one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedDocument {
    pub artifact: InspectedArtifact,
    pub nodes: BTreeMap<lash_vm::WorkflowNodeId, lash_vm::WorkflowNodeId>,
}
impl lash_vm::ModuleArtifactBytes for InspectedArtifact {
    fn artifact_ref(&self) -> &lash_vm::ModuleRef {
        &self.module_ref
    }
    fn encoded_artifact(&self) -> Result<Vec<u8>, lash_core_execution::ArtifactStoreError> {
        Ok(self.bytes.clone())
    }
}
