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
//! document's `use` line gives it; the selected `lash::package::PackageView`
//! supplies display facts and its `title` joins one by identity, falling back to the bare
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
//! Give that reducer [`workflow_overlay_document`] the document read through
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
//! `lash::typescript::print(document, captions, library, options)` or
//! `lash::python::print` returns plain source, only the
//! code an agent could have written, an optional disposable map, and the
//! captions the dialect could not spell (`PrintedSource::dropped`). The
//! print is the workflow module an author writes: each declared function
//! and `main` prints as authored source when reading that region back
//! lowers to the identical region, and otherwise in kernel form, opened by
//! a `"use kernel"` directive (`lash::dialect::regions`). The
//! environment (effect manifest, number mode, entry signatures, private
//! bindings, helper identities and name renames) stays in the document and
//! is never printed. Printing does not compile or admit it. Hosts read the
//! source back, edited or not, against the document it was printed from:
//! `lash::typescript::read(source, &base, library, limits)` or
//! `lash::python::read`, and compare `Document::canonical_bytes()` to check
//! reconstruction. Each dialect's `print_kernel` and `read_kernel` are its
//! kernel-surface pair, every region in kernel form.
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
    pub use lash_kernel_doc::ActExpr;
    pub use lash_kernel_doc::ActKind;
    pub use lash_kernel_doc::Action;
    pub use lash_kernel_doc::Args;
    pub use lash_kernel_doc::AssignOrder;
    pub use lash_kernel_doc::Atom;
    pub use lash_kernel_doc::Block;
    pub use lash_kernel_doc::BodyCensus;
    pub use lash_kernel_doc::Bytes;
    pub use lash_kernel_doc::BytesView;
    pub use lash_kernel_doc::Callee;
    pub use lash_kernel_doc::Catch;
    pub use lash_kernel_doc::CheckedDeclarations;
    pub use lash_kernel_doc::Closure;
    pub use lash_kernel_doc::Correspondence;
    pub use lash_kernel_doc::DOCUMENT_GRAMMAR_VERSION;
    pub use lash_kernel_doc::Datum;
    pub use lash_kernel_doc::DeclarationRefusal as DocumentDeclarationRefusal;
    pub use lash_kernel_doc::DecodeError;
    pub use lash_kernel_doc::DecodeMismatch;
    pub use lash_kernel_doc::Decoder;
    pub use lash_kernel_doc::Document;
    pub use lash_kernel_doc::DocumentBytes;
    pub use lash_kernel_doc::DocumentBytesRefusal;
    pub use lash_kernel_doc::EncodeError;
    pub use lash_kernel_doc::Entry;
    pub use lash_kernel_doc::ErrorDatum;
    pub use lash_kernel_doc::Expr;
    pub use lash_kernel_doc::FIRST_NATIVE_VERSION;
    pub use lash_kernel_doc::FROZEN_DATUM;
    pub use lash_kernel_doc::FROZEN_JSON;
    pub use lash_kernel_doc::FROZEN_TOOL_VALUE;
    pub use lash_kernel_doc::FUNCTION_VALUE_ENTRY;
    pub use lash_kernel_doc::FUNCTION_VALUE_TAG;
    pub use lash_kernel_doc::Float;
    pub use lash_kernel_doc::Formula;
    pub use lash_kernel_doc::FrozenDatum;
    pub use lash_kernel_doc::Function;
    pub use lash_kernel_doc::FunctionBody;
    pub use lash_kernel_doc::FunctionCatalog;
    pub use lash_kernel_doc::FunctionDeclarations;
    pub use lash_kernel_doc::FunctionDefinition;
    pub use lash_kernel_doc::FunctionHeader;
    pub use lash_kernel_doc::FunctionRegistry;
    pub use lash_kernel_doc::FunctionRepresentation;
    pub use lash_kernel_doc::FunctionValue;
    pub use lash_kernel_doc::FunctionValueLimits;
    pub use lash_kernel_doc::FunctionValueRefusal;
    pub use lash_kernel_doc::Guard;
    pub use lash_kernel_doc::GuardExceeded;
    pub use lash_kernel_doc::Handle;
    pub use lash_kernel_doc::IdentifiedDocument;
    pub use lash_kernel_doc::Identity;
    pub use lash_kernel_doc::ImageError;
    pub use lash_kernel_doc::Implementation;
    pub use lash_kernel_doc::ImplementationKind;
    pub use lash_kernel_doc::Integer;
    pub use lash_kernel_doc::Invalid;
    pub use lash_kernel_doc::InvalidBytes;
    pub use lash_kernel_doc::InvalidFloat;
    pub use lash_kernel_doc::InvalidInteger;
    pub use lash_kernel_doc::InvalidNumberToken;
    pub use lash_kernel_doc::InvalidReason;
    pub use lash_kernel_doc::JoinMode;
    pub use lash_kernel_doc::KERNEL_VERSION;
    pub use lash_kernel_doc::KernelVersion;
    pub use lash_kernel_doc::Literal;
    pub use lash_kernel_doc::MAX_DECODE_DEPTH;
    pub use lash_kernel_doc::MAX_NESTING_DEPTH;
    pub use lash_kernel_doc::Manifest;
    pub use lash_kernel_doc::MapEntry;
    pub use lash_kernel_doc::Measure;
    pub use lash_kernel_doc::Member;
    pub use lash_kernel_doc::NativeCall;
    pub use lash_kernel_doc::NativeCtor;
    pub use lash_kernel_doc::NativeError;
    pub use lash_kernel_doc::NativeFunction;
    pub use lash_kernel_doc::NewObject;
    pub use lash_kernel_doc::Node;
    pub use lash_kernel_doc::NodeMut;
    pub use lash_kernel_doc::NumberPolicy;
    pub use lash_kernel_doc::NumberToken;
    pub use lash_kernel_doc::Nursery;
    pub use lash_kernel_doc::Operand;
    pub use lash_kernel_doc::Ordinals;
    pub use lash_kernel_doc::ParseError;
    pub use lash_kernel_doc::ParseErrorReason;
    pub use lash_kernel_doc::Place;
    pub use lash_kernel_doc::PreparedAnswer;
    pub use lash_kernel_doc::PreparedNative;
    pub use lash_kernel_doc::ProjectionRead;
    pub use lash_kernel_doc::Protocol;
    pub use lash_kernel_doc::RecordEntry;
    pub use lash_kernel_doc::RegisteredFunction;
    pub use lash_kernel_doc::RegistryError;
    pub use lash_kernel_doc::ReleasedFunctions;
    pub use lash_kernel_doc::Reservation;
    pub use lash_kernel_doc::Retry;
    pub use lash_kernel_doc::Rhs;
    pub use lash_kernel_doc::Semantics;
    pub use lash_kernel_doc::SequenceView;
    pub use lash_kernel_doc::SourceLanguage;
    pub use lash_kernel_doc::SourceMap;
    pub use lash_kernel_doc::SourceMapRefusal;
    pub use lash_kernel_doc::SourceSite;
    pub use lash_kernel_doc::Span;
    pub use lash_kernel_doc::StartAdapter;
    pub use lash_kernel_doc::StartConvention;
    pub use lash_kernel_doc::StatementForm;
    pub use lash_kernel_doc::StatementRuleViolation;
    pub use lash_kernel_doc::Stmt;
    pub use lash_kernel_doc::Survivor;
    pub use lash_kernel_doc::TableView;
    pub use lash_kernel_doc::TaskId;
    pub use lash_kernel_doc::TextView;
    pub use lash_kernel_doc::Timestamp;
    pub use lash_kernel_doc::TryStmt;
    pub use lash_kernel_doc::ValidatedFunctions;
    pub use lash_kernel_doc::Value;
    pub use lash_kernel_doc::ValueKind;
    pub use lash_kernel_doc::WorkCounter;
    pub use lash_kernel_doc::arena;
    pub use lash_kernel_doc::check_declarations;
    pub use lash_kernel_doc::check_statement;
    pub use lash_kernel_doc::core_support;
    pub use lash_kernel_doc::document_work;
    pub use lash_kernel_doc::heap;
    pub use lash_kernel_doc::parse_definition;
    pub use lash_kernel_doc::parse_definition_with_limits;
    pub use lash_kernel_doc::parse_document;
    pub use lash_kernel_doc::parse_document_with_limits;
    pub use lash_kernel_doc::performed_by;
    pub use lash_kernel_doc::performed_through;
    pub use lash_kernel_doc::plan;
    pub use lash_kernel_doc::print_definition;
    pub use lash_kernel_doc::print_document;
    pub use lash_kernel_doc::required_declarations;
    pub use lash_kernel_doc::scalar;
    pub use lash_kernel_doc::validate_definition;
    pub use lash_kernel_doc::validate_definition_with_limits;
    pub use lash_kernel_doc::validate_document;
    pub use lash_kernel_doc::validate_document_with_limits;
    /// Plain-language captions for a document's steps, its entries'
    /// parameters and the fields of their types, held beside it like its
    /// source map (`K-CAP-001`..`K-CAP-007`).
    pub use lash_kernel_doc::{
        CAPTION_DESCRIPTION_BYTES, CAPTION_TITLE_BYTES, Caption, CaptionEntry, CaptionField,
        CaptionKind, CaptionRefusal, CaptionTarget, Captions, CaptionsRefusal, FieldPath,
        FieldRoot, FieldTarget, ParamTarget, Rekeyed,
    };
    pub use lash_vocabulary::legacy_names::DocumentId;
    pub use lash_vocabulary::legacy_names::EffectName;
    pub use lash_vocabulary::legacy_names::FunctionId;
    pub use lash_vocabulary::legacy_names::FunctionName;
    pub use lash_vocabulary::legacy_names::InvalidIdentity;
    pub use lash_vocabulary::legacy_names::InvalidQualifiedName;
    pub use lash_vocabulary::legacy_names::Name;
    pub use lash_vocabulary::legacy_names::QualifiedName;
    pub use lash_vocabulary::legacy_names::SourceMapId;
    pub use lash_vocabulary::operations::ContractId;
    pub use lash_vocabulary::operations::OperationRequirement;
    pub use lash_vocabulary::sites::EffectId;
    pub use lash_vocabulary::sites::LoopCounts;
    pub use lash_vocabulary::sites::ObservationReader;
    pub use lash_vocabulary::sites::PositionCounts;
    pub use lash_vocabulary::sites::PositionLabel;
    pub use lash_vocabulary::sites::Site;
    pub use lash_vocabulary::sites::TaskLabel;
    pub use lash_vocabulary::sites::Unit;
    pub use lash_vocabulary::sites::VisitId;
    pub use lash_vocabulary::sites::WaitId;
    pub use lash_vocabulary::sites::testkit;
    pub use lash_vocabulary::tokens;
    pub use lash_vocabulary::types::MapType;
    pub use lash_vocabulary::types::Param;
    pub use lash_vocabulary::types::RecordType;
    pub use lash_vocabulary::types::RecordTypeField;
    pub use lash_vocabulary::types::Signature;
    pub use lash_vocabulary::types::Type;
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
/// with its selected [`PackageView`](crate::package::PackageView): the
/// document's declarations give library signatures, and the package view
/// supplies labels, comparisons and reply markers. No engine, native code
/// or dialect parser is needed. `PackageView::title` falls back to the bare
/// document name when that set has no label for the function.
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
    pub use lash_kernel_view::PackageMemberView;
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
    pub use lash_vocabulary::observation::RanCounts;
    pub use lash_vocabulary::observation::ReplayPath;
    pub use lash_vocabulary::observation::RunObservation;
    pub use lash_vocabulary::observation::TaskPath;
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
pub use lash_vm_runtime::trace_adapter::workflow_overlay_document;
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{
    ExecutionDocument, ReplaySegments, SegmentAnswer, SegmentReplay, Unreplayable,
    WorkflowEnvironment,
};

use crate::Result;
#[cfg(feature = "codemode")]
use crate::persistence::ArtifactName;
#[cfg(feature = "codemode")]
use crate::process::{ProcessDefinition, ProcessDefinitionId, ProcessEngineKind};
#[cfg(feature = "codemode")]
use lash_vocabulary::identity::ProcessId;

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
pub use lash_core::ProcessWorkflowFoldError;
pub use lash_core::ProcessWorkflowLiveCoverage;
pub use lash_core::ProcessWorkflowOverlay;
pub use lash_core::ProcessWorkflowOverlayAccumulator;
pub use lash_core::ProcessWorkflowRan;
pub use lash_core::ProcessWorkflowSite;
pub use lash_core::ProcessWorkflowSiteState;
pub use lash_core::ProcessWorkflowWaitKind;
pub use lash_vocabulary::observation::RunObservation;
