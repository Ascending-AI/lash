use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artifact::{HostRequirements, HostRequirementsRef, ModuleArtifact, ModuleRef};
use crate::ast::{LabelMetadata, TypeExpr, format_type_expr};
use crate::identity::ProcessDefinitionIdentity;
use crate::linker::{ModuleInstanceCatalog, ResourceTypeCatalog, TriggerSourceBinding};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleIntrospection {
    pub module_ref: ModuleRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub host_requirements: HostRequirements,
    pub canonical_source: String,
    pub exported_processes: Vec<ProcessIntrospection>,
    pub required_module_instances: Vec<ModuleInstanceIntrospection>,
    pub required_resource_types: Vec<ResourceTypeIntrospection>,
    pub named_data_types: Vec<NamedDataTypeIntrospection>,
    pub value_constructors: Vec<ValueConstructorIntrospection>,
    pub trigger_source_requirements: Vec<TriggerSourceIntrospection>,
}

impl ModuleIntrospection {
    pub fn from_artifact(artifact: &ModuleArtifact) -> Result<Self, ModuleIntrospectionError> {
        let canonical_source = artifact
            .canonical_source()
            .map_err(ModuleIntrospectionError::CanonicalSource)?;
        let mut exported_processes = Vec::new();
        for process_name in artifact.exports.processes.keys() {
            let definition =
                ProcessDefinitionIdentity::from_artifact_export(artifact, process_name)
                    .ok_or_else(|| ModuleIntrospectionError::MissingProcess {
                        process_name: process_name.clone(),
                    })?;
            let process = artifact.canonical_ir.process(process_name).ok_or_else(|| {
                ModuleIntrospectionError::MissingProcess {
                    process_name: process_name.clone(),
                }
            })?;
            let canonical_process_source = artifact
                .canonical_process_source_by_name(process_name)
                .map_err(ModuleIntrospectionError::CanonicalSource)?
                .ok_or_else(|| ModuleIntrospectionError::MissingProcess {
                    process_name: process_name.clone(),
                })?;
            exported_processes.push(ProcessIntrospection {
                definition,
                label: process.label.clone(),
                params: process
                    .params
                    .iter()
                    .map(|param| ProcessInputIntrospection {
                        name: param.name.to_string(),
                        ty: TypeView::new(param.ty.clone()),
                    })
                    .collect(),
                signals: process
                    .signals
                    .iter()
                    .map(|signal| ProcessSignalIntrospection {
                        name: signal.name.to_string(),
                        ty: TypeView::new(signal.ty.clone()),
                    })
                    .collect(),
                return_type: process.return_ty.clone().map(TypeView::new),
                canonical_source: canonical_process_source,
            });
        }

        Ok(Self {
            module_ref: artifact.module_ref.clone(),
            host_requirements_ref: artifact.host_requirements_ref.clone(),
            host_requirements: artifact.host_requirements.clone(),
            canonical_source,
            exported_processes,
            required_module_instances: module_instances(artifact),
            required_resource_types: resource_types(artifact),
            named_data_types: artifact
                .host_requirements
                .resources
                .named_data_types()
                .map(|(name, data_type)| NamedDataTypeIntrospection {
                    name: name.to_string(),
                    ty: TypeView::new(data_type.ty().clone()),
                })
                .collect(),
            value_constructors: artifact
                .host_requirements
                .resources
                .value_constructors()
                .map(|(path, constructor)| ValueConstructorIntrospection {
                    path: constructor.path.clone(),
                    key: path.to_string(),
                    type_name: constructor.type_name.clone(),
                    input_type: TypeView::new(constructor.input_ty.clone()),
                    output_type: TypeView::new(constructor.output_ty.clone()),
                })
                .collect(),
            trigger_source_requirements: artifact
                .host_requirements
                .resources
                .trigger_sources()
                .map(trigger_source)
                .collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessIntrospection {
    pub definition: ProcessDefinitionIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<LabelMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ProcessInputIntrospection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<ProcessSignalIntrospection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_type: Option<TypeView>,
    pub canonical_source: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessInputIntrospection {
    pub name: String,
    pub ty: TypeView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSignalIntrospection {
    pub name: String,
    pub ty: TypeView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleInstanceIntrospection {
    pub path: Vec<String>,
    pub alias: String,
    pub resource_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<ModuleOperationIntrospection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleOperationIntrospection {
    pub operation: String,
    pub host_operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_type: Option<TypeView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_type: Option<TypeView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceTypeIntrospection {
    pub resource_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<ResourceOperationIntrospection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceOperationIntrospection {
    pub operation: String,
    pub input_type: TypeView,
    pub output_type: TypeView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedDataTypeIntrospection {
    pub name: String,
    pub ty: TypeView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValueConstructorIntrospection {
    pub key: String,
    pub path: Vec<String>,
    pub type_name: String,
    pub input_type: TypeView,
    pub output_type: TypeView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSourceIntrospection {
    pub source_type: String,
    pub event_type_name: String,
    pub event_type: TypeView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TypeView {
    pub ty: TypeExpr,
    pub display: String,
}

impl TypeView {
    pub fn new(ty: TypeExpr) -> Self {
        let display = format_type_expr(&ty);
        Self { ty, display }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ModuleIntrospectionError {
    #[error("failed to render canonical source: {0}")]
    CanonicalSource(#[from] crate::CanonicalSourceError),
    #[error("module artifact export is missing process `{process_name}`")]
    MissingProcess { process_name: String },
}

fn module_instances(artifact: &ModuleArtifact) -> Vec<ModuleInstanceIntrospection> {
    artifact
        .host_requirements
        .resources
        .module_instances()
        .map(|(_, module)| module_instance(module, artifact))
        .collect()
}

fn module_instance(
    module: &ModuleInstanceCatalog,
    artifact: &ModuleArtifact,
) -> ModuleInstanceIntrospection {
    let operations = module
        .operations
        .iter()
        .map(|(operation, binding)| {
            let resource_binding = artifact
                .host_requirements
                .resources
                .resource_types()
                .find(|(resource_type, _)| *resource_type == module.resource_type.as_str())
                .and_then(|(_, catalog)| catalog.operations.get(operation));
            ModuleOperationIntrospection {
                operation: operation.clone(),
                host_operation: binding.host_operation.clone(),
                input_type: resource_binding.map(|binding| TypeView::new(binding.input_ty.clone())),
                output_type: resource_binding
                    .map(|binding| TypeView::new(binding.output_ty.clone())),
            }
        })
        .collect();

    ModuleInstanceIntrospection {
        path: module.path.clone(),
        alias: module.alias.clone(),
        resource_type: module.resource_type.clone(),
        operations,
    }
}

fn resource_types(artifact: &ModuleArtifact) -> Vec<ResourceTypeIntrospection> {
    artifact
        .host_requirements
        .resources
        .resource_types()
        .map(resource_type)
        .collect()
}

fn resource_type(
    (resource_type, catalog): (&str, &ResourceTypeCatalog),
) -> ResourceTypeIntrospection {
    ResourceTypeIntrospection {
        resource_type: resource_type.to_string(),
        operations: catalog
            .operations
            .iter()
            .map(|(operation, binding)| ResourceOperationIntrospection {
                operation: operation.clone(),
                input_type: TypeView::new(binding.input_ty.clone()),
                output_type: TypeView::new(binding.output_ty.clone()),
            })
            .collect(),
    }
}

fn trigger_source(
    (source_type, binding): (&str, &TriggerSourceBinding),
) -> TriggerSourceIntrospection {
    TriggerSourceIntrospection {
        source_type: source_type.to_string(),
        event_type_name: binding.event_type_name().to_string(),
        event_type: TypeView::new(binding.event_ty().clone()),
    }
}

/// Collect the module call-paths (`module.operation`, dotted for nested
/// modules) that a parsed program references through receiver calls. This is
/// the "gather" step of deferred tool resolution: the host resolver is asked to
/// resolve any returned path that the link-time host environment does not
/// already provide.
pub fn referenced_module_call_paths(
    program: &crate::ast::Program,
) -> std::collections::BTreeSet<String> {
    let mut paths = std::collections::BTreeSet::new();
    collect_module_call_paths(&program.main, &mut paths);
    for declaration in &program.declarations {
        if let crate::ast::Declaration::Process(process) = declaration {
            collect_module_call_paths(&process.body, &mut paths);
        }
    }
    paths
}

fn collect_module_call_paths(
    expr: &crate::ast::Expr,
    paths: &mut std::collections::BTreeSet<String>,
) {
    if let crate::ast::Expr::ReceiverCall {
        receiver,
        operation,
        ..
    } = expr
        && let Some(module_path) = module_call_receiver_path(receiver)
    {
        paths.insert(format!("{}.{}", module_path.join("."), operation.as_str()));
    }
    for child in expr.children() {
        collect_module_call_paths(child, paths);
    }
}

fn module_call_receiver_path(expr: &crate::ast::Expr) -> Option<Vec<String>> {
    match expr {
        crate::ast::Expr::LabelAnnotated { expr, .. } => module_call_receiver_path(expr),
        crate::ast::Expr::ResourceRef(reference) if !reference.path.is_empty() => Some(
            reference
                .path
                .iter()
                .map(|segment| segment.as_str().to_string())
                .collect(),
        ),
        crate::ast::Expr::Variable(name) => Some(vec![name.as_str().to_string()]),
        crate::ast::Expr::Field { target, field } => {
            let mut path = module_call_receiver_path(target)?;
            path.push(field.as_str().to_string());
            Some(path)
        }
        _ => None,
    }
}
