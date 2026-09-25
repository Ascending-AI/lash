#[cfg(test)]
use std::collections::HashSet;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core_execution::{
    ArtifactPublicationPause, ArtifactStoreError, DurabilityTier, ModuleArtifactStore,
};
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
    write_binary_op, write_label_metadata, write_process_origin, write_resource_ref,
    write_structural_role, write_unary_expr, write_unary_op,
};

use crate::ast::{
    AssignPathStep, BinaryOp, Declaration, Expr, LabelMetadata, ListComprehensionClause, MethodKey,
    ProcessDecl, Program, ResourceRefExpr, TypeExpr, UnaryOp,
};
use crate::linker::{
    LashlangAbilities, LashlangHostCatalog, LashlangLanguageFeatures, ResourceOperationBinding,
};

pub use lash_sansio::LASHLANG_SEMANTIC_HASH_VERSION;
pub const LASHLANG_COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// v11: `ResourceOperationBatch` carries the aggregate's consumer mode, timer
/// leaves and the immediate-prefix boundary, and its result is the response
/// algebra of ADR 0099 §10 L2 instead of a settlement order.
/// v12 (FIG-3586): `ResourceOperationBatch` no longer carries the aggregate's
/// instruction pointer (`site`) or its per-instruction `occurrence`. A host
/// keys an aggregate by the issue ordinal it mints when the batch leaves the
/// VM, so nothing compiler-derived reaches a replay key.
/// v14 (FIG-3707): a compiled program shares an assigned captured binding
/// through a binding cell rather than copying it, so a host bridge built for
/// v13 would run a program this VM compiled under the old capture meaning.
pub const LASHLANG_VM_ABI_VERSION: &str = "lashlang-vm-abi-v14";

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

/// One admitted module: the executable program, exactly as the linker
/// produced it, and the identities derived from it.
///
/// `ir` is the only program an artifact carries. Execution, node identity,
/// trace maps and runnable graph views all read it; nothing re-derives a
/// second program from it. It keeps every binding name and carries no spans
/// (the durable form is span-free; a linked module keeps its diagnostic spans
/// beside the artifact).
///
/// An artifact is admitted by construction: its fields are private, and the
/// only ways to obtain one are the validating builders (the linker,
/// [`Self::from_program`]) and the verifying store decoder
/// ([`Self::from_store_bytes`]). There is no struct literal, no field write
/// and no `Deserialize` path around them, so every artifact [`crate::compile`]
/// sees has a valid program and refs derived from its own content.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModuleArtifact {
    module_ref: ModuleRef,
    host_requirements_ref: HostRequirementsRef,
    host_requirements: HostRequirements,
    exports: ModuleExports,
    ir: Program,
}

/// The stored shape of a [`ModuleArtifact`], decoded only to be verified.
#[derive(Deserialize)]
struct StoredModuleArtifact {
    module_ref: ModuleRef,
    host_requirements_ref: HostRequirementsRef,
    host_requirements: HostRequirements,
    exports: ModuleExports,
    ir: Program,
}

impl ModuleArtifact {
    /// Builds a raw module artifact from already-complete program IR.
    ///
    /// Source programs whose process output is inferred must go through the
    /// linker; this builder refuses an incomplete exported signature. Spans are
    /// diagnostics, not identity, and never reach the artifact.
    pub fn from_program(mut program: Program) -> Result<Self, ModuleArtifactError> {
        program.spans.clear();
        let requirements = host_requirements_for_program(&program);
        Self::from_ir_and_requirements(program, requirements)
    }

    /// `ir` must already be span-free: the linker keeps the spans it strips.
    pub(crate) fn from_ir_and_requirements(
        ir: Program,
        requirements: HostRequirements,
    ) -> Result<Self, ModuleArtifactError> {
        Self::check_ir(&ir)?;
        let host_requirements_ref = hash_host_requirements(&requirements);
        let exports = module_exports(&ir);
        let module_ref = module_ref(&ir, &host_requirements_ref, &exports);
        Ok(Self {
            module_ref,
            host_requirements_ref,
            host_requirements: requirements,
            exports,
            ir,
        })
    }

    /// Refuses IR the compiler cannot lower, independently of its refs, and
    /// IR that carries spans: the durable form is span-free.
    ///
    /// Shared by the builder and by `verify` so an admission check sees exactly
    /// what construction does.
    fn check_ir(ir: &Program) -> Result<(), ModuleArtifactError> {
        if !ir.spans.is_empty() {
            return Err(ModuleArtifactError::DurableSpans);
        }
        crate::ast::validate_ast(ir)?;
        crate::ast::check_unique_declarations(ir)?;
        if let Some(process) = ir.declarations.iter().find_map(|declaration| {
            let Declaration::Process(process) = declaration else {
                return None;
            };
            process.return_ty.is_none().then_some(process)
        }) {
            return Err(ModuleArtifactError::IncompleteProcessSignature {
                process: process.name.to_string(),
            });
        }
        Ok(())
    }

    /// The definition identity a trace and an admitted graph both name
    /// (ADR 0100 R6): a digest, under `lash-workflow-source/v4`, of the same
    /// deterministic atom stream the module ref hashes for the program (its
    /// language and its span-free IR, names and number literals by the one IR
    /// number rule). It never depends on how a dialect would print the program
    /// or on a serializer's spelling.
    pub fn source_identity(&self) -> String {
        let mut writer = HashWriter::for_source_identity();
        writer.atom("source");
        writer.atom(self.ir.language.as_str());
        write_program(&mut writer, &self.ir);
        writer.finish().as_str().to_string()
    }

    /// The module's identity: a hash of its language, host requirements,
    /// exports and complete program.
    pub fn module_ref(&self) -> &ModuleRef {
        &self.module_ref
    }

    pub fn host_requirements_ref(&self) -> &HostRequirementsRef {
        &self.host_requirements_ref
    }

    pub fn host_requirements(&self) -> &HostRequirements {
        &self.host_requirements
    }

    pub fn exports(&self) -> &ModuleExports {
        &self.exports
    }

    /// The executable program: the linked program, verbatim and span-free.
    pub fn ir(&self) -> &Program {
        &self.ir
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
            .ir
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

    pub fn process_type(&self, process_name: &str) -> Option<TypeExpr> {
        let process = self.ir.process(process_name)?;
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

    pub fn introspect(
        &self,
    ) -> Result<crate::ModuleIntrospection, crate::ModuleIntrospectionError> {
        crate::ModuleIntrospection::from_artifact(self)
    }

    /// Refuses decoded content whose recorded refs do not match the content.
    ///
    /// The refs are derived from borrowed content rather than by rebuilding the
    /// artifact, so a decode never deep-copies the program only to hash it
    /// (FIG-3088). Only the store decoder needs it: every other artifact was
    /// built by a validating builder.
    fn verify(&self) -> Result<(), ModuleArtifactError> {
        Self::check_ir(&self.ir)?;
        let derived_host_requirements_ref = hash_host_requirements(&self.host_requirements);
        let derived_exports = module_exports(&self.ir);
        let derived_module_ref =
            module_ref(&self.ir, &derived_host_requirements_ref, &derived_exports);
        if derived_module_ref != self.module_ref {
            return Err(ModuleArtifactError::HashMismatch {
                field: "module_ref",
                expected: derived_module_ref.to_string(),
                actual: self.module_ref.to_string(),
            });
        }
        if derived_host_requirements_ref != self.host_requirements_ref {
            return Err(ModuleArtifactError::HashMismatch {
                field: "host_requirements_ref",
                expected: derived_host_requirements_ref.to_string(),
                actual: self.host_requirements_ref.to_string(),
            });
        }
        if derived_exports != self.exports {
            return Err(ModuleArtifactError::HashMismatch {
                field: "exports",
                expected: "derived exports".to_string(),
                actual: "artifact exports".to_string(),
            });
        }
        Ok(())
    }

    pub fn to_store_bytes(&self) -> Result<Vec<u8>, ModuleArtifactError> {
        serde_json::to_vec(self).map_err(|err| ModuleArtifactError::Codec(err.to_string()))
    }

    pub fn from_store_bytes(bytes: &[u8]) -> Result<Self, ModuleArtifactError> {
        let raw: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|err| ModuleArtifactError::Codec(err.to_string()))?;
        reject_future_shape(&raw)?;
        let stored: StoredModuleArtifact = serde_json::from_slice(bytes).map_err(|err| {
            let message = err.to_string();
            if message.contains("unknown variant") {
                ModuleArtifactError::FutureShape {
                    field: "artifact shape",
                    value: "nested enum variant".to_string(),
                }
            } else {
                ModuleArtifactError::Codec(message)
            }
        })?;
        let artifact = Self {
            module_ref: stored.module_ref,
            host_requirements_ref: stored.host_requirements_ref,
            host_requirements: stored.host_requirements,
            exports: stored.exports,
            ir: stored.ir,
        };
        artifact.verify()?;
        Ok(artifact)
    }
}

#[expect(
    clippy::expect_used,
    reason = "the resolved children and output were all validated before the original signature formed, so the rebuild revalidates, per the message"
)]
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
        TypeExpr::Union(items) => TypeExpr::union(
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
    if raw.get("compilation_dialect").is_some() {
        return Err(ModuleArtifactError::RetiredCompilationDialect);
    }
    Ok(())
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
    #[error("invalid module program: {0}")]
    InvalidAst(#[from] crate::InvalidAst),
    #[error("a module artifact carries no source spans; spans stay with the linked module")]
    DurableSpans,
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
        "module artifact records the retired `compilation_dialect` field; it was published \
         before TypeScript became the sole RLM dialect and must be recompiled and republished"
    )]
    RetiredCompilationDialect,
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

impl From<ModuleArtifactError> for ArtifactStoreError {
    fn from(value: ModuleArtifactError) -> Self {
        match value {
            ModuleArtifactError::InvalidAst(source) => Self::Decode(source.to_string()),
            ModuleArtifactError::DurableSpans => Self::Decode(value.to_string()),
            ModuleArtifactError::ObsoleteProcessTypeShape => Self::Decode(
                "module artifact uses the obsolete anonymous process type shape; recompile and republish the module"
                    .to_string(),
            ),
            ModuleArtifactError::IncompleteProcessSignature { .. } => {
                Self::Decode(value.to_string())
            }
            ModuleArtifactError::Codec(message) => Self::Decode(message),
            ModuleArtifactError::FutureShape { .. } => Self::Decode(value.to_string()),
            ModuleArtifactError::RetiredCompilationDialect => Self::Decode(value.to_string()),
            ModuleArtifactError::HashMismatch { .. } => Self::Decode(value.to_string()),
        }
    }
}

/// The typed Lashlang view of a store set's module-artifact port.
///
/// The port ([`ModuleArtifactStore`]) keeps a module's verified store bytes
/// and never decodes them; this view encodes a [`ModuleArtifact`] on publish
/// and decodes and verifies it on read. Modules are content-addressed and
/// immutable, so a decoded module is cached by its reference, and a read
/// returns the cached module only once the port confirms the module is still
/// retained. Cloning shares the port and the cache.
#[derive(Clone)]
pub struct LashlangArtifacts {
    store: Arc<dyn ModuleArtifactStore>,
    decoded: Arc<Mutex<BTreeMap<ModuleRef, Arc<ModuleArtifact>>>>,
}

impl LashlangArtifacts {
    /// The typed view of `store`.
    pub fn new(store: Arc<dyn ModuleArtifactStore>) -> Self {
        Self {
            store,
            decoded: Arc::default(),
        }
    }

    /// The typed view of `backend`'s store set's artifact port: the
    /// artifacts live in the storage that reopens the backend's sessions.
    pub fn of_backend(backend: &lash_core_execution::Backend) -> Self {
        Self::new(backend.module_artifacts())
    }

    /// The port this view reads and writes through.
    pub fn store(&self) -> &Arc<dyn ModuleArtifactStore> {
        &self.store
    }

    /// See [`ModuleArtifactStore::pause_next_publication_for_testing`].
    pub fn pause_next_publication_for_testing(&self) -> Option<ArtifactPublicationPause> {
        self.store.pause_next_publication_for_testing()
    }

    /// The durability tier of the port.
    pub fn durability_tier(&self) -> DurabilityTier {
        self.store.durability_tier()
    }

    /// Publish an immutable module and retain it for one exact owner.
    pub async fn publish_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        artifact: &ModuleArtifact,
    ) -> Result<(), ArtifactStoreError> {
        let bytes = artifact
            .to_store_bytes()
            .map_err(|err| ArtifactStoreError::Encode(err.to_string()))?;
        self.store
            .publish_module_artifact(owner, artifact.module_ref().as_str(), &bytes)
            .await?;
        self.decoded
            .lock_recover()
            .insert(artifact.module_ref().clone(), Arc::new(artifact.clone()));
        Ok(())
    }

    /// Add an owner edge to an already published module.
    pub async fn retain_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError> {
        self.store
            .retain_module_artifact(owner, module_ref.as_str())
            .await
    }

    /// Atomically add `to` and sever `from` for one module artifact.
    pub async fn transfer_module_artifact(
        &self,
        from: &lash_core_execution::ArtifactOwner,
        to: &lash_core_execution::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError> {
        self.store
            .transfer_module_artifact(from, to, module_ref.as_str())
            .await
    }

    /// Sever one exact owner edge and reclaim the module when it was the last.
    pub async fn release_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError> {
        self.store
            .release_module_artifact(owner, module_ref.as_str())
            .await?;
        self.decoded.lock_recover().remove(module_ref);
        Ok(())
    }

    /// Permanently fence an execution owner against late publication and sever
    /// every module edge it still owns.
    pub async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), ArtifactStoreError> {
        self.store.retire_module_artifact_owner(owner).await?;
        self.decoded.lock_recover().clear();
        Ok(())
    }

    /// The module published under `module_ref`, decoded and verified, if it is
    /// retained.
    pub async fn get_module_artifact(
        &self,
        module_ref: &ModuleRef,
    ) -> Result<Option<Arc<ModuleArtifact>>, ArtifactStoreError> {
        let Some(bytes) = self.store.get_module_artifact(module_ref.as_str()).await? else {
            self.decoded.lock_recover().remove(module_ref);
            return Ok(None);
        };
        if let Some(artifact) = self.decoded.lock_recover().get(module_ref).cloned() {
            return Ok(Some(artifact));
        }
        let artifact = Arc::new(ModuleArtifact::from_store_bytes(&bytes)?);
        // The port keys bytes by the reference its caller names; the decoder
        // proves the bytes hash to the reference they carry, and this proves
        // that reference is the one asked for.
        if artifact.module_ref() != module_ref {
            return Err(ArtifactStoreError::Decode(format!(
                "module artifact stored under `{module_ref}` is `{}`",
                artifact.module_ref()
            )));
        }
        self.decoded
            .lock_recover()
            .insert(module_ref.clone(), Arc::clone(&artifact));
        Ok(Some(artifact))
    }
}

/// Reference model of [`ModuleArtifactStore`] for lashlang's own unit
/// tests. Hosts take their artifact store from their store set
/// ([`lash_core_execution::StoreSet::module_artifacts`]); nothing outside
/// this crate's tests can name this type.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct InMemoryLashlangArtifactStore {
    state: Arc<Mutex<InMemoryArtifactState>>,
    publication_pause: Arc<Mutex<Option<ArtifactPublicationPause>>>,
}

#[cfg(test)]
#[derive(Default)]
struct InMemoryArtifactState {
    modules: BTreeMap<String, Vec<u8>>,
    owners: HashSet<(String, lash_core_execution::ArtifactOwner)>,
    retired_owners: HashSet<lash_core_execution::ArtifactOwner>,
}

#[cfg(test)]
impl InMemoryLashlangArtifactStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl ModuleArtifactStore for InMemoryLashlangArtifactStore {
    fn pause_next_publication_for_testing(&self) -> Option<ArtifactPublicationPause> {
        let pause = ArtifactPublicationPause::default();
        *self.publication_pause.lock_recover() = Some(pause.clone());
        Some(pause)
    }

    async fn publish_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let publication_pause = self.publication_pause.lock_recover().take();
        if let Some(pause) = publication_pause {
            pause.pause().await;
        }
        let mut state = self.state.lock_recover();
        if state.retired_owners.contains(owner) {
            return Err(ArtifactStoreError::OwnerRetired);
        }
        if let Some(existing) = state.modules.get(module_ref)
            && existing.as_slice() != bytes
        {
            return Err(ArtifactStoreError::Backend(format!(
                "module artifact `{module_ref}` is immutable"
            )));
        }
        state
            .modules
            .entry(module_ref.to_string())
            .or_insert_with(|| bytes.to_vec());
        state.owners.insert((module_ref.to_string(), owner.clone()));
        Ok(())
    }

    async fn retain_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        let mut state = self.state.lock_recover();
        if state.retired_owners.contains(owner) {
            return Err(ArtifactStoreError::OwnerRetired);
        }
        if !state.modules.contains_key(module_ref) {
            return Err(ArtifactStoreError::Backend(format!(
                "missing module artifact `{module_ref}`"
            )));
        }
        state.owners.insert((module_ref.to_string(), owner.clone()));
        Ok(())
    }

    async fn transfer_module_artifact(
        &self,
        from: &lash_core_execution::ArtifactOwner,
        to: &lash_core_execution::ArtifactOwner,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        let mut state = self.state.lock_recover();
        if state.retired_owners.contains(to) {
            return Err(ArtifactStoreError::DestinationOwnerRetired);
        }
        let from_edge = (module_ref.to_string(), from.clone());
        if !state.owners.contains(&from_edge) {
            if state.owners.contains(&(module_ref.to_string(), to.clone())) {
                return Ok(());
            }
            return Err(ArtifactStoreError::StagingEdgeMissing {
                artifact: format!("module artifact `{module_ref}`"),
            });
        }
        state.owners.insert((module_ref.to_string(), to.clone()));
        state.owners.remove(&from_edge);
        Ok(())
    }

    async fn release_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        let mut state = self.state.lock_recover();
        state
            .owners
            .remove(&(module_ref.to_string(), owner.clone()));
        if !state
            .owners
            .iter()
            .any(|(candidate, _)| candidate == module_ref)
        {
            state.modules.remove(module_ref);
        }
        Ok(())
    }

    async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), ArtifactStoreError> {
        if !matches!(owner, lash_core_execution::ArtifactOwner::Execution(_)) {
            return Err(ArtifactStoreError::Backend(
                "only execution artifact owners can be retired".to_string(),
            ));
        }
        let mut state = self.state.lock_recover();
        state.retired_owners.insert(owner.clone());
        let affected = state
            .owners
            .iter()
            .filter_map(|(module_ref, candidate)| {
                (candidate == owner).then_some(module_ref.clone())
            })
            .collect::<Vec<_>>();
        state.owners.retain(|(_, candidate)| candidate != owner);
        for module_ref in affected {
            if !state
                .owners
                .iter()
                .any(|(candidate, _)| candidate == &module_ref)
            {
                state.modules.remove(&module_ref);
            }
        }
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let state = self.state.lock_recover();
        Ok(state.modules.get(module_ref).cloned())
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

fn hash_host_requirements(requirements: &HostRequirements) -> HostRequirementsRef {
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
    writer.bool(requirements.abilities.sleep);
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
    write_expr(writer, &program.main);
    writer.atom("private-bindings");
    writer.usize(program.private_bindings.len());
    for name in &program.private_bindings {
        write_name(writer, name);
    }
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
    write_expr(writer, &function.body);
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
    write_process_origin(writer, &process.origin);
    write_expr(writer, &process.body);
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

/// A binding name, written verbatim: a module's identity is its program with
/// every name in it, so two programs that differ only in a name are two
/// modules (FIG-3571).
fn write_name(writer: &mut HashWriter, name: &str) {
    writer.prefixed_atom("name:", name);
}

fn write_expr(writer: &mut HashWriter, expr: &Expr) {
    match expr {
        Expr::Block(expressions) => {
            writer.atom("block");
            writer.usize(expressions.len());
            for expression in expressions {
                write_expr(writer, expression);
            }
        }
        Expr::LabelAnnotated { label, expr } => {
            writer.atom("label-annotated");
            write_label_metadata(writer, label);
            write_expr(writer, expr);
        }
        Expr::ProcessLiteral(literal) => {
            writer.atom("process-literal");
            for param in &literal.params {
                writer.atom("param");
                writer.atom(param.name.as_str());
                write_type(writer, &param.ty);
            }
            for param in &literal.hidden_args {
                writer.atom("hidden-arg");
                writer.atom(param.name.as_str());
                write_type(writer, &param.ty);
            }
            write_expr(writer, &literal.body);
        }
        Expr::Null => writer.atom("null"),
        Expr::Undefined => writer.atom("javascript:undefined"),
        Expr::Bool(value) => {
            writer.atom("bool");
            writer.bool(*value);
        }
        Expr::Number(value) => {
            writer.atom("number");
            writer.u64(crate::ast::number::canonical_bits(*value));
        }
        Expr::String(value) => {
            writer.atom("string");
            writer.atom(value.as_str());
        }
        Expr::Variable(name) => {
            writer.atom("variable");
            write_name(writer, name.as_str());
        }
        Expr::Tuple(items) => {
            writer.atom("tuple");
            writer.usize(items.len());
            for item in items {
                write_expr(writer, item);
            }
        }
        Expr::List(items) => {
            writer.atom("list");
            writer.usize(items.len());
            for item in items {
                write_expr(writer, item);
            }
        }
        Expr::ListComprehension { element, clauses } => {
            writer.atom("list-comprehension");
            writer.usize(clauses.len());
            for clause in clauses {
                match clause {
                    ListComprehensionClause::For { binding, iterable } => {
                        writer.atom("for");
                        write_name(writer, binding.as_str());
                        write_expr(writer, iterable);
                    }
                    ListComprehensionClause::If { condition } => {
                        writer.atom("if");
                        write_expr(writer, condition);
                    }
                }
            }
            write_expr(writer, element);
        }
        Expr::Record(entries) => {
            writer.atom("record");
            writer.usize(entries.len());
            for (key, value) in entries {
                writer.atom(key.as_str());
                write_expr(writer, value);
            }
        }
        Expr::Assign { target, expr } => {
            writer.atom("assign");
            write_name(writer, target.root.as_str());
            writer.usize(target.steps.len());
            for step in &target.steps {
                match step {
                    AssignPathStep::Field(field) => {
                        writer.atom("field");
                        writer.atom(field.as_str());
                    }
                    AssignPathStep::Index(index) => {
                        writer.atom("index");
                        write_expr(writer, index);
                    }
                }
            }
            write_expr(writer, expr);
        }
        Expr::If {
            condition,
            then_block,
            else_block,
        } => {
            writer.atom("if");
            write_expr(writer, condition);
            write_expr(writer, then_block);
            write_expr(writer, else_block);
        }
        Expr::For {
            binding,
            iterable,
            bind,
            body,
        } => {
            writer.atom("for");
            write_name(writer, binding.as_str());
            write_expr(writer, iterable);
            match bind {
                Some(bind) => {
                    writer.atom("bind");
                    write_expr(writer, bind);
                }
                None => writer.atom("no-bind"),
            }
            write_expr(writer, body);
        }
        Expr::Role { role, expr } => {
            writer.atom("role");
            write_structural_role(writer, role);
            write_expr(writer, expr);
        }
        Expr::While { condition, body } => {
            writer.atom("while");
            write_expr(writer, condition);
            write_expr(writer, body);
        }
        Expr::Break => writer.atom("break"),
        Expr::Continue => writer.atom("continue"),
        Expr::ProcessRef { process } => {
            writer.atom("process-ref");
            writer.atom(process.as_str());
        }
        Expr::HostDescriptorConstructor { type_name, input } => {
            writer.atom("host-value-constructor");
            writer.atom(type_name.as_str());
            write_expr(writer, input);
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
            write_expr(writer, receiver);
            writer.atom(operation.as_str());
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg);
            }
        }
        Expr::Await(expr) => write_unary_expr(writer, "await", expr),
        Expr::SleepFor(expr) => write_unary_expr(writer, "sleep-for", expr),
        Expr::SleepUntil(expr) => write_unary_expr(writer, "sleep-until", expr),
        Expr::WaitSignal { name } => {
            writer.atom("wait-signal");
            writer.atom(name.as_str());
        }
        Expr::ResultUnwrap(expr) => write_unary_expr(writer, "unwrap", expr),
        Expr::Print(expr) => write_unary_expr(writer, "print", expr),
        Expr::Yield(expr) => write_unary_expr(writer, "yield", expr),
        Expr::Finish(expr) => write_unary_expr(writer, "finish", expr),
        Expr::Fail(expr) => write_unary_expr(writer, "fail", expr),
        Expr::BuiltinCall { name, args } => {
            writer.atom("builtin-call");
            writer.atom(name.as_str());
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg);
            }
        }
        Expr::FunctionCall { function, args } => {
            writer.atom("declared-function-call");
            writer.atom(function.as_str());
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg);
            }
        }
        Expr::Function(function) => {
            writer.atom("function");
            match &function.name {
                Some(name) => write_name(writer, name.as_str()),
                None => writer.atom("anonymous"),
            }
            // The ECMA `name` own property is observable state — two programs
            // identical but for an inferred name answer `f.name` differently.
            match &function.js_name {
                Some(name) => write_name(writer, name.as_str()),
                None => writer.atom("unnamed"),
            }
            match &function.receiver {
                Some(receiver) => {
                    writer.atom("receiver");
                    write_name(writer, receiver.as_str());
                }
                None => writer.atom("no-receiver"),
            }
            writer.usize(function.params.len());
            for param in &function.params {
                write_name(writer, param.as_str());
            }
            writer.usize(function.captures.len());
            for capture in &function.captures {
                write_name(writer, capture.as_str());
            }
            write_expr(writer, &function.body);
        }
        Expr::Call { function, args } => {
            writer.atom("function-call");
            write_expr(writer, function);
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg);
            }
        }
        Expr::MethodCall {
            receiver,
            method,
            args,
        } => {
            writer.atom("method-call");
            write_expr(writer, receiver);
            match method {
                MethodKey::Field(field) => {
                    writer.atom("field");
                    writer.atom(field.as_str());
                }
                MethodKey::Index(key) => {
                    writer.atom("index");
                    write_expr(writer, key);
                }
            }
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg);
            }
        }
        Expr::ThisCall {
            this,
            function,
            args,
        } => {
            writer.atom("this-call");
            write_expr(writer, this);
            write_expr(writer, function);
            writer.usize(args.len());
            for arg in args {
                write_expr(writer, arg);
            }
        }
        Expr::Map { items, function } => {
            writer.atom("function-map");
            write_expr(writer, items);
            write_expr(writer, function);
        }
        Expr::Try(scope) => {
            writer.atom("try");
            write_expr(writer, &scope.body);
            match &scope.catch {
                Some(catch) => {
                    writer.atom("catch");
                    write_name(writer, catch.binding.as_str());
                    write_expr(writer, &catch.body);
                }
                None => writer.atom("no-catch"),
            }
            match &scope.finally {
                Some(finally) => {
                    writer.atom("finally");
                    write_expr(writer, finally);
                }
                None => writer.atom("no-finally"),
            }
        }
        Expr::Throw(value) => write_unary_expr(writer, "throw", value),
        Expr::Return(value) => write_unary_expr(writer, "javascript:return", value),
        Expr::Field { target, field } => {
            writer.atom("field-access");
            write_expr(writer, target);
            writer.atom(field.as_str());
        }
        Expr::Index { target, index } => {
            writer.atom("index-access");
            write_expr(writer, target);
            write_expr(writer, index);
        }
        Expr::Unary { op, expr } => {
            writer.atom("unary");
            write_unary_op(writer, *op);
            write_expr(writer, expr);
        }
        Expr::Binary { left, op, right } => {
            writer.atom("binary");
            write_binary_op(writer, *op);
            write_expr(writer, left);
            write_expr(writer, right);
        }
        Expr::JavaScriptUnary { op, expr } => {
            writer.atom("javascript:unary");
            writer.atom(&format!("{op:?}"));
            write_expr(writer, expr);
        }
        Expr::JavaScriptBinary { left, op, right } => {
            writer.atom("javascript:binary");
            writer.atom(&format!("{op:?}"));
            write_expr(writer, left);
            write_expr(writer, right);
        }
        Expr::JavaScriptLogical { left, op, right } => {
            writer.atom("javascript:logical");
            writer.atom(&format!("{op:?}"));
            write_expr(writer, left);
            write_expr(writer, right);
        }
        Expr::TypeLiteral(ty) => {
            writer.atom("type-literal");
            write_type(writer, ty);
        }
    }
}

#[cfg(test)]
mod tests;
