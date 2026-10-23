//! Scoped linking, inspection and typed admission outcomes.
//! Preparation does not publish or resolve deferred providers. Holds validate
//! and retain the complete content/provenance DAG before publication.
use super::engine::ProcessDocument;
use lash_core_store::artifact_referrer::ArtifactName;

use lash_vocabulary::{claims::*, names::*, operations::*, package::*, types::*};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Trusted admission role. Workflow attachment is explicit; package publication validates internal ownership.
pub enum LinkPurpose {
    Cell,
    Workflow { attachment: Attachment },
    PackageCode { path: PackagePath },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Reach for all Exports or one Main/function Export. Unused Export effects do not become start effects.
pub enum ReachSelector {
    AllExports,
    Start(StartSelector),
    Callable { function: FunctionId },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Immutable document, selected reach, accepted namespace and trusted admission purpose/context.
pub struct FulfilRequest {
    pub document: DocumentId,
    pub selector: ReachSelector,
    pub snapshot: NamespaceSnapshot,
    pub context: PublicationContext,
    pub purpose: LinkPurpose,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// A reached fixed requirement or unresolved grant; target provider debt is unknown until resolution.
pub enum OperationNeed {
    Fixed {
        name: QualifiedName,
        requirement: OperationRequirement,
    },
    Deferred {
        name: QualifiedName,
        grant: Box<DeferredGrant>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Runtime-computed linked reach, operation needs, machine debt and retained artifacts; Core sees no executable types.
pub struct LinkInspection {
    pub link: LinkId,
    pub document: DocumentId,
    pub selector: ReachSelector,
    #[serde(with = "lash_vocabulary::ordered_pairs")]
    pub operations: BTreeMap<OperationUse, OperationNeed>,
    pub machine: NodeRequirements,
    pub artifacts: Vec<ArtifactName>,
    pub signature: Option<Signature>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Prepared immutable Link content and inspection, not permission to publish arbitrary bytes.
pub struct Fulfilled {
    pub content: Link,
    pub inspection: LinkInspection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Content identity for trusted revalidation, loading and complete DAG retention.
pub enum ContentRef {
    Document(DocumentId),
    Link(LinkId),
    Package(PackageId),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Versioned opaque engine bytes. Persisted material never contains boxed Any.
pub struct EngineBlob {
    pub format: String,
    pub version: u32,
    pub bytes: Vec<u8>,
}
#[derive(Debug)]
/// Opaque runtime material loaded by identity. Only runtime-aware hosts downcast it.
pub struct LoadedContent {
    pub reference: ContentRef,
    pub material: ProcessDocument,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Typed deterministic admission refusal retained across the plugin/host boundary.
pub enum AdmissionRefusal {
    CallableIdentity {
        callable: lash_vocabulary::CallableId,
    },
    EntryAnnotation {
        source: Box<lash_vocabulary::EntryAnnotationRefusal>,
    },
    SignatureRequired {
        source: Box<lash_vocabulary::SignatureRequired>,
    },
    CallableTooLarge {
        bytes: u64,
        limit: u64,
    },
    UnpublishedOrigin {
        link: LinkId,
    },
    MissingImport {
        slot: ImportSlot,
        name: QualifiedName,
    },
    KindMismatch {
        slot: ImportSlot,
        name: QualifiedName,
    },
    SignatureMismatch {
        slot: ImportSlot,
        name: QualifiedName,
    },
    ConventionMismatch {
        slot: ImportSlot,
        name: QualifiedName,
    },
    NotVisible {
        name: QualifiedName,
    },
    PathConflict {
        path: PackagePath,
    },
    NotOwner {
        path: PackagePath,
    },
    UndeclaredDeferred {
        path: PackagePath,
    },
    ProviderNotHostable {
        provider: ProviderName,
        registry: RegistryKind,
    },
    MissingRoute {
        use_: OperationUse,
    },
    ContractMismatch {
        use_: OperationUse,
        pinned: ContractId,
        offered: ContractId,
    },
    MissingMain,
    NotFunction {
        export: ExportName,
    },
    NotDataOnly {
        export: ExportName,
    },
    InvalidDocument {
        message: String,
    },
    IncompatibleSuccession {
        message: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Typed refusal, artifact, fence, unsupported or infrastructure error. Infrastructure is never a guest terminal.
pub enum EnginePortError {
    PackageRefused(lash_core_store::packages::PackageRefusal),
    Refused(AdmissionRefusal),
    Missing {
        artifact: ArtifactName,
    },
    Corrupt {
        artifact: ArtifactName,
        message: String,
    },
    Fenced,
    Unsupported {
        operation: String,
    },
    /// A substrate fault retains its kind and fields across the engine port.
    StoreUnavailable {
        fault: crate::store::StoreFault,
    },
    /// A store refusal retains its structured cause across the engine port.
    StoreRefused(crate::store::StoreRefusal),
    Unavailable {
        message: String,
    },
}

impl std::fmt::Display for EnginePortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for EnginePortError {}

impl EnginePortError {
    /// Content/admission refusals cannot succeed on an identical retry; missing
    /// content, fences and service failures remain infrastructure outcomes.
    pub fn failure_class(&self) -> lash_protocol_types::ToolFailureClass {
        match self {
            Self::PackageRefused(_)
            | Self::Refused(_)
            | Self::Corrupt { .. }
            | Self::Unsupported { .. } => lash_protocol_types::ToolFailureClass::InvalidRequest,
            Self::StoreRefused(_) => lash_protocol_types::ToolFailureClass::Internal,
            Self::Missing { .. }
            | Self::Fenced
            | Self::StoreUnavailable { .. }
            | Self::Unavailable { .. } => lash_protocol_types::ToolFailureClass::Unavailable,
        }
    }
}

impl From<lash_durable::DurableError> for EnginePortError {
    fn from(error: lash_durable::DurableError) -> Self {
        use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
        match error {
            DurableError::Package(refusal) => Self::PackageRefused(refusal),
            DurableError::OwnershipLost(_) => Self::Fenced,
            DurableError::StoreRefused(refusal) => Self::StoreRefused(refusal),
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Contended,
                ..
            }) => Self::StoreUnavailable {
                fault: crate::store::StoreFault::Contended,
            },
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                message,
            }) => Self::StoreUnavailable {
                fault: crate::store::StoreFault::Backend { message },
            },
            error => Self::Unavailable {
                message: error.to_string(),
            },
        }
    }
}

impl From<EnginePortError> for crate::PluginError {
    fn from(error: EnginePortError) -> Self {
        match error {
            EnginePortError::StoreUnavailable { fault } => Self::StoreUnavailable { fault },
            EnginePortError::StoreRefused(refusal) => Self::StoreRefusal(refusal),
            error => Self::Package(Box::new(error)),
        }
    }
}
