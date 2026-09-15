use std::collections::{BTreeMap, BTreeSet, HashSet};
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
    AssignPathStep, AstString, BinaryOp, Declaration, Expr, LabelMetadata, ListComprehensionClause,
    ProcessDecl, Program, ResourceRefExpr, TypeExpr, UnaryOp,
};
use crate::linker::{
    LashlangAbilities, LashlangHostCatalog, LashlangLanguageFeatures, ResourceOperationBinding,
};

pub use lash_sansio::LASHLANG_SEMANTIC_HASH_VERSION;
pub const LASHLANG_COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const LASHLANG_VM_ABI_VERSION: &str = "lashlang-vm-abi-v8";

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
    pub canonical_ir: Program,
}

impl ModuleArtifact {
    /// Builds a raw module artifact from already-complete program IR.
    ///
    /// Source programs whose process output is inferred must go through the
    /// linker; this builder refuses an incomplete exported signature.
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
        Self::check_canonical_ir(&canonical_ir)?;
        let host_requirements_ref = host_requirements_ref(&requirements);
        let exports = module_exports(&canonical_ir);
        let module_ref = module_ref(&canonical_ir, &host_requirements_ref, &exports);
        Ok(Self {
            module_ref,
            host_requirements_ref,
            host_requirements: requirements,
            exports,
            canonical_ir,
        })
    }

    /// Refuses IR the compiler cannot lower, independently of its refs.
    ///
    /// Shared by the builder and by `verify` so an admission check sees exactly
    /// what construction does.
    fn check_canonical_ir(canonical_ir: &Program) -> Result<(), ModuleArtifactError> {
        crate::ast::validate_ast(canonical_ir)?;
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
        Ok(())
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

    pub fn introspect(
        &self,
    ) -> Result<crate::ModuleIntrospection, crate::ModuleIntrospectionError> {
        crate::ModuleIntrospection::from_artifact(self)
    }

    /// Refuses an artifact whose recorded refs do not match its own content.
    ///
    /// The refs are derived from borrowed content rather than by rebuilding the
    /// artifact: a rebuild cloned the whole canonical IR and the host
    /// requirements only to hash them, which made every publish cost a deep
    /// copy of the program (FIG-3088). What this refuses is unchanged - the
    /// same AST validation, the same incomplete-signature refusal, and the same
    /// three ref comparisons in the same order. Canonicalisation only clears the
    /// span tables, which neither `write_program` nor `validate_ast` reads, so
    /// deriving from the artifact's own IR is equivalent to deriving from a
    /// canonicalised copy of it.
    pub fn verify(&self) -> Result<(), ModuleArtifactError> {
        Self::check_canonical_ir(&self.canonical_ir)?;
        let derived_host_requirements_ref = host_requirements_ref(&self.host_requirements);
        let derived_exports = module_exports(&self.canonical_ir);
        let derived_module_ref = module_ref(
            &self.canonical_ir,
            &derived_host_requirements_ref,
            &derived_exports,
        );
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
        reject_future_shape(&raw)?;
        let artifact: Self = serde_json::from_slice(bytes).map_err(|err| {
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
            ModuleArtifactError::RetiredCompilationDialect => Self::Decode(value.to_string()),
            ModuleArtifactError::HashMismatch { .. } => Self::Decode(value.to_string()),
        }
    }
}

#[async_trait::async_trait]
pub trait LashlangArtifactStore: Send + Sync {
    /// Arm a one-shot conformance pause immediately before this backend's
    /// publication serialization point. Production callers never use this
    /// diagnostic seam; stores that participate in ownership conformance
    /// return a handle and pause their next publish until it is resumed.
    fn pause_next_publication_for_testing(&self) -> Option<ArtifactPublicationPause> {
        None
    }

    /// Durability tier this artifact store provides; defaults to [`DurabilityTier::Inline`].
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Inline
    }

    /// Publish an immutable module and retain it for one exact owner.
    async fn publish_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        artifact: &ModuleArtifact,
    ) -> Result<(), ArtifactStoreError>;

    /// Add an owner edge to an existing module artifact.
    async fn retain_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError>;

    /// Atomically add `to` and sever `from` for one module artifact.
    async fn transfer_module_artifact(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError>;

    /// Sever one exact owner edge and reclaim the module when it was the last.
    async fn release_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError>;

    /// Permanently fence an execution owner against late publication and sever
    /// every module edge it still owns.
    async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), ArtifactStoreError>;

    async fn get_module_artifact(
        &self,
        module_ref: &ModuleRef,
    ) -> Result<Option<Arc<ModuleArtifact>>, ArtifactStoreError>;
}

#[derive(Clone, Default)]
pub struct ArtifactPublicationPause {
    state: Arc<Mutex<ArtifactPublicationPauseState>>,
}

#[derive(Default)]
struct ArtifactPublicationPauseState {
    reached: bool,
    resumed: bool,
    writer_waker: Option<std::task::Waker>,
}

impl ArtifactPublicationPause {
    pub fn is_reached(&self) -> bool {
        self.state.lock_recover().reached
    }

    pub fn resume(&self) {
        let mut state = self.state.lock_recover();
        state.resumed = true;
        if let Some(waker) = state.writer_waker.take() {
            waker.wake();
        }
    }

    pub async fn pause(&self) {
        std::future::poll_fn(|context| {
            let mut state = self.state.lock_recover();
            state.reached = true;
            if state.resumed {
                std::task::Poll::Ready(())
            } else {
                state.writer_waker = Some(context.waker().clone());
                std::task::Poll::Pending
            }
        })
        .await
    }
}

#[derive(Clone, Default)]
pub struct InMemoryLashlangArtifactStore {
    state: Arc<Mutex<InMemoryArtifactState>>,
    publication_pause: Arc<Mutex<Option<ArtifactPublicationPause>>>,
}

#[derive(Default)]
struct InMemoryArtifactState {
    modules: BTreeMap<ModuleRef, Arc<ModuleArtifact>>,
    owners: HashSet<(ModuleRef, lash_core::ArtifactOwner)>,
    retired_owners: HashSet<lash_core::ArtifactOwner>,
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
    fn pause_next_publication_for_testing(&self) -> Option<ArtifactPublicationPause> {
        let pause = ArtifactPublicationPause::default();
        *self.publication_pause.lock_recover() = Some(pause.clone());
        Some(pause)
    }

    async fn publish_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        artifact: &ModuleArtifact,
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact.module_ref.as_str()) {
            return Err(ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        artifact.verify()?;
        let publication_pause = self.publication_pause.lock_recover().take();
        if let Some(pause) = publication_pause {
            pause.pause().await;
        }
        let mut state = self.state.lock_recover();
        if state.retired_owners.contains(owner) {
            return Err(ArtifactStoreError::Backend(
                "artifact owner has been permanently retired".to_string(),
            ));
        }
        if let Some(existing) = state.modules.get(&artifact.module_ref)
            && existing.as_ref() != artifact
        {
            return Err(ArtifactStoreError::Backend(format!(
                "module artifact `{}` is immutable",
                artifact.module_ref
            )));
        }
        state
            .modules
            .entry(artifact.module_ref.clone())
            .or_insert_with(|| Arc::new(artifact.clone()));
        state
            .owners
            .insert((artifact.module_ref.clone(), owner.clone()));
        Ok(())
    }

    async fn retain_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError> {
        let mut state = self.state.lock_recover();
        if state.retired_owners.contains(owner) {
            return Err(ArtifactStoreError::Backend(
                "artifact owner has been permanently retired".to_string(),
            ));
        }
        if !state.modules.contains_key(module_ref) {
            return Err(ArtifactStoreError::Backend(format!(
                "missing module artifact `{module_ref}`"
            )));
        }
        state.owners.insert((module_ref.clone(), owner.clone()));
        Ok(())
    }

    async fn transfer_module_artifact(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError> {
        let mut state = self.state.lock_recover();
        if state.retired_owners.contains(to) {
            return Err(ArtifactStoreError::Backend(
                "artifact destination owner has been permanently retired".to_string(),
            ));
        }
        let from_edge = (module_ref.clone(), from.clone());
        if !state.owners.contains(&from_edge) {
            if state.owners.contains(&(module_ref.clone(), to.clone())) {
                return Ok(());
            }
            return Err(ArtifactStoreError::Backend(format!(
                "module artifact `{module_ref}` is not retained by the staging owner"
            )));
        }
        state.owners.insert((module_ref.clone(), to.clone()));
        state.owners.remove(&from_edge);
        Ok(())
    }

    async fn release_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &ModuleRef,
    ) -> Result<(), ArtifactStoreError> {
        let mut state = self.state.lock_recover();
        state.owners.remove(&(module_ref.clone(), owner.clone()));
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
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), ArtifactStoreError> {
        if !matches!(owner, lash_core::ArtifactOwner::Execution(_)) {
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
        module_ref: &ModuleRef,
    ) -> Result<Option<Arc<ModuleArtifact>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref.as_str()) {
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

pub fn canonical_program_ir(mut program: Program) -> Program {
    program.declaration_spans.clear();
    program.expression_spans.clear();
    program.expression_source_spans.clear();
    normalize_local_binder_names(&mut program);
    program
}

/// The shape a normalized local binder name is rewritten to.
///
/// `#` is not an identifier character in any dialect that reaches the IR, so a
/// normalized binder can never collide with an authored name, a lifted process
/// declaration name, or a session global carried by name.
fn normalized_local_name(index: u32) -> AstString {
    AstString::from(format!("local#{index}"))
}

/// Rewrites every local binder name the module identity alpha-normalizes.
///
/// `module_ref` writes a local binder as `local:<index>` rather than as its
/// name (`NameNormalizer`), so two modules that differ only in a local name
/// share one module ref. The artifact stored under that ref must therefore not
/// carry the name either: every artifact store addresses a module by its ref
/// and refuses a second publish whose bytes differ, so a name the identity
/// drops but the bytes keep makes one ref name two byte strings and the second
/// session to run an alpha-variant cell fails its turn (FIG-3120). Spans are
/// already cleared above for exactly this reason; local names are the same
/// class of fact.
///
/// The walk mirrors the hash's unit by unit: a function body binds its params
/// as ABI names, a process body binds its params plus `input`/`inputs`, and
/// `main` starts empty — so a name that hashes as `local:<i>` here is the name
/// renamed here, and equal refs now carry equal bytes.
fn normalize_local_binder_names(program: &mut Program) {
    let mut declaration_locals = Vec::with_capacity(program.declarations.len());
    for declaration in &program.declarations {
        declaration_locals.push(match declaration {
            Declaration::Type(_) => LocalNames::default(),
            Declaration::Function(function) => {
                let mut normalizer = NameNormalizer::default();
                for param in &function.params {
                    normalizer.bind_abi(param.name.as_str());
                }
                normalizer.collect_expr(&function.body);
                normalizer.local_names()
            }
            Declaration::Process(process) => {
                let mut normalizer = NameNormalizer::default();
                for param in &process.params {
                    normalizer.bind_abi(param.name.as_str());
                }
                normalizer.bind_abi("input");
                normalizer.bind_abi("inputs");
                normalizer.collect_expr(&process.body);
                normalizer.local_names()
            }
        });
    }
    let main_locals = {
        let mut normalizer = NameNormalizer::default();
        normalizer.collect_expr(&program.main);
        normalizer.local_names()
    };

    for (declaration, locals) in program.declarations.iter_mut().zip(declaration_locals) {
        match declaration {
            Declaration::Type(_) => {}
            Declaration::Function(function) => rename_local_names(&mut function.body, &locals),
            Declaration::Process(process) => rename_local_names(&mut process.body, &locals),
        }
    }
    rename_local_names(&mut program.main, &main_locals);
}

/// Renames one name mention if the identity hashes it as a local.
fn rename_name(name: &mut AstString, locals: &LocalNames) {
    if let Some(&index) = locals.get(name.as_str()) {
        *name = normalized_local_name(index);
    }
}

/// Rewrites every name position `write_expr` passes through
/// `NameNormalizer::name_token`, then recurses through `children_mut`, which is
/// pinned to visit the same nodes `write_expr` does. A process literal's
/// parameter names are deliberately absent: the identity writes those verbatim,
/// so they are not local names.
fn rename_local_names(expr: &mut Expr, locals: &LocalNames) {
    if locals.is_empty() {
        return;
    }
    match expr {
        Expr::Variable(name) => rename_name(name, locals),
        Expr::Assign { target, .. } => rename_name(&mut target.root, locals),
        Expr::For { binding, .. } => rename_name(binding, locals),
        Expr::ListComprehension { clauses, .. } => {
            for clause in clauses {
                if let ListComprehensionClause::For { binding, .. } = clause {
                    rename_name(binding, locals);
                }
            }
        }
        Expr::Function(function) => {
            if let Some(name) = function.name.as_mut() {
                rename_name(name, locals);
            }
            for param in &mut function.params {
                rename_name(param, locals);
            }
            for capture in &mut function.captures {
                rename_name(capture, locals);
            }
        }
        Expr::Try(scope) => {
            if let Some(catch) = scope.catch.as_mut() {
                rename_name(&mut catch.binding, locals);
            }
        }
        _ => {}
    }
    for child in expr.children_mut() {
        rename_local_names(child, locals);
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

fn write_name_token(writer: &mut HashWriter, token: NameToken<'_>) {
    match token {
        NameToken::Abi(name) => writer.prefixed_atom("abi:", name),
        NameToken::Global(name) => writer.prefixed_atom("global:", name),
        NameToken::Local(index) => writer.numbered_atom("local:", u64::from(index)),
    }
}

fn write_expr<'program>(
    writer: &mut HashWriter,
    expr: &'program Expr,
    normalizer: &NameNormalizer<'program>,
) {
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
        Expr::ProcessLiteral(literal) => {
            writer.atom("process-literal");
            for param in &literal.params {
                writer.atom("param");
                writer.atom(param.name.as_str());
                write_type(writer, &param.ty);
            }
            write_expr(writer, &literal.body, normalizer);
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
            write_name_token(writer, normalizer.name_token(name.as_str()));
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
                        write_name_token(writer, normalizer.name_token(binding.as_str()));
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
            write_name_token(writer, normalizer.name_token(target.root.as_str()));
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
            write_name_token(writer, normalizer.name_token(binding.as_str()));
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
        Expr::ResultUnwrap(expr) => write_unary_expr(writer, "unwrap", expr, normalizer),
        Expr::Print(expr) => write_unary_expr(writer, "print", expr, normalizer),
        Expr::Yield(expr) => write_unary_expr(writer, "yield", expr, normalizer),
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
                Some(name) => write_name_token(writer, normalizer.name_token(name.as_str())),
                None => writer.atom("anonymous"),
            }
            writer.usize(function.params.len());
            for param in &function.params {
                write_name_token(writer, normalizer.name_token(param.as_str()));
            }
            writer.usize(function.captures.len());
            for capture in &function.captures {
                write_name_token(writer, normalizer.name_token(capture.as_str()));
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
                    write_name_token(writer, normalizer.name_token(catch.binding.as_str()));
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
mod tests;

/// The local-bound names of one hashing unit, as `name -> local index`.
type LocalNames = rustc_hash::FxHashMap<String, u32>;

/// One name's hashed identity, held as a description rather than as a rendered
/// string.
///
/// Rendering the token (`abi:x`, `local:3`, `global:x`) allocated once per
/// binding and once per mention, which is the bulk of what hashing a module
/// cost (FIG-3088). `HashWriter` writes the same bytes from the parts.
#[derive(Clone, Copy)]
enum NameToken<'program> {
    Abi(&'program str),
    Local(u32),
    Global(&'program str),
}

/// Name bindings are looked up, never iterated, so the map is unordered: a
/// `BTreeMap` allocates a ~KiB node for the handful of names one scope binds,
/// which showed up directly in the artifact roundtrip's byte budget
/// (FIG-3088). The hashed bytes do not depend on the map's order - the local
/// index a name gets is assigned in binder order by `next_local`.
#[derive(Default)]
struct NameNormalizer<'program> {
    names: rustc_hash::FxHashMap<&'program str, NameToken<'program>>,
    next_local: u32,
}

impl<'program> NameNormalizer<'program> {
    fn bind_abi(&mut self, name: &'program str) {
        self.names.insert(name, NameToken::Abi(name));
    }

    fn bind_local(&mut self, name: &'program str) {
        // An ABI name is in `names` too, so one lookup covers both.
        if self.names.contains_key(name) {
            return;
        }
        let token = NameToken::Local(self.next_local);
        self.next_local += 1;
        self.names.insert(name, token);
    }

    /// The local-bound names this unit carries, owned, for the canonical-IR
    /// rewrite that has to drop exactly the names the hash drops.
    fn local_names(&self) -> LocalNames {
        self.names
            .iter()
            .filter_map(|(name, token)| match token {
                NameToken::Local(index) => Some(((*name).to_string(), *index)),
                NameToken::Abi(_) | NameToken::Global(_) => None,
            })
            .collect()
    }

    /// The hashed token for one name reference.
    fn name_token(&self, name: &'program str) -> NameToken<'program> {
        self.names
            .get(name)
            .copied()
            .unwrap_or(NameToken::Global(name))
    }

    fn collect_expr(&mut self, expr: &'program Expr) {
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
