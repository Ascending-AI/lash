//! Workflows as kernel documents a host reads, draws, edits and publishes.
//!
//! Code mode (`codemode`) runs and publishes workflows without the RLM REPL
//! protocol. Drawing is `workflow-view` and editing is `workflow-authoring`:
//! each is a separate opt-in, and neither links a dialect parser. A host
//! that prints or lowers source names `typescript` or `python` too.
//!
//! A process is an entry function of an admitted **kernel document**
//! ([`document::Document`]): a typed tree of statements and expressions
//! with a manifest of what it needs, the functions it declares, and the
//! entries a host may start. The document is the only authority. It holds
//! no source text, no node ids and nothing derived.
//!
//! # Reading
//!
//! [`Processes::graph`](crate::process::Processes::graph) and
//! [`HostArtifacts::definition_graph`](crate::process::HostArtifacts::definition_graph)
//! answer a [`WorkflowRead`]: the definition's identity and signature
//! beside its [`WorkflowDocument`], or the typed reason there is none. A
//! [`WorkflowDocument`] is the kernel document with the total view the
//! checker derives from it ([`graph::Graph`]): a node for every statement
//! and expression, control and data edges, scopes and bindings, a type
//! facet and an effect set for each node, and every execution site. The
//! view is derived again on every read. A host draws it; it never stores
//! or edits it.
//!
//! With `workflow-view`, `view::derive` is the serialisable read a canvas
//! draws from one [`WorkflowDocument`]: its entry's statements with their
//! sites, the blocks a host can select with what opens each, the dataflow
//! ports and wires are drawn from, and every execution site. It is lash's
//! wire shape (the `kernel-workflow-view` host schema); layout and styling
//! stay the host's. A library callee names its identity and the name the
//! document's `use` line gives it; `lash::helper_labels::labels` reads the shipped
//! display labels and `helper_labels::title` joins one by that name, falling back to the bare
//! name for a helper this build does not know. A [`WorkflowDocument`] holds
//! the library declarations it was linked against, so the view needs no
//! other catalog. A document's `use` lines carry names and ids but no
//! signatures: the runtime can ship
//! [`document::FunctionRegistry::export_declarations`] as serde data, and a
//! separate reader decodes [`document::FunctionDeclarations`], narrows it with
//! [`document::required_declarations`] and holds the document with
//! [`WorkflowDocument::new`], which checks every declaration against its
//! identity first. This needs no engine object, native implementation or
//! dialect.
//!
//! A place in a document is a [`document::Site`]: the unit (`main`, a
//! declared function or a library function) and the path of child indexes
//! to the node. What runs is named by an [`document::EffectId`]: the
//! task that ran it, the site, its occurrence in that task and the loop
//! iterations around it. A fan-out spawns one task for each element, so
//! each element's run of one site has its own identity.
//!
//! # Editing
//!
//! [`HostArtifacts::workflow_environment`](crate::process::HostArtifacts::workflow_environment)
//! answers the [`WorkflowEnvironment`] a document is checked against: the
//! effects its processes would be offered and the library functions the
//! workers hold. With `workflow-authoring`, an `edit::Draft` opens a document
//! and applies `authoring::Operation`s in atomic `edit::Transaction`s against a base
//! identity. Each transaction of values, effect steps, if/for/try wrappers
//! and conditions is admitted whole against the environment or changes nothing. A
//! refusal names the edit and the site (`edit::EditRefusal`).
//!
//! Canvas operations carry their base identity. SetValue accepts only data;
//! step and wrapper operations reject nonseparable actions. Other changes go
//! through whole-source replacement. `edit::extract_entry` atomically returns
//! an admitted entry slice and its effect requirements.
//! `edit::Draft::relink` resolves library names against a host-selected
//! `document::FunctionRegistry` and replaces the complete manifest closure in
//! one transaction. Admit the result against the runtime environment before use.
//!
//! A workflow definition id is a content address. Holding or copying one proves
//! content only, not possession. Lash performs no possession check; the host
//! authorizes who may create or run a workflow from an id.
//!
//! Sites are structural paths, so they change under edits. Continuity is
//! the [`document::Correspondence`] each applied transaction answers: where
//! every surviving node of the base sits in the result, and whether an
//! edit touched it. A host keys its layout by site and carries it across
//! with the correspondence.
//!
//! # Publishing
//!
//! [`HostArtifacts::publish_workflow`](crate::process::HostArtifacts::publish_workflow)
//! admits opaque [`document::DocumentBytes`] as a definition of one entry.
//! Import returns these bytes; a host retaining and running them needs no
//! typed tree or JSON. Receipt validates grammar, tokens, limits and id.
//! Reading/editing converts explicitly with `DocumentBytes::to_document`;
//! turn a completed edit back into bytes with `DocumentBytes::from`. The
//! checker admits the document against the environment a process of it
//! would run under; lash stores the document under its identity, with the
//! declaration of every library function it lists under that function's
//! identity, and publishes the selected entry's descriptor under the host's
//! pin. A function passed to a tool crosses inline as a small document with
//! frozen captures; a child start realizes its own selected descriptor. Every later read links the document against those retained
//! declarations, never against the reading build's library. A
//! [`WorkflowPublication`] names the admitted definition, the entry reference and
//! the id of the source map published with it, if any. A refused document
//! ([`WorkflowAdmissionRefusal`]) or map publishes nothing.
//!
//! A definition names its document by identity. A process keeps the
//! document it was admitted under: publishing an edited document creates a
//! new definition and changes no running process.
//!
//! A definition id is a content address, not a permission: lash does not check
//! that the caller ran or holds the definition; authorizing creation from an id
//! is the host's decision.
//!
//! # Following a run
//!
//! A process observation snapshot includes a [`ProcessWorkflowOverlay`]:
//! site states, occurrence counts and each task's last entered site. It uses
//! [`ProcessWorkflowOverlayAccumulator`], which also folds the typed process
//! events a host stores and the payloads on its process feed. Site entries
//! remain visible through pure computation; committed outcomes and the
//! process's terminal govern completion. Read labels and edges from the
//! process's document, never from the overlay.
//! A host folding only stored events constructs the accumulator with the
//! process id alone and calls `committed`; no document read is needed. Live
//! payloads supply their expected document reference to `observe`. For a stored
//! projected terminal, call `finish` instead of decoding it as a durable event.
//!
//! Each site's `first_entered` and `last_settled` identify committed event
//! sequences across its occurrences. Re-entry keeps the first entry and the
//! previous settlement until another occurrence settles. Live-only evidence
//! leaves both `None`; a live completion or terminal-unobserved state supplies
//! no committed settlement. Lash stores no wall time in the overlay: map these
//! sequences to `occurred_at_ms` using the process events you already hold.
//!
//! A run commits a fact for its effects and waits, never for a library
//! call: a library call's coverage count is the only durable fact about it,
//! and its overlay row, when the live feed reported one, is never settled
//! (FIG-6810). A process whose body ends in an error no `catch` took fails
//! with the `UncaughtError` cause: the error's kind, and the site that
//! raised it, a wait action or the statement a library call raised in; the
//! failure's message is the error's own and its raw value the error's data.
//!
//! Session cells and an effect's nested language execution have separate
//! execution identities. Their richer provisional history remains a
//! [`WorkflowExecutionOverlay`], built with
//! [`WorkflowExecutionOverlayAccumulator`] or [`fold_workflow_overlay`].
//! Give that reducer [`WorkflowOverlayDocument::of`] the document read through
//! [`HostArtifacts::execution_document`](crate::process::HostArtifacts::execution_document).
//!
//! # Source
//!
//! No part of reading, editing or publishing prints or parses a language.
//! [`HostArtifacts::import_workflow`](crate::process::HostArtifacts::import_workflow)
//! lowers a whole workflow module, TypeScript or Python, in the worker and
//! admits it against the same environment as publication. Both dialects
//! implement one contract ([`crate::dialect::workflow`]): every top-level
//! async function, by declaration or by `const` (TypeScript) or `async def`
//! (Python), exported or not, is an entry, and one declared inside a block is
//! ordinary code. An entry captures nothing of the module; its signature is
//! what its annotations mean in its dialect, with no defaults; and the
//! contract's refusals carry the same codes in both. [`WorkflowImport`]
//! distinguishes the imported document with its source map and source,
//! front-end diagnostics with source spans, admission refusals and typed
//! unsupported reasons. Import writes no artifact and runs no effect.
//!
//! With the matching `typescript` or `python` feature,
//! `lash::typescript::print(document, captions, options)` or
//! `lash::python::print` returns plain source, only the
//! code an agent could have written, an optional disposable map, and the
//! captions the dialect could not spell (`PrintedSource::dropped`). The
//! environment (effect manifest, number mode, entry signatures, private
//! bindings, helper identities and name renames) stays in the document and
//! is never printed. Printing does not compile or admit it. Hosts read the
//! source back, edited or not, against the document it was printed from:
//! `lash::typescript::read(source, &base, limits)` or `lash::python::read`,
//! and compare `Document::canonical_bytes()` to check reconstruction.
//! [`HostArtifacts::import_exported_workflow`](crate::process::HostArtifacts::import_exported_workflow)
//! reads and admits in one step. `vm::WorkerService` exposes separate `print`
//! and `read_exported` operations in a worker. Read documents still require
//! normal admission before execution; maps are never stored automatically.

#[cfg(feature = "codemode")]
pub(crate) mod import;
#[cfg(feature = "codemode")]
pub use import::{
    WorkflowImport, WorkflowImportUnsupported, WorkflowSourceDiagnostic,
    WorkflowSourceDiagnosticKind,
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
    pub use lash_kernel_doc::{
        ActExpr, ActKind, Action, Args, AssignOrder, Atom, Block, BodyCensus, Bytes, BytesView,
        Callee, Catch, CheckedDeclarations, Closure, ContractId, Correspondence,
        DOCUMENT_GRAMMAR_VERSION, Datum, DeclarationRefusal as DocumentDeclarationRefusal,
        DecodeError, DecodeMismatch, Decoder, Document, DocumentBytes, DocumentBytesRefusal,
        DocumentId, EffectId, EffectName, EncodeError, Entry, ErrorDatum, Expr,
        FIRST_NATIVE_VERSION, FROZEN_DATUM, FROZEN_JSON, FROZEN_TOOL_VALUE, FUNCTION_VALUE_ENTRY,
        FUNCTION_VALUE_TAG, Float, Formula, FrozenDatum, Function, FunctionBody, FunctionCatalog,
        FunctionDeclarations, FunctionDefinition, FunctionId, FunctionName, FunctionRegistry,
        FunctionRepresentation, FunctionValue, FunctionValueLimits, FunctionValueRefusal, Guard,
        GuardExceeded, Handle, IdentifiedDocument, Identity, ImageError, Implementation, Integer,
        Invalid, InvalidBytes, InvalidFloat, InvalidIdentity, InvalidInteger, InvalidNumberToken,
        InvalidQualifiedName, InvalidReason, JoinMode, KERNEL_VERSION, KernelVersion, Literal,
        LoopCounts, MAX_DECODE_DEPTH, MAX_NESTING_DEPTH, Manifest, MapEntry, MapType, Measure,
        Member, Name, NativeCall, NativeError, NativeFunction, NewObject, Node, NodeMut,
        NumberPolicy, NumberToken, Nursery, ObservationReader, Operand, OperationRequirement,
        Ordinals, Param, ParseError, ParseErrorReason, Place, PositionCounts, PositionLabel,
        PreparedAnswer, PreparedNative, ProjectionRead, Protocol, QualifiedName, RecordEntry,
        RecordType, RecordTypeField, RegisteredFunction, RegistryError, ReleasedFunctions,
        Reservation, Retry, Rhs, Semantics, SequenceView, Signature, Site, SourceLanguage,
        SourceMap, SourceMapId, SourceMapRefusal, SourceSite, Span, StartAdapter, StartConvention,
        StatementForm, StatementRuleViolation, Stmt, Survivor, TableView, TaskId, TaskLabel,
        TextView, Timestamp, TryStmt, Type, Unit, ValidatedFunctions, Value, ValueKind, VisitId,
        WaitId, WorkCounter, arena, check_declarations, check_statement, core_support,
        document_work, graph_tokens, heap, parse_definition, parse_definition_with_limits,
        parse_document, parse_document_with_limits, performed_by, performed_through, plan,
        print_definition, print_document, required_declarations, testkit, validate_definition,
        validate_definition_with_limits, validate_document, validate_document_with_limits,
    };
    /// Plain-language captions for a document's steps, its entries'
    /// parameters and the fields of their types, held beside it like its
    /// source map (`K-CAP-001`..`K-CAP-007`).
    pub use lash_kernel_doc::{
        CAPTION_DESCRIPTION_BYTES, CAPTION_TITLE_BYTES, Caption, CaptionEntry, CaptionField,
        CaptionKind, CaptionRefusal, CaptionTarget, Captions, CaptionsRefusal, FieldPath,
        FieldRoot, FieldTarget, ParamTarget, Rekeyed,
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
        NodeClass, Position, RelinkRefusal, Transaction, extract_entry,
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
/// [`derive`] reads a checked [`WorkflowDocument`](super::WorkflowDocument)
/// alone: the declarations it was linked against give library signatures,
/// so no engine, registry, native code or dialect is needed. Labels are
/// display data keyed by a callee's name:
/// [`helper_labels::labels`](crate::helper_labels::labels) reads them with no
/// dialect or helper release, and
/// [`helper_labels::title`](crate::helper_labels::title) falls back to the
/// bare name.
#[cfg(feature = "workflow-view")]
pub mod view {
    pub use lash_kernel_view::{
        ActionActivity, ActionObservation, ActionRole, ActionState, ActionView, ActivityRefusal,
        ArgumentView, AssignmentView, BindingKind, BindingView, CallableView, CalleeView,
        ComparisonView, ControlArm, ControlHeader, Dataflow, EntryParamView, EntryView,
        ExecutionSiteView, ExpressionView, FlowEdge, FlowKind, FlowTarget, HeaderRole, JSON_SCHEMA,
        LazyArm, LineRange, NodeCoverage, PathSite, PathStep, RanCounts, ReadableLens,
        ReadableRefusal, ReplayPath, ReturnView, RunObservation, ScopeCoverage, ScopeKind,
        ScopeView, SiteKind, StatementCoverage, StatementView, TYPESCRIPT_DECLARATION, TaskPath,
        Use, ValueRef, ValueSource, ViewError, WorkflowCoverage, WorkflowNode, WorkflowNodeKind,
        WorkflowView, coverage, derive, derive_callable, path,
    };
    /// A view's captions, joined by view location ([`WorkflowView::captions`]).
    pub use lash_kernel_view::{
        ArmCaption, BindingCaption, CallableCaption, CaptionView, CaptionViewRefusal, FieldCaption,
        NodeCaption, ParamCaption,
    };
}

/// A checked document entered at one entry, with its graph and the library
/// declarations it linked against: what a host reads, draws and admits.
pub use lash_kernel_check::{WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentError};
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
/// Which document an execution runs, as a process's observation snapshot
/// ([`ProcessDocumentIdentity`](crate::process::ProcessDocumentIdentity))
/// and an execution's start name it.
pub use lash_trace::{WorkflowDocumentEntry, WorkflowDocumentRef};
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{
    ExecutionDocument, ReplaySegments, SegmentAnswer, SegmentReplay, Unreplayable,
    WorkflowEnvironment,
};

#[cfg(feature = "codemode")]
use crate::persistence::ArtifactName;
#[cfg(feature = "codemode")]
use crate::process::{ProcessDefinition, ProcessDefinitionId, ProcessEngineKind};
#[cfg(feature = "codemode")]
use crate::{ProcessId, Result};

#[cfg(feature = "codemode")]
/// One definition as a host inspects it.
#[derive(Clone, Debug, PartialEq)]
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
/// A workflow published as a definition.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowPublication {
    /// The content-derived id and the signature derived by admission.
    pub definition: ProcessDefinition,
    /// The selected entry, independent of the opaque body.
    pub reference: WorkflowDocumentRef,
    /// Lash stores no definition-to-map association, so publication supplies this id.
    /// The id of the source map published with it, when one was: the host
    /// keeps it beside the definition id, and reads the map and its source
    /// back with
    /// [`HostArtifacts::read_source_map`](crate::process::HostArtifacts::read_source_map).
    pub source_map: Option<document::SourceMapId>,
}

#[cfg(feature = "codemode")]
/// The answer to publishing a workflow.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowPublish {
    Published(Box<WorkflowPublication>),
    /// The document was refused admission. Nothing was published.
    Refused(WorkflowAdmissionRefusal),
    /// The source map is not one of the document and source it was
    /// published with: the first check it failed. Nothing was published.
    SourceMapRefused(document::SourceMapRefusal),
    /// No engine of this core admits workflow documents.
    Unsupported {
        engine_kind: ProcessEngineKind,
    },
}

#[cfg(feature = "codemode")]
/// A workflow inspection, or why there is none.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowDocumentRead {
    /// The document entered where the reference says, held with the
    /// declarations it was stored with, and the document it was rewritten
    /// from with the rewrite's correspondence when a migration or a
    /// successor carry made it.
    Read(Box<ExecutionDocument>),
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
pub use lash_core::{
    ProcessWorkflowFoldError, ProcessWorkflowLiveCoverage, ProcessWorkflowObservation,
    ProcessWorkflowOverlay, ProcessWorkflowOverlayAccumulator, ProcessWorkflowRan,
    ProcessWorkflowSite, ProcessWorkflowSiteState, ProcessWorkflowWaitKind,
};
