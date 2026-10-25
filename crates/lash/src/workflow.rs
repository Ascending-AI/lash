//! Author, inspect and run linked kernel Documents.
//!
//! A host supplies a trusted [`WorkflowEnvironmentRequest`](crate::vm::WorkflowEnvironmentRequest): accepted publications,
//! its consumer context and explicit attachments. Import lowers source against
//! that namespace. Publication fulfils the Document once, validates the selected
//! Main or function Export and retains its Link under a host artifact pin.
//!
//! `HostArtifacts::publish_workflow` returns the immutable `LinkedStart`.
//! `HostArtifacts::start_linked` stages it with an explicit process environment,
//! settings and initial resolutions. Starting linked code preserves its defining
//! Link; uploading an unlinked Document fulfils it anew against the supplied
//! namespace. Processes run through the engine's normal drive.
//!
//! Workflow views and execution overlays remain projections of retained code
//! and recorded observations. Source maps and captions are separate sidecars.
//!
//! `HostArtifacts::publish_callable` returns a saved `ProcessDefinition` retaining
//! its frozen captures. Select routes with `HostArtifacts::link_process_env` and
//! [`LinkTarget::Definition`](crate::process::LinkTarget::Definition), publish the
//! environment, then start the definition through [`Processes::start`](crate::process::Processes::start)
//! with [`ProcessStartRequest::definition`](crate::process::ProcessStartRequest::definition)
//! and an [`EngineLaunch`](crate::process::EngineLaunch) carrying that environment.

#[cfg(feature = "codemode")]
pub(crate) mod import;
#[cfg(feature = "codemode")]
pub use import::{
    WorkflowImport, WorkflowImportUnsupported, WorkflowSourceDiagnostic,
    WorkflowSourceDiagnosticFact, WorkflowSourceDiagnosticKind,
};

/// The kernel document: its statements, expressions, types, data, sites
/// and identities, and its canonical encoding.
///
/// [`DocumentDeclarationRefusal`] explains why the held library declarations
/// do not match the functions a document uses.
///
/// # Kernel text
///
/// A document is one whole workflow: the `kernel` and `numbers` header
/// lines, an `entry` signature for each declared function a host may start,
/// and `main`. [`parse_document`] reads one and [`print_document`] writes
/// the canonical text that parses back to it. Statements end at the line;
/// kernel text has no `;`:
///
/// ```rust
#[doc = include_str!("workflow/document_example.rs")]
/// ```
///
/// The example is `src/workflow/document_example.rs`, run by the
/// `the_document_module_docs_example_runs` law in `tests/workflow_laws.rs`;
/// the workspace builds no doctests, so that test is its proof.
pub mod document {
    // Observation inputs belong to `view`; data limits belong to `persistence`.
    // An explicit vocabulary keeps those names out of this module.
    // Helper images remain nameable for viewers without `codemode`; workflow
    // references belong beside the checked `WorkflowDocument` in `workflow`.
    pub use lash_kernel_doc::{
        ActExpr, ActKind, Action, AnnotationOrigin, AnnotationResolutionReason, Args, AssignOrder,
        Atom, Block, BodyCensus, BuildHelpers, Bytes, BytesView, CallablePresentation, Callee,
        CaptureValue, Catch, CheckedDeclarations, Closure, Correspondence,
        DOCUMENT_GRAMMAR_VERSION, Datum, DeclarationRefusal as DocumentDeclarationRefusal,
        DecodeError, DecodeRefusal, Decoder, Document, DocumentBytes, DocumentBytesRefusal,
        DocumentChecks, EncodeError, EntryAnnotationRefusal, EntrySignatureRefusal,
        EntryTypeMismatch, ErrorDatum, Export, Expr, FIRST_NATIVE_VERSION, FUNCTION_VALUE_ENTRY,
        FUNCTION_VALUE_TAG, Float, Formula, FrozenDatum, FunctionBody, FunctionCatalog,
        FunctionDeclarations, FunctionDefinition, FunctionExport, FunctionHeader, FunctionLocation,
        FunctionRegistry, FunctionRepresentation, FunctionTarget, FunctionValue,
        FunctionValueLimits, FunctionValueRefusal, Guard, GuardExceeded, Handle,
        IdentifiedDocument, Identity, ImageError, Implementation, ImplementationKind, Integer,
        Invalid, InvalidBytes, InvalidFloat, InvalidInteger, InvalidNumberToken, InvalidReason,
        JoinMode, KERNEL_VERSION, KernelVersion, LibraryDependencies, Literal, MAX_NESTING_DEPTH,
        MapEntry, Measure, Member, Metering, MissingHelper, NativeCall, NativeCtor, NativeError,
        NativeFunction, NewObject, Node, NodeMut, NumberPolicy, NumberToken, Nursery, Operand,
        Ordinals, ParseError, ParseErrorReason, Place, PreparedAnswer, PreparedNative, Protocol,
        RecordEntry, RegisteredFunction, RegistryError, ReleasedFunctions, Reservation,
        RestoreSelection, Retry, Rhs, Selection, Semantics, SequenceView, SignatureOrigin,
        SignaturePosition, SignatureRequired, SourceLanguage, SourceMap, SourceMapRefusal,
        SourceSite, Span, StartConvention, StatementForm, StatementRuleViolation, Stmt, Survivor,
        TableView, TaskId, TextView, Timestamp, TryStmt, TypeDef, TypeMember, TypeMismatch,
        ValidatedFunctions, Value, ValueKind, WorkCounter, WrittenSignature, arena,
        check_declarations, check_statement, core_support, document_work, heap, parse_definition,
        parse_definition_with_limits, parse_document, parse_document_with_limits, plan,
        print_definition, print_document, require_entry_signature, required_declarations, scalar,
        validate_definition, validate_definition_with_limits, validate_document,
        validate_document_with_limits,
    };
    pub use lash_vocabulary::names::{DocumentId, FunctionId, FunctionName, QualifiedName};
    pub use lash_vocabulary::names::{
        EffectName, InvalidIdentity, InvalidQualifiedName, SourceMapId,
    };
    pub use lash_vocabulary::operations::{ContractId, OperationRequirement};
    pub use lash_vocabulary::sites::{
        EffectId, LoopCounts, ObservationReader, PositionCounts, PositionLabel, Site, TaskLabel,
        Unit, VisitId, WaitId,
    };

    pub use lash_kernel_doc::linked_site::LinkedSite;
    /// Plain-language captions for a document's steps, its entries'
    /// parameters and the fields of their types, held beside it like its
    /// source map (`K-CAP-001`..`K-CAP-007`).
    pub use lash_kernel_doc::{
        CAPTION_DESCRIPTION_BYTES, CAPTION_TITLE_BYTES, Caption, CaptionEntry, CaptionField,
        CaptionKind, CaptionRefusal, CaptionTarget, Captions, CaptionsRefusal, FieldPath,
        FieldRoot, FieldTarget, ParamTarget, Rekeyed,
    };
    pub use lash_vocabulary::sites::testkit;
    pub use lash_vocabulary::tokens;
    pub use lash_vocabulary::tokens::{
        FROZEN_DATUM, FROZEN_JSON, FROZEN_TOOL_VALUE, MAX_DECODE_DEPTH,
    };
}

/// The total view of a document and its admission: nodes, edges, scopes,
/// type facets, effect sets, execution sites, and the refusals of a
/// document that is not admitted.
pub mod graph {
    pub use lash_kernel_check::*;
}

/// Editing a document: drafts, edits, transactions, refusals and the
/// correspondence an applied transaction answers.
#[cfg(feature = "workflow-authoring")]
pub mod edit {
    pub use lash_kernel_edit::{
        Applied, Draft, EditDiagnostic, EditDiagnosticKind, EditRefusal, Extraction, Location,
        NodeClass, Position, Transaction, extract_entry,
    };
}

/// Compose expression-valued effect insertions and direct edits into a
/// canonical transaction, without admission, publication or execution.
#[cfg(feature = "workflow-authoring")]
pub use lash_kernel_edit::authoring;

/// The canvas read of a document's entry: statements, scopes, dataflow and
/// execution sites, as serialisable data with no presentation.
///
/// [`TYPESCRIPT_DECLARATION`] ships the `kernel-workflow-view` schema's
/// generated `.d.ts` inside the crate, so a host that vendors lash can check
/// its own TypeScript copy hermetically against the same bytes. [`JSON_SCHEMA`]
/// ships the authoritative JSON Schema for validation and constraints.
///
/// [`derive`] reads a checked [`WorkflowDocument`](super::WorkflowDocument).
/// Document exports and declarations supply the names and signatures; hosts
/// join their own labels separately.
#[cfg(feature = "workflow-view")]
pub mod view {
    pub use lash_kernel_view::ActionActivity;
    pub use lash_kernel_view::ActionObservation;
    pub use lash_kernel_view::ActionRole;
    pub use lash_kernel_view::ActionState;
    pub use lash_kernel_view::ActionView;
    pub use lash_kernel_view::ActivityRefusal;
    pub use lash_kernel_view::ArgumentView;
    pub use lash_kernel_view::AssignmentView;
    pub use lash_kernel_view::BindingKind;
    pub use lash_kernel_view::BindingView;
    pub use lash_kernel_view::CallableView;
    pub use lash_kernel_view::CalleeView;
    pub use lash_kernel_view::ComparisonView;
    pub use lash_kernel_view::ControlArm;
    pub use lash_kernel_view::ControlHeader;
    pub use lash_kernel_view::Dataflow;
    pub use lash_kernel_view::DurationConversion;
    pub use lash_kernel_view::EntryParamView;
    pub use lash_kernel_view::EntryView;
    pub use lash_kernel_view::ExecutionSiteView;
    pub use lash_kernel_view::ExpressionView;
    pub use lash_kernel_view::FlowEdge;
    pub use lash_kernel_view::FlowKind;
    pub use lash_kernel_view::FlowTarget;
    pub use lash_kernel_view::HeaderRole;
    pub use lash_kernel_view::JSON_SCHEMA;
    pub use lash_kernel_view::LazyArm;
    pub use lash_kernel_view::LineRange;
    pub use lash_kernel_view::NodeCoverage;
    pub use lash_kernel_view::PathSite;
    pub use lash_kernel_view::PathStep;
    pub use lash_kernel_view::ReadableLens;
    pub use lash_kernel_view::ReadableRefusal;
    pub use lash_kernel_view::ReturnView;
    pub use lash_kernel_view::ScopeCoverage;
    pub use lash_kernel_view::ScopeKind;
    pub use lash_kernel_view::ScopeView;
    pub use lash_kernel_view::SiteKind;
    pub use lash_kernel_view::StatementCoverage;
    pub use lash_kernel_view::StatementView;
    pub use lash_kernel_view::TYPESCRIPT_DECLARATION;
    pub use lash_kernel_view::Use;
    pub use lash_kernel_view::ValueRef;
    pub use lash_kernel_view::ValueSource;
    pub use lash_kernel_view::ViewContext;
    pub use lash_kernel_view::ViewError;
    pub use lash_kernel_view::WorkflowCoverage;
    pub use lash_kernel_view::WorkflowNode;
    pub use lash_kernel_view::WorkflowNodeKind;
    pub use lash_kernel_view::WorkflowView;
    pub use lash_kernel_view::coverage;
    pub use lash_kernel_view::derive;
    pub use lash_kernel_view::derive_callable;
    pub use lash_kernel_view::path;
    /// A view's captions, joined by view location ([`WorkflowView::captions`]).
    pub use lash_kernel_view::{
        ArmCaption, BindingCaption, CallableCaption, CaptionView, CaptionViewRefusal, FieldCaption,
        NodeCaption, ParamCaption,
    };
    pub use lash_kernel_view::{Comparison, LabelError, MemberDisplay};
    pub use lash_vocabulary::observation::RanCounts;
    pub use lash_vocabulary::observation::ReplayPath;
    pub use lash_vocabulary::observation::TaskPath;
}

/// A checked document entered at one entry, with its graph and the library
/// declarations it linked against: what a host reads, draws and admits.
pub use lash_kernel_check::{WorkflowDocument, WorkflowDocumentError};
pub use lash_kernel_doc::WorkflowReference;
pub use lash_trace::{
    DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT, WorkflowExecutionOverlay,
    WorkflowExecutionOverlayAccumulator, WorkflowOverlayCall, WorkflowOverlayChildLink,
    WorkflowOverlayConflict, WorkflowOverlayConflictKind, WorkflowOverlayCoverage,
    WorkflowOverlayDocument, WorkflowOverlayDocumentBinding, WorkflowOverlayEventIdentity,
    WorkflowOverlayExecutionTransition, WorkflowOverlayFact, WorkflowOverlayFoldError,
    WorkflowOverlayHistoryEvent, WorkflowOverlayMismatch, WorkflowOverlayNodeTransition,
    WorkflowOverlayOccurrence, WorkflowOverlaySettlement, WorkflowOverlaySite,
    WorkflowOverlaySiteReport, WorkflowOverlaySiteRetention, WorkflowOverlaySiteState,
    WorkflowOverlayTerminal, WorkflowOverlayTerminalRecord, WorkflowOverlayTerminalStatus,
    WorkflowTaskSite, fold_workflow_overlay,
};
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::trace_adapter::workflow_overlay_document;
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{
    ReplaySegments, SegmentAnswer, SegmentReplay, Unreplayable, WorkflowEnvironment,
};
/// Which document an execution runs, as a process's observation snapshot
/// ([`ProcessDocumentIdentity`](crate::process::ProcessDocumentIdentity))
/// and an execution's start name it.
pub use lash_vocabulary::names::{LinkedStart, StartSelector};

use crate::Result;
#[cfg(feature = "codemode")]
use crate::persistence::ArtifactName;
#[cfg(feature = "codemode")]
use crate::process::{ProcessDefinition, ProcessDefinitionId, ProcessEngineKind};
#[cfg(feature = "codemode")]
use lash_vocabulary::identity::ProcessId;

#[cfg(feature = "codemode")]
/// One definition as a host inspects it.
#[derive(Clone, Debug)]
pub struct WorkflowInspection {
    /// The content-derived id and the signature of the entry the
    /// definition starts.
    pub definition: ProcessDefinition,
    /// The engine that owns the definition.
    pub engine_kind: ProcessEngineKind,
    /// The document the definition runs, entered at its entry.
    pub document: WorkflowDocument,
}

#[cfg(feature = "codemode")]
/// A workflow inspection, or why there is none.
#[derive(Clone, Debug)]
pub enum WorkflowRead {
    Inspected(Box<WorkflowInspection>),
    /// Nothing retains what the read needs any more.
    Unavailable(WorkflowUnavailable),
    /// The engine has no workflow document: it was registered with no
    /// document provider, or it is a core built-in with no definition.
    Unsupported {
        engine_kind: ProcessEngineKind,
    },
}

#[cfg(feature = "codemode")]
/// The document an execution names, or why there is none.
#[derive(Clone, Debug)]
pub enum WorkflowDocumentRead {
    /// The document entered where the reference says, held with the
    /// declarations it was stored with, and the document it was rewritten
    /// from with the rewrite's correspondence when a migration or a
    /// successor carry made it.
    Read(Box<WorkflowDocument>),
    /// Nothing retains the document the reference names any more.
    Unavailable(WorkflowUnavailable),
    /// This core has no engine that reads workflow documents.
    Unsupported,
}

#[cfg(feature = "codemode")]
/// What a workflow read found no longer retained.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkflowUnavailable {
    /// No row is retained under the process id.
    Process { process_id: ProcessId },
    /// No referrer holds the definition's descriptor.
    Definition { definition_id: ProcessDefinitionId },
    /// No referrer holds an artifact the definition reads.
    Artifact { artifact: ArtifactName },
    /// An artifact the read needs is retained with bytes that are not the
    /// artifact its name says: a declaration that is not the function its
    /// identity names, or a map that is not its source's.
    Corrupt {
        artifact: ArtifactName,
        message: String,
    },
}

#[cfg(feature = "codemode")]
impl WorkflowUnavailable {
    pub(crate) fn of(unretained: lash_vm_runtime::Unretained) -> Self {
        match unretained {
            lash_vm_runtime::Unretained::Missing { artifact } => Self::Artifact { artifact },
            lash_vm_runtime::Unretained::Corrupt { artifact, message } => {
                Self::Corrupt { artifact, message }
            }
        }
    }
}

#[cfg(feature = "codemode")]
/// A published source map and the source it maps, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceMapRead {
    Read {
        map: document::SourceMap,
        source: String,
    },
    Unavailable(WorkflowUnavailable),
}

#[cfg(feature = "codemode")]
/// Read the definition `payload` names through the `engine_kind` engine's
/// document provider.
pub(crate) async fn read(
    engines: &lash_core::ProcessEngineRegistry,
    engine_kind: &ProcessEngineKind,
    payload: &serde_json::Value,
) -> Result<WorkflowRead> {
    let unsupported = || WorkflowRead::Unsupported {
        engine_kind: engine_kind.clone(),
    };
    let Some(provider) = engines.document_provider(engine_kind.as_str()) else {
        return Ok(unsupported());
    };
    let inspected = match provider.document(payload).await? {
        lash_execution_contracts::process::ProcessDocumentRead::Inspected(inspected) => *inspected,
        lash_execution_contracts::process::ProcessDocumentRead::ArtifactMissing { artifact } => {
            return Ok(WorkflowRead::Unavailable(WorkflowUnavailable::Artifact {
                artifact,
            }));
        }
        lash_execution_contracts::process::ProcessDocumentRead::ArtifactCorrupt {
            artifact,
            message,
        } => {
            return Ok(WorkflowRead::Unavailable(WorkflowUnavailable::Corrupt {
                artifact,
                message,
            }));
        }
    };
    let Ok(document) = inspected.document.downcast::<WorkflowDocument>() else {
        return Ok(unsupported());
    };
    Ok(WorkflowRead::Inspected(Box::new(WorkflowInspection {
        definition: inspected.definition,
        engine_kind: engine_kind.clone(),
        document,
    })))
}

/// Process overlays consume the lifecycle events hosts store and the same
/// process feed's live entries. The trace overlay above serves session cells,
/// which have no process log. Process observation snapshots include this fold.
pub use lash_core::ProcessWorkflowFoldError;
pub use lash_core::ProcessWorkflowLiveCoverage;
pub use lash_core::ProcessWorkflowOverlay;
pub use lash_core::ProcessWorkflowOverlayAccumulator;
pub use lash_core::ProcessWorkflowRan;
pub use lash_core::ProcessWorkflowSite;
pub use lash_core::ProcessWorkflowSiteState;
pub use lash_core::ProcessWorkflowWaitKind;

/// Immutable linked content and validation used by document hosts and editors.
#[cfg(any(feature = "codemode", feature = "workflow-view"))]
pub mod link {
    pub use lash_kernel_package::{
        LinkRefusal, LinkedContent, LinkedReach, Namespace as LinkNamespace,
        OperationNeed as LinkedOperationNeed, ReachSelection, RetainedContent,
    };
}

/// A saved workflow opened as independently publishable ordinary content.
#[cfg(all(feature = "codemode", feature = "workflow-authoring"))]
#[derive(Clone, Debug)]
pub struct OpenedWorkflow {
    pub draft: authoring::Draft,
    pub selector: lash_vocabulary::StartSelector,
    pub captions: document::Captions,
    pub dropped: Vec<document::CaptionTarget>,
}
#[cfg(feature = "codemode")]
pub use lash_kernel_package::{MaterializeRefusal, UnrepresentableValue};
