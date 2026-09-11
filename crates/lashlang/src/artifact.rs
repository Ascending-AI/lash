use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[path = "artifact_hash_writer.rs"]
mod hash_writer;
use hash_writer::HashWriter;
#[path = "artifact_dialect.rs"]
mod dialect;
use dialect::module_ref;
#[path = "artifact_requirements.rs"]
mod requirements;
#[path = "artifact_write_helpers.rs"]
mod write_helpers;
use requirements::RequirementsCollector;
use write_helpers::{
    write_binary_op, write_label_metadata, write_resource_ref, write_unary_expr, write_unary_op,
};

use crate::ast::{
    AssignPathStep, BinaryOp, Declaration, Expr, LabelMetadata, ListComprehensionClause,
    ProcessDecl, Program, ResourceRefExpr, TypeExpr, UnaryOp,
};
use crate::linker::{
    LashlangAbilities, LashlangHostCatalog, LashlangLanguageFeatures, ResourceOperationBinding,
};

pub use lash_sansio::LASHLANG_SEMANTIC_HASH_VERSION;
pub const LASHLANG_COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const LASHLANG_VM_ABI_VERSION: &str = "lashlang-vm-abi-v7";

/// Durability tier established by the execution path's concrete store or host.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityTier {
    #[default]
    Inline,
    Durable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModuleRef(String);

impl ModuleRef {
    pub fn new(hash: &ContentHash) -> Self {
        Self(format!("lashlang:v2:blake3:{hash}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn hash_hex(&self) -> Option<&str> {
        self.0.strip_prefix("lashlang:v2:blake3:")
    }
}

impl std::fmt::Display for ModuleRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProcessRef {
    pub component: ContentHash,
    pub pos: u32,
}

impl ProcessRef {
    pub fn new(component: ContentHash, pos: u32) -> Self {
        Self { component, pos }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostRequirementsRef(String);

impl HostRequirementsRef {
    pub fn new(hash: &ContentHash) -> Self {
        Self(format!("lashlang-host-requirements:v2:blake3:{hash}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostRequirementsRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRequirements {
    #[serde(default)]
    pub resources: LashlangHostCatalog,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub globals: BTreeSet<String>,
    #[serde(default)]
    pub abilities: LashlangAbilities,
    #[serde(default)]
    pub language_features: LashlangLanguageFeatures,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleExports {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub processes: BTreeMap<String, ProcessRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleArtifact {
    pub module_ref: ModuleRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub host_requirements: HostRequirements,
    pub exports: ModuleExports,
    /// Never defaulted: a defaulted dialect lets a TypeScript artifact verify as Lashlang.
    pub compilation_dialect: crate::CompilationDialect,
    pub canonical_ir: Program,
}

impl ModuleArtifact {
    /// Builds a raw Lashlang artifact from already-complete program IR.
    ///
    /// Source programs whose process output is inferred must go through the
    /// linker; this builder refuses an incomplete exported signature.
    pub fn from_program(program: Program) -> Result<Self, ModuleArtifactError> {
        let canonical_ir = canonical_program_ir(program);
        let requirements = host_requirements_for_program(&canonical_ir);
        Self::from_canonical_ir_and_requirements(
            canonical_ir,
            requirements,
            crate::CompilationDialect::Lashlang,
        )
    }

    pub(crate) fn from_program_with_requirements_and_dialect(
        program: Program,
        requirements: HostRequirements,
        compilation_dialect: crate::CompilationDialect,
    ) -> Result<Self, ModuleArtifactError> {
        let canonical_ir = canonical_program_ir(program);
        Self::from_canonical_ir_and_requirements(canonical_ir, requirements, compilation_dialect)
    }

    fn from_canonical_ir_and_requirements(
        canonical_ir: Program,
        requirements: HostRequirements,
        compilation_dialect: crate::CompilationDialect,
    ) -> Result<Self, ModuleArtifactError> {
        crate::ast::validate_ast(&canonical_ir)?;
        if let Some(process) = canonical_ir.declarations.iter().find_map(|declaration| {
            let Declaration::Process(process) = declaration else {
                return None;
            };
            process.return_ty.is_none().then_some(process)
        }) {
            return Err(ModuleArtifactError::IncompleteProcessSignature {
                process: process.name.to_string(),
            });
        }
        let host_requirements_ref = host_requirements_ref(&requirements);
        let exports = module_exports(&canonical_ir);
        let module_ref = module_ref(
            &canonical_ir,
            &host_requirements_ref,
            &exports,
            compilation_dialect,
        );
        Ok(Self {
            module_ref,
            host_requirements_ref,
            host_requirements: requirements,
            exports,
            compilation_dialect,
            canonical_ir,
        })
    }

    pub fn process_ref(&self, process_name: &str) -> Option<&ProcessRef> {
        self.exports.processes.get(process_name)
    }

    pub fn process_name_for_ref(&self, process_ref: &ProcessRef) -> Option<&str> {
        self.exports
            .processes
            .iter()
            .find_map(|(name, candidate)| (candidate == process_ref).then_some(name.as_str()))
    }

    /// Resolves aliases and host named-data references using this artifact's
    /// immutable requirements snapshot.
    pub fn resolve_type(&self, ty: &TypeExpr) -> TypeExpr {
        let aliases = self
            .canonical_ir
            .declarations
            .iter()
            .filter_map(|declaration| match declaration {
                Declaration::Type(declaration) => {
                    Some((declaration.name.to_string(), declaration.ty.clone()))
                }
                Declaration::Process(_) | Declaration::Function(_) => None,
            })
            .collect::<BTreeMap<_, _>>();
        resolve_artifact_type(
            ty,
            &aliases,
            &self.host_requirements.resources,
            &mut BTreeSet::new(),
        )
    }

    /// Returns the complete, resolved signature named by one artifact export.
    pub fn process_type(&self, process_name: &str) -> Option<TypeExpr> {
        let process = self.canonical_ir.process(process_name)?;
        let output = process.return_ty.as_ref()?;
        let signature = crate::ProcessSignature::try_new(
            process
                .params
                .iter()
                .map(|param| crate::ProcessParam {
                    name: param.name.clone(),
                    ty: self.resolve_type(&param.ty),
                })
                .collect(),
            self.resolve_type(output),
        )
        .ok()?;
        Some(TypeExpr::Process(crate::ProcessType::known(signature)))
    }

    /// Render compile-equivalent Lashlang source from canonical IR and requirements.
    pub fn canonical_source(&self) -> Result<String, crate::CanonicalSourceError> {
        crate::canonical_program_source_with_requirements(
            &self.canonical_ir,
            &self.host_requirements,
        )
    }

    /// Render one exported process by ref, or `None` when it is absent.
    pub fn canonical_process_source(
        &self,
        process_ref: &ProcessRef,
    ) -> Result<Option<String>, crate::CanonicalSourceError> {
        let Some(process_name) = self.process_name_for_ref(process_ref) else {
            return Ok(None);
        };
        self.canonical_process_source_by_name(process_name)
    }

    /// Pretty-print a focused process definition by exported process name.
    ///
    /// Returns `Ok(None)` when the process is not declared by this artifact.
    pub fn canonical_process_source_by_name(
        &self,
        process_name: &str,
    ) -> Result<Option<String>, crate::CanonicalSourceError> {
        let Some(process) = self.canonical_ir.process(process_name) else {
            return Ok(None);
        };
        crate::canonical_process_source_with_requirements(process, &self.host_requirements)
            .map(Some)
    }

    pub fn introspect(
        &self,
    ) -> Result<crate::ModuleIntrospection, crate::ModuleIntrospectionError> {
        crate::ModuleIntrospection::from_artifact(self)
    }

    pub fn verify(&self) -> Result<(), ModuleArtifactError> {
        let rebuilt = Self::from_program_with_requirements_and_dialect(
            self.canonical_ir.clone(),
            self.host_requirements.clone(),
            self.compilation_dialect,
        )?;
        if rebuilt.module_ref != self.module_ref {
            return Err(ModuleArtifactError::HashMismatch {
                field: "module_ref",
                expected: rebuilt.module_ref.to_string(),
                actual: self.module_ref.to_string(),
            });
        }
        if rebuilt.host_requirements_ref != self.host_requirements_ref {
            return Err(ModuleArtifactError::HashMismatch {
                field: "host_requirements_ref",
                expected: rebuilt.host_requirements_ref.to_string(),
                actual: self.host_requirements_ref.to_string(),
            });
        }
        if rebuilt.exports != self.exports {
            return Err(ModuleArtifactError::HashMismatch {
                field: "exports",
                expected: "canonical exports".to_string(),
                actual: "artifact exports".to_string(),
            });
        }
        Ok(())
    }

    pub fn to_store_bytes(&self) -> Result<Vec<u8>, ModuleArtifactError> {
        self.verify()?;
        serde_json::to_vec(self).map_err(|err| ModuleArtifactError::Codec(err.to_string()))
    }

    pub fn from_store_bytes(bytes: &[u8]) -> Result<Self, ModuleArtifactError> {
        let raw: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|err| ModuleArtifactError::Codec(err.to_string()))?;
        let known_dialect = matches!(
            raw.get("compilation_dialect")
                .and_then(|value| value.as_str()),
            Some("lashlang" | "typescript")
        );
        reject_future_shape(&raw)?;
        let artifact: Self = serde_json::from_slice(bytes).map_err(|err| {
            let message = err.to_string();
            if known_dialect && message.contains("unknown variant") {
                ModuleArtifactError::FutureShape {
                    field: "artifact shape",
                    value: "nested enum variant".to_string(),
                }
            } else {
                ModuleArtifactError::Codec(message)
            }
        })?;
        artifact.verify()?;
        Ok(artifact)
    }
}

fn resolve_artifact_type(
    ty: &TypeExpr,
    aliases: &BTreeMap<String, TypeExpr>,
    resources: &LashlangHostCatalog,
    seen: &mut BTreeSet<String>,
) -> TypeExpr {
    match ty {
        TypeExpr::Ref(name) if seen.insert(name.to_string()) => {
            let resolved = if let Some(ty) = aliases.get(name.as_str()) {
                resolve_artifact_type(ty, aliases, resources, seen)
            } else if let Some(data_type) = resources.resolve_named_data_type(name.as_str()) {
                resolve_artifact_type(data_type.ty(), aliases, resources, seen)
            } else {
                ty.clone()
            };
            seen.remove(name.as_str());
            resolved
        }
        TypeExpr::List(item) => TypeExpr::List(Box::new(resolve_artifact_type(
            item, aliases, resources, seen,
        ))),
        TypeExpr::Object(fields) => TypeExpr::Object(
            fields
                .iter()
                .map(|field| crate::TypeField {
                    name: field.name.clone(),
                    ty: resolve_artifact_type(&field.ty, aliases, resources, seen),
                    optional: field.optional,
                })
                .collect(),
        ),
        TypeExpr::Union(items) => TypeExpr::Union(
            items
                .iter()
                .map(|item| resolve_artifact_type(item, aliases, resources, seen))
                .collect(),
        ),
        TypeExpr::Process(process) => match process.as_signature() {
            None => ty.clone(),
            Some(signature) => TypeExpr::Process(crate::ProcessType::known(
                crate::ProcessSignature::try_new(
                    signature
                        .params()
                        .iter()
                        .map(|param| crate::ProcessParam {
                            name: param.name.clone(),
                            ty: resolve_artifact_type(&param.ty, aliases, resources, seen),
                        })
                        .collect(),
                    resolve_artifact_type(signature.output(), aliases, resources, seen),
                )
                .expect("resolved checked process signature remains valid"),
            )),
        },
        TypeExpr::TriggerHandle(event) => TypeExpr::TriggerHandle(Box::new(resolve_artifact_type(
            event, aliases, resources, seen,
        ))),
        _ => ty.clone(),
    }
}

/// Refuse a known-extensible top-level shape before typed Serde reaches a
/// future enum variant nested in the artifact. Module artifacts carry no
/// version envelope: their module ref is the identity fence, and a host must
/// recompile and republish source when this build cannot read that identity.
fn reject_future_shape(raw: &serde_json::Value) -> Result<(), ModuleArtifactError> {
    if contains_obsolete_process_type(raw) {
        return Err(ModuleArtifactError::ObsoleteProcessTypeShape);
    }
    if raw.get("trigger_key_manifest").is_some() {
        return Err(ModuleArtifactError::FutureShape {
            field: "trigger_key_manifest",
            value: "obsolete current-trigger manifest artifact field".to_string(),
        });
    }
    let Some(dialect) = raw
        .get("compilation_dialect")
        .and_then(|value| value.as_str())
    else {
        return Ok(());
    };
    if matches!(dialect, "lashlang" | "typescript") {
        return Ok(());
    }
    Err(ModuleArtifactError::FutureShape {
        field: "compilation_dialect",
        value: dialect.to_string(),
    })
}

fn contains_obsolete_process_type(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object
                .get("Process")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|process| {
                    process.contains_key("input") || process.contains_key("input_count")
                })
                || object.values().any(contains_obsolete_process_type)
        }
        serde_json::Value::Array(items) => items.iter().any(contains_obsolete_process_type),
        _ => false,
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleArtifactError {
    #[error("invalid canonical program: {0}")]
    InvalidAst(#[from] crate::InvalidAst),
    #[error(
        "module artifact uses the obsolete anonymous process type shape; recompile and republish the module"
    )]
    ObsoleteProcessTypeShape,
    #[error(
        "process `{process}` has no output type; link source to infer it before building an artifact"
    )]
    IncompleteProcessSignature { process: String },
    #[error("failed to encode module artifact: {0}")]
    Codec(String),
    #[error(
        "module artifact uses unsupported future shape `{field}` = `{value}`; \
         recompile and republish the module"
    )]
    FutureShape { field: &'static str, value: String },
    #[error("module artifact {field} mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArtifactStoreError {
    #[error("failed to encode lashlang artifact: {0}")]
    Encode(String),
    #[error("failed to decode lashlang artifact: {0}")]
    Decode(String),
    #[error("artifact store backend error: {0}")]
    Backend(String),
}

impl From<ModuleArtifactError> for ArtifactStoreError {
    fn from(value: ModuleArtifactError) -> Self {
        match value {
            ModuleArtifactError::InvalidAst(source) => Self::Decode(source.to_string()),
            ModuleArtifactError::ObsoleteProcessTypeShape => Self::Decode(
                "module artifact uses the obsolete anonymous process type shape; recompile and republish the module"
                    .to_string(),
            ),
            ModuleArtifactError::IncompleteProcessSignature { .. } => {
                Self::Decode(value.to_string())
            }
            ModuleArtifactError::Codec(message) => Self::Decode(message),
            ModuleArtifactError::FutureShape { .. } => Self::Decode(value.to_string()),
            ModuleArtifactError::HashMismatch { .. } => Self::Decode(value.to_string()),
        }
    }
}

#[async_trait::async_trait]
pub trait LashlangArtifactStore: Send + Sync {
    /// Durability tier this artifact store provides; defaults to [`DurabilityTier::Inline`].
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Inline
    }

    async fn put_module_artifact(
        &self,
        artifact: &ModuleArtifact,
    ) -> Result<(), ArtifactStoreError>;

    async fn get_module_artifact(
        &self,
        module_ref: &ModuleRef,
    ) -> Result<Option<Arc<ModuleArtifact>>, ArtifactStoreError>;

    async fn put_artifact_bytes(
        &self,
        artifact_ref: &str,
        descriptor: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError>;

    async fn get_artifact_bytes(
        &self,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError>;
}

#[derive(Clone, Default)]
pub struct InMemoryLashlangArtifactStore {
    modules: Arc<Mutex<BTreeMap<ModuleRef, Arc<ModuleArtifact>>>>,
    artifacts: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl InMemoryLashlangArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn global_in_memory_lashlang_artifact_store() -> Arc<InMemoryLashlangArtifactStore> {
    static STORE: OnceLock<Arc<InMemoryLashlangArtifactStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(InMemoryLashlangArtifactStore::new()))
        .clone()
}

#[async_trait::async_trait]
impl LashlangArtifactStore for InMemoryLashlangArtifactStore {
    async fn put_module_artifact(
        &self,
        artifact: &ModuleArtifact,
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact.module_ref.as_str()) {
            return Err(ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let mut modules = self.modules.lock_recover();
        modules.insert(artifact.module_ref.clone(), Arc::new(artifact.clone()));
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        module_ref: &ModuleRef,
    ) -> Result<Option<Arc<ModuleArtifact>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref.as_str()) {
            return Err(ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let modules = self.modules.lock_recover();
        Ok(modules.get(module_ref).cloned())
    }

    async fn put_artifact_bytes(
        &self,
        artifact_ref: &str,
        _descriptor: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact_ref) {
            return Err(ArtifactStoreError::Backend(
                "invalid artifact namespace key".into(),
            ));
        }
        self.artifacts
            .lock_recover()
            .insert(artifact_ref.to_string(), bytes.to_vec());
        Ok(())
    }

    async fn get_artifact_bytes(
        &self,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact_ref) {
            return Err(ArtifactStoreError::Backend(
                "invalid artifact namespace key".into(),
            ));
        }
        Ok(self.artifacts.lock_recover().get(artifact_ref).cloned())
    }
}

#[derive(Clone)]
pub(crate) struct CompiledModuleContext {
    pub(crate) module_ref: ModuleRef,
    pub(crate) host_requirements_ref: HostRequirementsRef,
    pub(crate) process_refs: BTreeMap<String, ProcessRef>,
}

impl From<&ModuleArtifact> for CompiledModuleContext {
    fn from(value: &ModuleArtifact) -> Self {
        Self {
            module_ref: value.module_ref.clone(),
            host_requirements_ref: value.host_requirements_ref.clone(),
            process_refs: value.exports.processes.clone(),
        }
    }
}

pub fn canonical_program_ir(mut program: Program) -> Program {
    program.declaration_spans.clear();
    program.expression_spans.clear();
    program.expression_source_spans.clear();
    program
}

pub fn host_requirements_for_program(program: &Program) -> HostRequirements {
    RequirementsCollector::new(program).collect()
}

pub(crate) fn host_requirements_for_program_with_catalog(
    program: &Program,
    catalog: &LashlangHostCatalog,
) -> HostRequirements {
    RequirementsCollector::new(program)
        .with_resource_catalog(catalog)
        .collect()
}

fn module_exports(program: &Program) -> ModuleExports {
    let mut exports = ModuleExports::default();
    let mut process_pos = 0u32;
    for declaration in &program.declarations {
        if let Declaration::Process(process) = declaration {
            exports.processes.insert(
                process.name.to_string(),
                ProcessRef::new(process_component_hash(process), process_pos),
            );
            process_pos += 1;
        }
    }
    exports
}

fn host_requirements_ref(requirements: &HostRequirements) -> HostRequirementsRef {
    let mut writer = HashWriter::new();
    writer.atom(LASHLANG_SEMANTIC_HASH_VERSION);
    writer.atom("host-requirements");
    write_host_requirements(&mut writer, requirements);
    HostRequirementsRef::new(&writer.finish())
}

fn process_component_hash(process: &ProcessDecl) -> ContentHash {
    let mut writer = HashWriter::new();
    writer.atom(LASHLANG_SEMANTIC_HASH_VERSION);
    writer.atom("process");
    write_process(&mut writer, process);
    writer.finish()
}

fn write_exports(writer: &mut HashWriter, exports: &ModuleExports) {
    writer.atom("exports");
    writer.usize(exports.processes.len());
    for (name, process_ref) in &exports.processes {
        writer.atom("process-export");
        writer.atom(name);
        writer.atom(process_ref.component.as_str());
        writer.u32(process_ref.pos);
    }
}

fn write_host_requirements(writer: &mut HashWriter, requirements: &HostRequirements) {
    writer.atom("abilities");
    writer.bool(requirements.abilities.processes);
    writer.bool(requirements.abilities.sleep);
    writer.bool(requirements.abilities.process_signals);
    writer.bool(requirements.abilities.triggers);
    if requirements.language_features.label_annotations {
        writer.atom("language-features");
        writer.atom("label-annotations");
    }
    if !requirements.globals.is_empty() {
        writer.atom("globals");
        writer.usize(requirements.globals.len());
        for name in &requirements.globals {
            writer.atom(name);
        }
    }
    writer.atom("resources");
    writer.atom("modules");
    writer.usize(requirements.resources.module_instances().count());
    for (module_path, module) in requirements.resources.module_instances() {
        writer.atom(module_path);
        writer.atom(&module.resource_type);
        writer.atom(&module.alias);
        writer.atom("operations");
        writer.usize(module.operations.len());
        for (operation, binding) in &module.operations {
            writer.atom(operation);
            writer.atom(&binding.host_operation);
        }
    }
    writer.usize(requirements.resources.resource_types().count());
    for (resource_type, catalog) in requirements.resources.resource_types() {
        writer.atom(resource_type);
        writer.atom("operations");
        writer.usize(catalog.operations.len());
        for (operation, binding) in &catalog.operations {
            writer.atom(operation);
            write_type(writer, &binding.input_ty);
            write_type(writer, &binding.output_ty);
            if let Some(output_from_input) = &binding.output_from_input {
                writer.atom("output-from-input");
                writer.atom(&output_from_input.input_field);
                if let Some(default_schema) = &output_from_input.default_schema {
                    writer.atom("default-schema");
                    write_type(writer, default_schema);
                }
            }
        }
    }
    writer.atom("named-data-types");
    writer.usize(requirements.resources.named_data_types().count());
    for (name, data_type) in requirements.resources.named_data_types() {
        writer.atom(name);
        write_type(writer, data_type.ty());
    }
    writer.atom("constructors");
    writer.usize(requirements.resources.value_constructors().count());
    for (path, constructor) in requirements.resources.value_constructors() {
        writer.atom(path);
        writer.atom(&constructor.type_name);
        write_type(writer, &constructor.input_ty);
        write_type(writer, &constructor.output_ty);
    }
    writer.atom("trigger-sources");
    writer.usize(requirements.resources.trigger_sources().count());
    for (source_ty, binding) in requirements.resources.trigger_sources() {
        writer.atom(source_ty);
        writer.atom(binding.event_type_name());
    }
}

fn write_program(writer: &mut HashWriter, program: &Program) {
    writer.atom("program");
    writer.usize(program.declarations.len());
    for declaration in &program.declarations {
        write_declaration(writer, declaration);
    }
    let mut normalizer = NameNormalizer::default();
    normalizer.collect_expr(&program.main);
    write_expr(writer, &program.main, &normalizer);
}

fn write_declaration(writer: &mut HashWriter, declaration: &Declaration) {
    match declaration {
        Declaration::Type(type_decl) => {
            writer.atom("type-decl");
            writer.atom(type_decl.name.as_str());
            write_type(writer, &type_decl.ty);
        }
        Declaration::Process(process) => write_process(writer, process),
        Declaration::Function(function) => write_function(writer, function),
    }
}

fn write_function(writer: &mut HashWriter, function: &crate::ast::FunctionDecl) {
    writer.atom("function-decl");
    writer.atom(function.name.as_str());
    writer.usize(function.params.len());
    for param in &function.params {
        writer.atom(param.name.as_str());
        write_type(writer, &param.ty);
    }
    writer.atom("return");
    write_type(writer, &function.return_ty);
    let mut normalizer = NameNormalizer::default();
    for param in &function.params {
        normalizer.bind_abi(param.name.as_str());
    }
    normalizer.collect_expr(&function.body);
    write_expr(writer, &function.body, &normalizer);
}

fn write_process(writer: &mut HashWriter, process: &ProcessDecl) {
    writer.atom("process-decl");
    writer.atom(process.name.as_str());
    writer.usize(process.params.len());
    for param in &process.params {
        writer.atom(param.name.as_str());
        write_type(writer, &param.ty);
    }
    writer.atom("signals");
    writer.usize(process.signals.len());
    for signal in &process.signals {
        writer.atom(signal.name.as_str());
        write_type(writer, &signal.ty);
    }
    match &process.return_ty {
        Some(ty) => {
            writer.atom("return");
            write_type(writer, ty);
        }
        None => writer.atom("no-return"),
    }
    if let Some(label) = &process.label {
        write_label_metadata(writer, label);
    }
    let mut normalizer = NameNormalizer::default();
    for param in &process.params {
        normalizer.bind_abi(param.name.as_str());
    }
    normalizer.bind_abi("input");
    normalizer.bind_abi("inputs");
    normalizer.collect_expr(&process.body);
    write_expr(writer, &process.body, &normalizer);
}

fn write_type(writer: &mut HashWriter, ty: &TypeExpr) {
    match ty {
        TypeExpr::Any => writer.atom("type:any"),
        TypeExpr::Str => writer.atom("type:str"),
        TypeExpr::Int => writer.atom("type:int"),
        TypeExpr::Float => writer.atom("type:float"),
        TypeExpr::Bool => writer.atom("type:bool"),
        TypeExpr::Dict => writer.atom("type:dict"),
        TypeExpr::Null => writer.atom("type:null"),
        TypeExpr::Enum(values) => {
            writer.atom("type:enum");
            writer.usize(values.len());
            for value in values {
                writer.atom(value.as_str());
            }
        }
        TypeExpr::List(item) => {
            writer.atom("type:list");
            write_type(writer, item);
        }
        TypeExpr::Object(fields) => {
            writer.atom("type:object");
            writer.usize(fields.len());
            for field in fields {
                writer.atom(field.name.as_str());
                writer.bool(field.optional);
                write_type(writer, &field.ty);
            }
        }
        TypeExpr::Ref(name) => {
            writer.atom("type:ref");
            writer.atom(name.as_str());
        }
        TypeExpr::Process(process) => {
            let Some(signature) = process.as_signature() else {
                writer.atom("type:process-unknown");
                return;
            };
            writer.atom("type:process-signature");
            writer.usize(signature.arity());
            for param in signature.params() {
                writer.atom(param.name.as_str());
                write_type(writer, &param.ty);
            }
            write_type(writer, signature.output());
        }
        TypeExpr::TriggerHandle(event) => {
            writer.atom("type:trigger-handle");
            write_type(writer, event);
        }
        TypeExpr::Union(items) => {
            writer.atom("type:union");
            writer.usize(items.len());
            for item in items {
                write_type(writer, item);
            }
        }
    }
}

fn write_expr(writer: &mut HashWriter, expr: &Expr, normalizer: &NameNormalizer) {
    match expr {
        Expr::Block(expressions) => {
            writer.atom("block");
            writer.usize(expressions.len());
            for expression in expressions {
                write_expr(writer, expression, normalizer);
            }
        }
        Expr::LabelAnnotated { label, expr } => {
            writer.atom("label-annotated");
            write_label_metadata(writer, label);
            write_expr(writer, expr, normalizer);
        }
        Expr::Null => writer.atom("null"),
        Expr::Undefined => writer.atom("javascript:undefined"),
        Expr::Bool(value) => {
            writer.atom("bool");
            writer.bool(*value);
        }
        Expr::Number(value) => {
            writer.atom("number");
            writer.u64(if *value == 0.0 { 0 } else { value.to_bits() });
        }
        Expr::String(value) => {
            writer.atom("string");
            writer.atom(value.as_str());
        }
        Expr::Variable(name) => {
            writer.atom("variable");
            writer.atom(&normalizer.name_token(name.as_str()));
        }
        Expr::Tuple(items) => {
            writer.atom("tuple");
            writer.usize(items.len());
            for item in items {
                write_expr(writer, item, normalizer);
            }
        }
        Expr::List(items) => {
            writer.atom("list");
            writer.usize(items.len());
            for item in items {
                write_expr(writer, item, normalizer);
            }
        }
        Expr::ListComprehension { element, clauses } => {
            writer.atom("list-comprehension");
            writer.usize(clauses.len());
            for clause in clauses {
                match clause {
                    ListComprehensionClause::For { binding, iterable } => {
                        writer.atom("for");
                        writer.atom(&normalizer.name_token(binding.as_str()));
                        write_expr(writer, iterable, normalizer);
                    }
                    ListComprehensionClause::If { condition } => {
                        writer.atom("if");
                        write_expr(writer, condition, normalizer);
                    }
                }
            }
            write_expr(writer, element, normalizer);
        }
        Expr::Record(entries) => {
            writer.atom("record");
            writer.usize(entries.len());
            for (key, value) in entries {
                writer.atom(key.as_str());
                write_expr(writer, value, normalizer);
            }
        }
        Expr::Assign { target, expr } => {
            writer.atom("assign");
            writer.atom(&normalizer.name_token(target.root.as_str()));
            writer.usize(target.steps.len());
            for step in &target.steps {
                match step {
                    AssignPathStep::Field(field) => {
                        writer.atom("field");
                        writer.atom(field.as_str());
                    }
                    AssignPathStep::Index(index) => {
                        writer.atom("index");
                        write_expr(writer, index, normalizer);
                    }
                }
            }
            write_expr(writer, expr, normalizer);
        }
        Expr::If {
            condition,
            then_block,
            else_block,
        } => {
            writer.atom("if");
            write_expr(writer, condition, normalizer);
            write_expr(writer, then_block, normalizer);
            write_expr(writer, else_block, normalizer);
        }
        Expr::For {
            binding,
            iterable,
            body,
        } => {
            writer.atom("for");
            writer.atom(&normalizer.name_token(binding.as_str()));
            write_expr(writer, iterable, normalizer);
            write_expr(writer, body, normalizer);
        }
        Expr::While { condition, body } => {
            writer.atom("while");
            write_expr(writer, condition, normalizer);
            write_expr(writer, body, normalizer);
        }
        Expr::Break => writer.atom("break"),
        Expr::Continue => writer.atom("continue"),
        Expr::StartProcess(start) => {
            writer.atom("start-process");
            writer.atom(start.process.as_str());
            writer.usize(start.args.len());
            for (key, value) in &start.args {
                writer.atom(key.as_str());
                write_expr(writer, value, normalizer);
            }
        }
        Expr::ProcessRef { process } => {
            writer.atom("process-ref");
            writer.atom(process.as_str());
        }
        Expr::HostDescriptorConstructor { type_name, input } => {
            writer.atom("host-value-constructor");
            writer.atom(type_name.as_str());
            write_expr(writer, input, normalizer);
        }
        Expr::ResourceRef(resource) => {
            writer.atom("resource-ref");
            write_resource_ref(writer, resource);
        }
        Expr::ReceiverCall {
            receiver,
            operation,
            args,
        } => {
            writer.atom("receiver-call");
            write_expr(writer, receiver, normalizer);
            writer.atom(operation.as_str());
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg, normalizer);
            }
        }
        Expr::Await(expr) => write_unary_expr(writer, "await", expr, normalizer),
        Expr::SleepFor(expr) => write_unary_expr(writer, "sleep-for", expr, normalizer),
        Expr::SleepUntil(expr) => write_unary_expr(writer, "sleep-until", expr, normalizer),
        Expr::WaitSignal { name } => {
            writer.atom("wait-signal");
            writer.atom(name.as_str());
        }
        Expr::SignalRun { run, name, payload } => {
            writer.atom("signal-run");
            writer.atom(name.as_str());
            write_expr(writer, run, normalizer);
            write_expr(writer, payload, normalizer);
        }
        Expr::ResultUnwrap(expr) => write_unary_expr(writer, "unwrap", expr, normalizer),
        Expr::Cancel(expr) => write_unary_expr(writer, "cancel", expr, normalizer),
        Expr::Print(expr) => write_unary_expr(writer, "print", expr, normalizer),
        Expr::Yield(expr) => write_unary_expr(writer, "yield", expr, normalizer),
        Expr::Wake(expr) => write_unary_expr(writer, "wake", expr, normalizer),
        Expr::Finish(expr) => write_unary_expr(writer, "finish", expr, normalizer),
        Expr::Fail(expr) => write_unary_expr(writer, "fail", expr, normalizer),
        Expr::BuiltinCall { name, args } => {
            writer.atom("builtin-call");
            writer.atom(name.as_str());
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg, normalizer);
            }
        }
        Expr::FunctionCall { function, args } => {
            writer.atom("declared-function-call");
            writer.atom(function.as_str());
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg, normalizer);
            }
        }
        Expr::Function(function) => {
            writer.atom("function");
            match &function.name {
                Some(name) => writer.atom(&normalizer.name_token(name.as_str())),
                None => writer.atom("anonymous"),
            }
            writer.usize(function.params.len());
            for param in &function.params {
                writer.atom(&normalizer.name_token(param.as_str()));
            }
            writer.usize(function.captures.len());
            for capture in &function.captures {
                writer.atom(&normalizer.name_token(capture.as_str()));
            }
            write_expr(writer, &function.body, normalizer);
        }
        Expr::Call { function, args } => {
            writer.atom("function-call");
            write_expr(writer, function, normalizer);
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg, normalizer);
            }
        }
        Expr::Map { items, function } => {
            writer.atom("function-map");
            write_expr(writer, items, normalizer);
            write_expr(writer, function, normalizer);
        }
        Expr::Try(scope) => {
            writer.atom("try");
            write_expr(writer, &scope.body, normalizer);
            match &scope.catch {
                Some(catch) => {
                    writer.atom("catch");
                    writer.atom(&normalizer.name_token(catch.binding.as_str()));
                    write_expr(writer, &catch.body, normalizer);
                }
                None => writer.atom("no-catch"),
            }
            match &scope.finally {
                Some(finally) => {
                    writer.atom("finally");
                    write_expr(writer, finally, normalizer);
                }
                None => writer.atom("no-finally"),
            }
        }
        Expr::Throw(value) => write_unary_expr(writer, "throw", value, normalizer),
        Expr::Return(value) => write_unary_expr(writer, "javascript:return", value, normalizer),
        Expr::Field { target, field } => {
            writer.atom("field-access");
            write_expr(writer, target, normalizer);
            writer.atom(field.as_str());
        }
        Expr::Index { target, index } => {
            writer.atom("index-access");
            write_expr(writer, target, normalizer);
            write_expr(writer, index, normalizer);
        }
        Expr::Unary { op, expr } => {
            writer.atom("unary");
            write_unary_op(writer, *op);
            write_expr(writer, expr, normalizer);
        }
        Expr::Binary { left, op, right } => {
            writer.atom("binary");
            write_binary_op(writer, *op);
            write_expr(writer, left, normalizer);
            write_expr(writer, right, normalizer);
        }
        Expr::JavaScriptUnary { op, expr } => {
            writer.atom("javascript:unary");
            writer.atom(&format!("{op:?}"));
            write_expr(writer, expr, normalizer);
        }
        Expr::JavaScriptBinary { left, op, right } => {
            writer.atom("javascript:binary");
            writer.atom(&format!("{op:?}"));
            write_expr(writer, left, normalizer);
            write_expr(writer, right, normalizer);
        }
        Expr::JavaScriptLogical { left, op, right } => {
            writer.atom("javascript:logical");
            writer.atom(&format!("{op:?}"));
            write_expr(writer, left, normalizer);
            write_expr(writer, right, normalizer);
        }
        Expr::TypeLiteral(ty) => {
            writer.atom("type-literal");
            write_type(writer, ty);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_typed_artifact(param_name: &str) -> ModuleArtifact {
        ModuleArtifact::from_program(
            crate::parse(&format!(
                "process target({param_name}: str) -> bool {{ finish true }}\nprocess install(handler: Process<({param_name}: str), bool>) -> bool {{ finish true }}"
            ))
            .expect("named process signature parses"),
        )
        .expect("artifact builds")
    }

    #[test]
    fn named_process_signature_round_trips_and_names_change_identity() {
        let event = process_typed_artifact("event");
        let payload = process_typed_artifact("payload");
        let bytes = event.to_store_bytes().expect("artifact encodes");
        let decoded = ModuleArtifact::from_store_bytes(&bytes).expect("artifact decodes");

        assert_eq!(decoded, event);
        assert_ne!(event.module_ref, payload.module_ref);
        assert_ne!(event.process_ref("target"), payload.process_ref("target"));
    }

    #[test]
    fn artifact_explicitly_refuses_obsolete_process_type_shape() {
        let artifact = process_typed_artifact("event");
        let mut raw = serde_json::to_value(&artifact).expect("artifact serializes");
        let declarations = raw["canonical_ir"]["declarations"]
            .as_array_mut()
            .expect("declarations array");
        let install = declarations
            .iter_mut()
            .find(|declaration| declaration["Process"]["name"] == "install")
            .expect("install declaration");
        install["Process"]["params"][0]["ty"] = serde_json::json!({
            "Process": {"input": "Str", "output": "Bool", "input_count": 1}
        });

        let error = ModuleArtifact::from_store_bytes(&serde_json::to_vec(&raw).unwrap())
            .expect_err("old process type must be refused");
        assert!(matches!(
            error,
            ModuleArtifactError::ObsoleteProcessTypeShape
        ));
    }

    #[test]
    fn artifact_decoder_refuses_duplicate_signature_fields_and_parameter_extras() {
        let bytes = process_typed_artifact("event")
            .to_store_bytes()
            .expect("artifact encodes");
        let source = String::from_utf8(bytes).expect("artifact encoding is JSON");
        let canonical =
            r#""Process":{"kind":"known","params":[{"name":"event","ty":"Str"}],"output":"Bool"}"#;
        assert_eq!(source.matches(canonical).count(), 1);
        let cases = [
            (
                "duplicate kind",
                r#""Process":{"kind":"unknown","kind":"known","params":[{"name":"event","ty":"Str"}],"output":"Bool"}"#,
            ),
            (
                "duplicate params",
                r#""Process":{"kind":"known","params":[],"params":[{"name":"event","ty":"Str"}],"output":"Bool"}"#,
            ),
            (
                "duplicate output",
                r#""Process":{"kind":"known","params":[{"name":"event","ty":"Str"}],"output":"Str","output":"Bool"}"#,
            ),
            (
                "unknown parameter field",
                r#""Process":{"kind":"known","params":[{"name":"event","ty":"Str","extra":true}],"output":"Bool"}"#,
            ),
        ];

        for (description, replacement) in cases {
            let malformed = source.replacen(canonical, replacement, 1);
            let error = ModuleArtifact::from_store_bytes(malformed.as_bytes())
                .expect_err("malformed signature bytes must be refused");
            assert!(
                matches!(error, ModuleArtifactError::Codec(_)),
                "{description}: {error}"
            );
        }
    }

    #[test]
    fn raw_artifact_builder_refuses_an_incomplete_process_output() {
        let program = crate::parse("process plain(message: str) { finish true }")
            .expect("unannotated process source parses");
        let error = ModuleArtifact::from_program(program)
            .expect_err("raw artifact IR must carry a complete process output");
        assert!(matches!(
            error,
            ModuleArtifactError::IncompleteProcessSignature { ref process }
                if process == "plain"
        ));
    }

    #[test]
    fn artifact_with_obsolete_trigger_manifest_field_is_explicitly_rejected() {
        let error = ModuleArtifact::from_store_bytes(
            include_str!("../tests/fixtures/module-artifact-old.json").as_bytes(),
        )
        .expect_err("an artifact carrying current-trigger manifest state must be refused");
        assert!(matches!(error, ModuleArtifactError::FutureShape { .. }));
        assert!(error.to_string().contains("trigger_key_manifest"));
    }

    #[test]
    fn frozen_sha256_artifact_without_the_obsolete_field_hits_the_identity_fence() {
        let mut raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/module-artifact-old.json"))
                .expect("frozen fixture should be JSON");
        raw.as_object_mut()
            .expect("artifact is an object")
            .remove("trigger_key_manifest");
        raw["canonical_ir"]["declarations"][0]["Process"]["return_ty"] = serde_json::json!("Str");
        let error = ModuleArtifact::from_store_bytes(
            &serde_json::to_vec(&raw).expect("legacy artifact should encode"),
        )
        .expect_err("a SHA-256 artifact must not verify under the BLAKE3 generation");
        assert!(matches!(error, ModuleArtifactError::HashMismatch { .. }));
        assert!(error.to_string().contains("lashlang:v2:blake3:"));
    }

    #[test]
    fn future_shape_refuses_before_serde_reaches_unknown_variants() {
        let mut raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/module-artifact-old.json"))
                .expect("frozen fixture should be JSON");
        raw["compilation_dialect"] = serde_json::json!("future_dialect");
        raw["canonical_ir"]["main"] = serde_json::json!({"FutureExpr": null});

        let error = ModuleArtifact::from_store_bytes(
            &serde_json::to_vec(&raw).expect("future fixture should encode"),
        )
        .expect_err("a future artifact shape must be refused");
        assert!(matches!(error, ModuleArtifactError::FutureShape { .. }));
        let message = error.to_string();
        assert!(message.contains("recompile and republish"), "{message}");
        assert!(!message.contains("unknown variant"), "{message}");
    }

    #[test]
    fn unchanged_dialect_with_unknown_nested_variant_is_a_future_shape_refusal() {
        let mut raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/module-artifact-old.json"))
                .expect("frozen fixture should be JSON");
        raw["canonical_ir"]["main"] = serde_json::json!({"FutureExpr": null});

        let error = ModuleArtifact::from_store_bytes(
            &serde_json::to_vec(&raw).expect("future fixture should encode"),
        )
        .expect_err("a known-dialect future variant must be refused legibly");
        assert!(matches!(error, ModuleArtifactError::FutureShape { .. }));
        let message = error.to_string();
        assert!(message.contains("recompile and republish"), "{message}");
        assert!(!message.contains("unknown variant"), "{message}");
    }

    #[test]
    fn malformed_artifact_json_remains_an_undecodable_codec_error() {
        let error = ModuleArtifact::from_store_bytes(br#"{"#)
            .expect_err("malformed JSON must remain undecodable");
        assert!(matches!(error, ModuleArtifactError::Codec(_)));
        assert!(!matches!(error, ModuleArtifactError::FutureShape { .. }));
    }
}

#[derive(Default)]
struct NameNormalizer {
    names: BTreeMap<String, String>,
    abi_names: BTreeSet<String>,
    next_local: u32,
}

impl NameNormalizer {
    fn bind_abi(&mut self, name: &str) {
        self.abi_names.insert(name.to_string());
        self.names.insert(name.to_string(), format!("abi:{name}"));
    }

    fn bind_local(&mut self, name: &str) {
        if self.abi_names.contains(name) || self.names.contains_key(name) {
            return;
        }
        let token = format!("local:{}", self.next_local);
        self.next_local += 1;
        self.names.insert(name.to_string(), token);
    }

    fn name_token(&self, name: &str) -> String {
        self.names
            .get(name)
            .cloned()
            .unwrap_or_else(|| format!("global:{name}"))
    }

    fn collect_expr(&mut self, expr: &Expr) {
        // Local binders are the only nodes that carry naming semantics; every
        // other node just feeds its sub-expressions back through `collect_expr`,
        // so the generic arm folds over `Expr::children()`. `Assign` and `For`
        // stay explicit because they must register their binder name in the
        // same order the original full walk did.
        match expr {
            Expr::Assign { target, expr } => {
                self.bind_local(target.root.as_str());
                for step in &target.steps {
                    if let AssignPathStep::Index(index) = step {
                        self.collect_expr(index);
                    }
                }
                self.collect_expr(expr);
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                self.collect_expr(iterable);
                self.bind_local(binding.as_str());
                self.collect_expr(body);
            }
            Expr::ListComprehension { element, clauses } => {
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { binding, iterable } => {
                            self.collect_expr(iterable);
                            self.bind_local(binding.as_str());
                        }
                        ListComprehensionClause::If { condition } => {
                            self.collect_expr(condition);
                        }
                    }
                }
                self.collect_expr(element);
            }
            _ => {
                for child in expr.children() {
                    self.collect_expr(child);
                }
            }
        }
    }
}
