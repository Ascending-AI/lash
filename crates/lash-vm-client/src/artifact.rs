//! Worker-verified artifacts: opaque executable bytes and bounded host metadata.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessMetadata {
    pub lifted: bool,
    pub params: BTreeMap<String, lashlang::TypeExpr>,
    pub process_type: Option<lashlang::TypeExpr>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedArtifact {
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    pub module_ref: lashlang::ModuleRef,
    pub host_requirements_ref: lashlang::HostRequirementsRef,
    pub host_requirements: lashlang::HostRequirements,
    pub exports: lashlang::ModuleExports,
    pub source_identity: String,
    pub processes: BTreeMap<String, ProcessMetadata>,
    pub graph: lashlang::WorkflowGraph,
}
impl InspectedArtifact {
    pub fn module_ref(&self) -> &lashlang::ModuleRef {
        &self.module_ref
    }
    pub fn host_requirements_ref(&self) -> &lashlang::HostRequirementsRef {
        &self.host_requirements_ref
    }
    pub fn host_requirements(&self) -> &lashlang::HostRequirements {
        &self.host_requirements
    }
    pub fn exports(&self) -> &lashlang::ModuleExports {
        &self.exports
    }
    pub fn source_identity(&self) -> String {
        self.source_identity.clone()
    }
    pub fn process_ref(&self, name: &str) -> Option<&lashlang::ProcessRef> {
        self.exports.processes.get(name)
    }
    pub fn process_name_for_ref(&self, process_ref: &lashlang::ProcessRef) -> Option<&str> {
        self.exports
            .processes
            .iter()
            .find_map(|(name, reference)| (reference == process_ref).then_some(name.as_str()))
    }
    pub fn process(&self, name: &str) -> Option<&ProcessMetadata> {
        self.processes.get(name)
    }
    pub fn definition_identity(&self, name: &str) -> Option<lashlang::ProcessDefinitionIdentity> {
        Some(lashlang::ProcessDefinitionIdentity {
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
        identity: &lashlang::ProcessDefinitionIdentity,
    ) -> Result<lashlang::TypeExpr, String> {
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
/// A worker-verified artifact read as a document: its metadata, the graph of
/// the program it executes with spans into its canonical TypeScript, and that
/// text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedDocument {
    pub artifact: InspectedArtifact,
    pub graph: lashlang::WorkflowGraph,
    pub source: String,
}
impl lashlang::ModuleArtifactBytes for InspectedArtifact {
    fn artifact_ref(&self) -> &lashlang::ModuleRef {
        &self.module_ref
    }
    fn encoded_artifact(&self) -> Result<Vec<u8>, lash_core_execution::ArtifactStoreError> {
        Ok(self.bytes.clone())
    }
}
