use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ast::{
    AssignPathStep, BinaryOp, Declaration, Expr, LabelMetadata, ListComprehensionClause,
    ProcessDecl, Program, ResourceRefExpr, TypeExpr, UnaryOp,
};
use crate::linker::{
    LashlangAbilities, LashlangHostCatalog, LashlangLanguageFeatures, ResourceOperationBinding,
};
use crate::trigger_manifest::{
    CurrentTriggerKeyManifest, TriggerKeyManifest, TriggerManifestReplacement,
};

pub const LASHLANG_SEMANTIC_HASH_VERSION: &str = "lashlang-semantic-v2";
pub const LASHLANG_COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const LASHLANG_VM_ABI_VERSION: &str = "lashlang-vm-abi-v1";

/// Durability tier of an execution path's wired store or effect host.
///
/// Durability is a property established by what the host wired, not a mode
/// flag: each runtime trait reports the tier of the concrete implementation
/// behind it, and the runtime validates that wiring is internally consistent.
/// `Inline` covers in-memory / build-time wiring; `Durable` covers a
/// crash-recoverable store or effect host (e.g. Sqlite-backed persistence or a
/// Restate-backed effect host).
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
        Self(format!("lashlang:v1:sha256:{hash}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn hash_hex(&self) -> Option<&str> {
        self.0.strip_prefix("lashlang:v1:sha256:")
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
        Self(format!("lashlang-host-requirements:v1:sha256:{hash}"))
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
    #[serde(default)]
    pub trigger_key_manifest: TriggerKeyManifest,
    pub canonical_ir: Program,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<ModuleRef>,
}

impl ModuleArtifact {
    pub fn from_program(program: Program) -> Result<Self, ModuleArtifactError> {
        let canonical_ir = canonical_program_ir(program);
        let requirements = host_requirements_for_program(&canonical_ir);
        Self::from_canonical_ir_and_requirements(canonical_ir, requirements)
    }

    pub(crate) fn from_program_with_requirements(
        program: Program,
        requirements: HostRequirements,
    ) -> Result<Self, ModuleArtifactError> {
        let canonical_ir = canonical_program_ir(program);
        Self::from_canonical_ir_and_requirements(canonical_ir, requirements)
    }

    fn from_canonical_ir_and_requirements(
        canonical_ir: Program,
        requirements: HostRequirements,
    ) -> Result<Self, ModuleArtifactError> {
        let host_requirements_ref = host_requirements_ref(&requirements);
        let exports = module_exports(&canonical_ir);
        let trigger_key_manifest = TriggerKeyManifest::from_program(&canonical_ir);
        let module_ref = module_ref(&canonical_ir, &host_requirements_ref, &exports);
        Ok(Self {
            module_ref,
            host_requirements_ref,
            host_requirements: requirements,
            exports,
            trigger_key_manifest,
            canonical_ir,
            dependencies: Vec::new(),
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

    /// Pretty-print the saved canonical IR as Lashlang source.
    ///
    /// The returned source is generated from canonical IR and host
    /// requirements. It preserves compile-equivalent structure, not comments or
    /// original formatting.
    pub fn canonical_source(&self) -> Result<String, crate::CanonicalSourceError> {
        crate::canonical_program_source_with_requirements(
            &self.canonical_ir,
            &self.host_requirements,
        )
    }

    /// Pretty-print a focused process definition by process ref.
    ///
    /// Returns `Ok(None)` when the ref is not exported by this module artifact.
    /// The fragment is the process declaration itself; use
    /// [`Self::canonical_source`] when the host needs a complete module snippet.
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
        let rebuilt = Self::from_program_with_requirements(
            self.canonical_ir.clone(),
            self.host_requirements.clone(),
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
        if rebuilt.trigger_key_manifest != self.trigger_key_manifest {
            return Err(ModuleArtifactError::HashMismatch {
                field: "trigger_key_manifest",
                expected: "canonical trigger key manifest".to_string(),
                actual: "artifact trigger key manifest".to_string(),
            });
        }
        Ok(())
    }

    pub fn to_store_bytes(&self) -> Result<Vec<u8>, ModuleArtifactError> {
        self.verify()?;
        serde_json::to_vec(self).map_err(|err| ModuleArtifactError::Codec(err.to_string()))
    }

    pub fn from_store_bytes(bytes: &[u8]) -> Result<Self, ModuleArtifactError> {
        let artifact: Self = serde_json::from_slice(bytes)
            .map_err(|err| ModuleArtifactError::Codec(err.to_string()))?;
        artifact.verify()?;
        Ok(artifact)
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ModuleArtifactError {
    #[error("failed to encode module artifact: {0}")]
    Codec(String),
    #[error("module artifact {field} mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
}

#[derive(Debug, Error)]
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
            ModuleArtifactError::Codec(message) => Self::Decode(message),
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

    /// Atomically make `artifact` the current module for an owner namespace and
    /// return the key-set delta from the prior current artifact.
    async fn replace_current_trigger_manifest(
        &self,
        owner_namespace: &str,
        artifact: &ModuleArtifact,
    ) -> Result<TriggerManifestReplacement, ArtifactStoreError>;

    async fn get_current_trigger_manifest(
        &self,
        owner_namespace: &str,
    ) -> Result<Option<CurrentTriggerKeyManifest>, ArtifactStoreError>;

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
    // This store is process-global in ephemeral mode, so current manifests are
    // intentionally retained for the process lifetime and this map can grow
    // without bound. The store is independently injected and has no per-session
    // owner: no session-store factory holds a handle to it, so a factory-level
    // session delete has nothing it could coherently clear here. A bounded
    // lifecycle therefore requires a durable artifact-store backend rather than
    // coupling the in-memory session factory to this map.
    current_trigger_manifests: Arc<Mutex<BTreeMap<String, CurrentTriggerKeyManifest>>>,
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
        let mut modules = self
            .modules
            .lock()
            .map_err(|_| ArtifactStoreError::Backend("artifact store lock poisoned".to_string()))?;
        modules.insert(artifact.module_ref.clone(), Arc::new(artifact.clone()));
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        module_ref: &ModuleRef,
    ) -> Result<Option<Arc<ModuleArtifact>>, ArtifactStoreError> {
        let modules = self
            .modules
            .lock()
            .map_err(|_| ArtifactStoreError::Backend("artifact store lock poisoned".to_string()))?;
        Ok(modules.get(module_ref).cloned())
    }

    async fn replace_current_trigger_manifest(
        &self,
        owner_namespace: &str,
        artifact: &ModuleArtifact,
    ) -> Result<TriggerManifestReplacement, ArtifactStoreError> {
        let mut manifests = self.current_trigger_manifests.lock().map_err(|_| {
            ArtifactStoreError::Backend("current trigger manifest lock poisoned".to_string())
        })?;
        let previous = manifests.insert(
            owner_namespace.to_string(),
            CurrentTriggerKeyManifest {
                module_ref: artifact.module_ref.clone(),
                manifest: artifact.trigger_key_manifest.clone(),
            },
        );
        Ok(TriggerManifestReplacement {
            previous_module_ref: previous.as_ref().map(|entry| entry.module_ref.clone()),
            current_module_ref: artifact.module_ref.clone(),
            diff: previous
                .map(|entry| entry.manifest.diff(&artifact.trigger_key_manifest))
                .unwrap_or_default(),
        })
    }

    async fn get_current_trigger_manifest(
        &self,
        owner_namespace: &str,
    ) -> Result<Option<CurrentTriggerKeyManifest>, ArtifactStoreError> {
        Ok(self
            .current_trigger_manifests
            .lock()
            .map_err(|_| {
                ArtifactStoreError::Backend("current trigger manifest lock poisoned".to_string())
            })?
            .get(owner_namespace)
            .cloned())
    }

    async fn put_artifact_bytes(
        &self,
        artifact_ref: &str,
        _descriptor: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        self.artifacts
            .lock()
            .map_err(|_| ArtifactStoreError::Backend("artifact store lock poisoned".to_string()))?
            .insert(artifact_ref.to_string(), bytes.to_vec());
        Ok(())
    }

    async fn get_artifact_bytes(
        &self,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        Ok(self
            .artifacts
            .lock()
            .map_err(|_| ArtifactStoreError::Backend("artifact store lock poisoned".to_string()))?
            .get(artifact_ref)
            .cloned())
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

fn module_ref(
    program: &Program,
    host_requirements_ref: &HostRequirementsRef,
    exports: &ModuleExports,
) -> ModuleRef {
    let mut writer = HashWriter::new();
    writer.atom(LASHLANG_SEMANTIC_HASH_VERSION);
    writer.atom("module");
    writer.atom(host_requirements_ref.as_str());
    write_exports(&mut writer, exports);
    write_program(&mut writer, program);
    ModuleRef::new(&writer.finish())
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
    }
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
        TypeExpr::Process {
            input,
            output,
            input_count,
        } => {
            writer.atom("type:process");
            writer.usize(*input_count);
            write_type(writer, input);
            write_type(writer, output);
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
        Expr::TypeLiteral(ty) => {
            writer.atom("type-literal");
            write_type(writer, ty);
        }
    }
}

fn write_label_metadata(writer: &mut HashWriter, label: &LabelMetadata) {
    writer.atom("label");
    writer.atom(label.title.as_str());
    match &label.description {
        Some(description) => {
            writer.atom("description");
            writer.atom(description.as_str());
        }
        None => writer.atom("no-description"),
    }
}

fn write_unary_expr(
    writer: &mut HashWriter,
    tag: &'static str,
    expr: &Expr,
    normalizer: &NameNormalizer,
) {
    writer.atom(tag);
    write_expr(writer, expr, normalizer);
}

fn write_resource_ref(writer: &mut HashWriter, resource: &ResourceRefExpr) {
    writer.atom("path");
    writer.usize(resource.path.len());
    for segment in &resource.path {
        writer.atom(segment.as_str());
    }
    writer.atom("handle");
    writer.atom(resource.resource_type.as_str());
    writer.atom(resource.alias.as_str());
}

fn write_unary_op(writer: &mut HashWriter, op: UnaryOp) {
    writer.atom(match op {
        UnaryOp::Negate => "negate",
        UnaryOp::Not => "not",
    });
}

fn write_binary_op(writer: &mut HashWriter, op: BinaryOp) {
    writer.atom(match op {
        BinaryOp::Add => "add",
        BinaryOp::Subtract => "subtract",
        BinaryOp::Multiply => "multiply",
        BinaryOp::Divide => "divide",
        BinaryOp::Modulo => "modulo",
        BinaryOp::Equal => "equal",
        BinaryOp::NotEqual => "not-equal",
        BinaryOp::Less => "less",
        BinaryOp::LessEqual => "less-equal",
        BinaryOp::Greater => "greater",
        BinaryOp::GreaterEqual => "greater-equal",
        BinaryOp::In => "in",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
    });
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

#[derive(Default)]
struct HashWriter {
    bytes: Vec<u8>,
}

impl HashWriter {
    fn new() -> Self {
        Self::default()
    }

    fn atom(&mut self, value: &str) {
        self.bytes
            .extend_from_slice(value.len().to_string().as_bytes());
        self.bytes.push(b':');
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(b';');
    }

    fn bool(&mut self, value: bool) {
        self.atom(if value { "true" } else { "false" });
    }

    fn usize(&mut self, value: usize) {
        self.atom(&value.to_string());
    }

    fn u32(&mut self, value: u32) {
        self.atom(&value.to_string());
    }

    fn u64(&mut self, value: u64) {
        self.atom(&value.to_string());
    }

    fn finish(self) -> ContentHash {
        ContentHash::new(hex_digest(&Sha256::digest(self.bytes)))
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[derive(Clone, Debug)]
enum RequirementBinding {
    Value,
    Resource {
        resource_type: String,
        path: Option<Vec<String>>,
    },
}

struct RequirementsCollector<'program> {
    program: &'program Program,
    resource_catalog: Option<&'program LashlangHostCatalog>,
    type_names: BTreeSet<String>,
    requirements: HostRequirements,
}

impl<'program> RequirementsCollector<'program> {
    fn new(program: &'program Program) -> Self {
        let type_names = program
            .declarations
            .iter()
            .filter_map(|declaration| match declaration {
                Declaration::Type(type_decl) => Some(type_decl.name.to_string()),
                _ => None,
            })
            .collect();
        Self {
            program,
            resource_catalog: None,
            type_names,
            requirements: HostRequirements::default(),
        }
    }

    fn with_resource_catalog(mut self, catalog: &'program LashlangHostCatalog) -> Self {
        self.resource_catalog = Some(catalog);
        self
    }

    fn collect(mut self) -> HostRequirements {
        for declaration in &self.program.declarations {
            match declaration {
                Declaration::Type(type_decl) => self.collect_type(&type_decl.ty),
                Declaration::Process(process) => {
                    self.requirements.abilities.processes = true;
                    if process.label.is_some() {
                        self.requirements.language_features.label_annotations = true;
                    }
                    let mut scope = BTreeMap::new();
                    for param in &process.params {
                        self.collect_type(&param.ty);
                        if let TypeExpr::Ref(name) = &param.ty
                            && !self.type_names.contains(name.as_str())
                            && self.is_resource_type_name(name)
                        {
                            self.requirements
                                .resources
                                .ensure_resource_type(name.to_string());
                            scope.insert(
                                param.name.to_string(),
                                RequirementBinding::Resource {
                                    resource_type: name.to_string(),
                                    path: None,
                                },
                            );
                        } else {
                            scope.insert(param.name.to_string(), RequirementBinding::Value);
                        }
                    }
                    if let Some(return_ty) = &process.return_ty {
                        self.collect_type(return_ty);
                    }
                    scope.insert("input".to_string(), RequirementBinding::Value);
                    scope.insert("inputs".to_string(), RequirementBinding::Value);
                    self.collect_expr(&process.body, &mut scope);
                }
            }
        }
        let mut top_level = BTreeMap::new();
        self.collect_expr(&self.program.main, &mut top_level);
        self.requirements
    }

    fn collect_type(&mut self, ty: &TypeExpr) {
        match ty {
            TypeExpr::List(item) => self.collect_type(item),
            TypeExpr::Object(fields) => {
                for field in fields {
                    self.collect_type(&field.ty);
                }
            }
            TypeExpr::Union(items) => {
                for item in items {
                    self.collect_type(item);
                }
            }
            TypeExpr::Process { input, output, .. } => {
                self.collect_type(input);
                self.collect_type(output);
            }
            TypeExpr::TriggerHandle(event) => self.collect_type(event),
            TypeExpr::Ref(name)
                if !self.type_names.contains(name.as_str())
                    && self.is_host_data_type_name(name) =>
            {
                let data_type = self
                    .resource_catalog
                    .and_then(|catalog| catalog.resolve_named_data_type(name.as_str()))
                    .expect("checked host data type presence")
                    .clone();
                self.requirements
                    .resources
                    .add_named_data_type(data_type)
                    .expect("host data type requirement came from host catalog");
            }
            TypeExpr::Ref(name)
                if !self.type_names.contains(name.as_str()) && self.is_resource_type_name(name) =>
            {
                self.requirements
                    .resources
                    .ensure_resource_type(name.to_string());
            }
            TypeExpr::Any
            | TypeExpr::Str
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Dict
            | TypeExpr::Null
            | TypeExpr::Enum(_)
            | TypeExpr::Ref(_) => {}
        }
    }

    fn is_host_data_type_name(&self, name: &str) -> bool {
        self.resource_catalog
            .map(|catalog| catalog.has_named_data_type(name))
            .unwrap_or(false)
    }

    fn is_resource_type_name(&self, name: &str) -> bool {
        self.resource_catalog
            .map(|catalog| catalog.has_resource_type(name))
            .unwrap_or(true)
    }

    fn collect_expr(
        &mut self,
        expr: &Expr,
        scope: &mut BTreeMap<String, RequirementBinding>,
    ) -> Option<RequirementBinding> {
        match expr {
            Expr::Block(expressions) => {
                let mut last = None;
                for expression in expressions {
                    last = self.collect_expr(expression, scope);
                }
                last
            }
            Expr::LabelAnnotated { expr, .. } => {
                self.requirements.language_features.label_annotations = true;
                self.collect_expr(expr, scope)
            }
            Expr::Variable(name) => match scope.get(name.as_str()).cloned() {
                Some(binding) => Some(binding),
                None => {
                    self.requirements.globals.insert(name.to_string());
                    Some(RequirementBinding::Value)
                }
            },
            Expr::Tuple(items) => {
                for item in items {
                    self.collect_expr(item, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::List(items) => {
                for item in items {
                    self.collect_expr(item, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::ListComprehension { element, clauses } => {
                let mut previous = Vec::new();
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { binding, iterable } => {
                            self.collect_expr(iterable, scope);
                            previous.push((
                                binding.to_string(),
                                scope.insert(binding.to_string(), RequirementBinding::Value),
                            ));
                        }
                        ListComprehensionClause::If { condition } => {
                            self.collect_expr(condition, scope);
                        }
                    }
                }
                self.collect_expr(element, scope);
                for (binding, previous) in previous.into_iter().rev() {
                    if let Some(previous) = previous {
                        scope.insert(binding, previous);
                    } else {
                        scope.remove(binding.as_str());
                    }
                }
                Some(RequirementBinding::Value)
            }
            Expr::Record(entries) => {
                for (_, value) in entries {
                    self.collect_expr(value, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::Assign { target, expr } => {
                for step in &target.steps {
                    if let AssignPathStep::Index(index) = step {
                        self.collect_expr(index, scope);
                    }
                }
                let binding = self
                    .collect_expr(expr, scope)
                    .unwrap_or(RequirementBinding::Value);
                if target.steps.is_empty() {
                    scope.insert(target.root.to_string(), binding);
                } else if !scope.contains_key(target.root.as_str()) {
                    self.requirements.globals.insert(target.root.to_string());
                }
                Some(RequirementBinding::Value)
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_expr(condition, scope);
                let mut then_scope = scope.clone();
                self.collect_expr(then_block, &mut then_scope);
                let mut else_scope = scope.clone();
                self.collect_expr(else_block, &mut else_scope);
                for (name, binding) in then_scope.into_iter().chain(else_scope) {
                    scope.entry(name).or_insert(binding);
                }
                Some(RequirementBinding::Value)
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                self.collect_expr(iterable, scope);
                let previous = scope.insert(binding.to_string(), RequirementBinding::Value);
                self.collect_expr(body, scope);
                if let Some(previous) = previous {
                    scope.insert(binding.to_string(), previous);
                } else {
                    scope.remove(binding.as_str());
                }
                Some(RequirementBinding::Value)
            }
            Expr::While { condition, body } => {
                self.collect_expr(condition, scope);
                self.collect_expr(body, scope);
                Some(RequirementBinding::Value)
            }
            Expr::StartProcess(start) => {
                self.requirements.abilities.processes = true;
                for (_, value) in &start.args {
                    self.collect_expr(value, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::ProcessRef { .. } => {
                self.requirements.abilities.processes = true;
                Some(RequirementBinding::Value)
            }
            Expr::HostDescriptorConstructor { type_name, input } => {
                if let Some(catalog) = self.resource_catalog
                    && let Some(constructor) = catalog
                        .value_constructors()
                        .map(|(_, constructor)| constructor)
                        .find(|constructor| constructor.type_name == type_name.as_str())
                {
                    self.requirements.resources.add_value_constructor(
                        constructor.path.iter().map(String::as_str),
                        constructor.input_ty.clone(),
                        constructor.output_ty.clone(),
                    );
                }
                if let Some(catalog) = self.resource_catalog
                    && let Some(binding) = catalog.resolve_trigger_source(type_name.as_str())
                {
                    self.requirements
                        .resources
                        .add_trigger_source_type(
                            type_name.to_string(),
                            binding.event_type().clone(),
                        )
                        .expect("trigger source requirement came from host catalog");
                }
                self.collect_expr(input, scope);
                Some(RequirementBinding::Value)
            }
            Expr::ResourceRef(resource) => {
                self.require_resource_ref(resource);
                Some(RequirementBinding::Resource {
                    resource_type: resource.resource_type.to_string(),
                    path: Some(resource.path.iter().map(ToString::to_string).collect()),
                })
            }
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let receiver = self.collect_expr(receiver, scope);
                if let Some(RequirementBinding::Resource {
                    resource_type,
                    path,
                }) = receiver
                {
                    self.require_resource_operation(resource_type, path, operation.as_str());
                }
                for arg in args {
                    self.collect_expr(arg, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::SleepFor(expr) | Expr::SleepUntil(expr) => {
                self.requirements.abilities.sleep = true;
                self.collect_expr(expr, scope);
                Some(RequirementBinding::Value)
            }
            Expr::WaitSignal { .. } => {
                self.requirements.abilities.process_signals = true;
                Some(RequirementBinding::Value)
            }
            Expr::SignalRun { run, payload, .. } => {
                self.requirements.abilities.process_signals = true;
                self.collect_expr(run, scope);
                self.collect_expr(payload, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Await(expr)
            | Expr::ResultUnwrap(expr)
            | Expr::Cancel(expr)
            | Expr::Print(expr)
            | Expr::Yield(expr)
            | Expr::Wake(expr)
            | Expr::Fail(expr)
            | Expr::Unary { expr, .. } => {
                self.collect_expr(expr, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Finish(expr) => {
                self.collect_expr(expr, scope);
                Some(RequirementBinding::Value)
            }
            Expr::BuiltinCall { args, .. } => {
                for arg in args {
                    self.collect_expr(arg, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::Field { target, .. } => {
                self.collect_expr(target, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Index { target, index } => {
                self.collect_expr(target, scope);
                self.collect_expr(index, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Binary { left, right, .. } => {
                self.collect_expr(left, scope);
                self.collect_expr(right, scope);
                Some(RequirementBinding::Value)
            }
            Expr::TypeLiteral(ty) => {
                self.collect_type(ty);
                Some(RequirementBinding::Value)
            }
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Break
            | Expr::Continue => Some(RequirementBinding::Value),
        }
    }

    fn require_resource_ref(&mut self, resource: &ResourceRefExpr) {
        self.requirements
            .resources
            .add_module_instance(
                resource.path.iter().map(|segment| segment.as_str()),
                resource.resource_type.to_string(),
            )
            .expect("resolved resource references cannot conflict");
    }

    fn require_resource_operation(
        &mut self,
        resource_type: String,
        path: Option<Vec<String>>,
        operation: &str,
    ) {
        let (operation, binding) = self.resource_operation_requirement(&resource_type, operation);
        if let (Some(catalog), Some(path)) = (self.resource_catalog, path.as_ref()) {
            let alias = path.join(".");
            if let Some(module_binding) =
                catalog.resolve_module_operation(&resource_type, &alias, &operation)
            {
                self.requirements.resources.add_module_operation_binding(
                    path.iter().map(String::as_str),
                    resource_type,
                    operation,
                    module_binding.host_operation.clone(),
                    binding,
                );
                return;
            }
        }
        self.requirements
            .resources
            .add_operation_binding(resource_type, operation, binding);
    }

    fn resource_operation_requirement(
        &self,
        resource_type: &str,
        operation: &str,
    ) -> (String, ResourceOperationBinding) {
        if let Some(catalog) = self.resource_catalog
            && let Some(binding) = catalog.resolve_operation(resource_type, operation)
        {
            return (operation.to_string(), binding.clone());
        }
        (
            operation.to_string(),
            ResourceOperationBinding {
                input_ty: TypeExpr::Any,
                output_ty: TypeExpr::Any,
                output_from_input: None,
            },
        )
    }
}
